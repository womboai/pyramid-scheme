#![feature(portable_simd)]

use crate::signature_checking::info_matches;
use anyhow::Result;
use neuron::auth::VerificationMessage;
use neuron::updater::Updater;
use neuron::{config, load_env, setup_logging, should_restart, subtensor, ProcessingNetworkRequest, SPEC_VERSION};
use rusttensor::api::apis;
use rusttensor::rpc::{call_runtime_api_decoded, RuntimeApiError};
use rusttensor::rpc::types::NeuronInfoLite;
use rusttensor::sign::{verify_signature, KeypairSignature};
use rusttensor::subtensor::Subtensor;
use rusttensor::wallet::{hotkey_location, load_key_account_id};
use rusttensor::{AccountId, Block, BlockNumber};
use std::cmp::min;
use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::simd::Simd;
use std::time::{Duration, Instant};
use std::{env, io, slice};
use std::process::exit;
use std::sync::LazyLock;
use threadpool::ThreadPool;
use tracing::{debug, error, info, warn};

mod signature_checking;

fn as_u8<T>(data: &[T]) -> &[u8] {
    // SAFETY: Every &_ is representable as &[u8], lifetimes match
    unsafe { slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * size_of::<T>()) }
}

fn as_u8_mut<T>(data: &mut [T]) -> &mut [u8] {
    // SAFETY: Every &mut _ is representable as &mut [u8], lifetimes match
    unsafe { slice::from_raw_parts_mut(data.as_mut_ptr() as *mut u8, data.len() * size_of::<T>()) }
}

static STAKE_THRESHOLD: LazyLock<u64> = LazyLock::new(|| {
    (env::var("STAKE_THRESHOLD")
        .as_ref()
        .map(|var| var.parse().unwrap())
        .unwrap_or(4000.0) * 1_000_000_000f64
    ) as u64
});

// Ensure that we're always aligned for SIMD access
type Word = u64;
type AlignedChunk = Simd<Word, 8>;

#[derive(Default)]
struct Solver {
    last: Word,
}

impl Solver {
    fn new(last_byte: u8) -> Self {
        Solver {
            last: Word::from_le_bytes([0, 0, 0, 0, 0, 0, 0, last_byte]),
        }
    }

    fn solve(&mut self, data: &mut [AlignedChunk]) {
        for i in 0..data.len() {
            let mut modified_chunk = AlignedChunk::splat(0);

            for j in 0..AlignedChunk::LEN {
                let x = data[i][j];

                modified_chunk[j] =
                    x << 1 | x << 2 | self.last >> (Word::BITS - 1) | self.last >> (Word::BITS - 2);

                self.last = x;
            }

            data[i] ^= modified_chunk;
        }
    }
}

fn read<T>(stream: &mut TcpStream) -> io::Result<T> {
    let mut data = MaybeUninit::<T>::uninit();

    unsafe {
        stream.read(slice::from_raw_parts_mut(
            data.as_mut_ptr() as *mut u8,
            size_of::<T>(),
        ))?;

        Ok(data.assume_init())
    }
}

fn handle_step_request(
    stream: &mut TcpStream,
    buffer: &mut [AlignedChunk],
    total_solved: &mut u128,
    validator_uid: u16,
) -> bool {
    let request = match read::<ProcessingNetworkRequest>(stream) {
        Ok(request) => request,
        Err(e) => {
            error!("Failed to read request from validator {validator_uid}, {e}");
            return false;
        }
    };

    debug!(
        "Solving {size} byte chunk for validator {validator_uid}",
        size = request.length,
    );

    let mut solved = 0;
    let mut solver = Solver::new(request.last_byte);

    while solved != request.length {
        let read_len = min(
            (request.length - solved) as usize,
            buffer.len() * size_of::<AlignedChunk>(),
        );

        let len = match stream.read(&mut as_u8_mut(buffer)[..read_len]) {
            Ok(len) => len,
            Err(error) => {
                error!("Failed to read from validator {validator_uid}, {error}");
                return false;
            }
        };

        if len == 0 {
            error!("Validator {validator_uid} unexpectedly stopped sending data");

            return false;
        }

        let chunk_len = len.div_ceil(size_of::<AlignedChunk>());
        let last_word_bytes = len % size_of::<AlignedChunk>();

        let extra_bytes = if last_word_bytes == 0 {
            0
        } else {
            size_of::<AlignedChunk>() - last_word_bytes
        };

        let word_index = AlignedChunk::LEN - (extra_bytes / size_of::<Word>()) - 1;
        let last = buffer[chunk_len - 1][word_index];

        solver.solve(&mut buffer[..chunk_len]);

        solver.last = last;

        let mut written = 0;

        while written < len {
            match stream.write(&as_u8(&buffer)[written..len]) {
                Ok(len) => {
                    if len == 0 {
                        error!(
                            "Validator {validator_uid}'s connection does not appear to be writable",
                        );
                    } else {
                        written += len;
                    }
                }
                Err(error) => {
                    error!("Failed to write to validator {validator_uid}, {error}");
                    return false;
                }
            }
        }

        debug!("Solved {written} bytes for validator {validator_uid}");

        solved += written as u64;
        *total_solved += written as u128;
    }

    true
}

fn handle_connection(mut stream: TcpStream, validator_uid: u16) {
    let mut buffer = Vec::with_capacity(512);

    unsafe {
        buffer.set_len(buffer.capacity());
    }

    let mut total_solved = 0;

    loop {
        if !handle_step_request(&mut stream, &mut buffer, &mut total_solved, validator_uid) {
            break;
        }
    }

    info!("Disconnected from validator {validator_uid}, solved {total_solved} total bytes");
}

struct Miner {
    account_id: AccountId,
    subtensor: Subtensor,
    current_block: Block,
    last_block_fetch: Instant,
    neurons: Vec<NeuronInfoLite>,
    last_metagraph_sync: BlockNumber,
    uid: u16,
}

impl Miner {
    async fn new(account_id: AccountId) -> Self {
        let subtensor = subtensor().await.unwrap();

        let current_block = subtensor.blocks().at_latest().await.unwrap();
        let last_block_fetch = Instant::now();
        let runtime_api = subtensor.runtime_api().at(current_block.reference());
        let neurons = call_runtime_api_decoded(
            &runtime_api,
            apis()
                .neuron_info_runtime_api()
                .get_neurons_lite(*config::NETUID),
        )
            .await
            .unwrap();

        let neuron = neurons
            .iter()
            .find(|&info| info.hotkey == account_id)
            .expect("Not registered");

        let uid = neuron.uid.0;

        Self {
            account_id,
            subtensor,
            last_metagraph_sync: current_block.number(),
            current_block,
            last_block_fetch,
            neurons,
            uid,
        }
    }

    async fn sync(&mut self, now: Instant) -> Result<(), RuntimeApiError> {
        self.current_block = self.subtensor.blocks().at_latest().await?;
        self.last_block_fetch = now;

        if self.current_block.number() - self.last_metagraph_sync >= *config::EPOCH_LENGTH {
            let runtime_api = self
                .subtensor
                .runtime_api()
                .at(self.current_block.reference());

            self.neurons = call_runtime_api_decoded(
                &runtime_api,
                apis()
                    .neuron_info_runtime_api()
                    .get_neurons_lite(*config::NETUID),
            )
                .await?;

            let neuron = self
                .neurons
                .iter()
                .find(|&info| info.hotkey == self.account_id)
                .expect("Not registered");

            self.last_metagraph_sync = self.current_block.number();
            self.uid = neuron.uid.0;
        }

        Ok(())
    }

    async fn run(&mut self, port: u16) {
        let ip: Ipv4Addr = [0u8, 0, 0, 0].into();
        let listener = TcpListener::bind((ip, port)).unwrap();
        let pool = ThreadPool::new(32);

        listener.set_nonblocking(true).unwrap();

        info!("Awaiting connections");

        loop {
            let now = Instant::now();

            if now - self.last_block_fetch >= Duration::from_secs(12) {
                if let Err(e) = self.sync(now).await {
                    if let RuntimeApiError::CallError(error) = &e {
                        if should_restart(error) {
                            error!("Irrecoverable RPC error, restarting");

                            exit(1);
                        }
                    }

                    error!("Failed to sync metagraph: {e}");

                    tokio::time::sleep(Duration::from_secs(12)).await;

                    continue;
                }
            }

            if let Ok((mut stream, address)) = listener.accept() {
                info!("IP {address} has connected");

                stream.set_nonblocking(false).unwrap();

                let message = match read::<VerificationMessage>(&mut stream) {
                    Ok(message) => message,
                    Err(error) => {
                        info!("Failed to read signed verification message from {address}, {error}");
                        continue;
                    }
                };

                if let Err(e) = info_matches(&message, &self.neurons, &self.account_id, self.uid) {
                    info!("{address} sent a signed verification message with incorrect information, {e}");
                    continue;
                }

                let signature_matches = {
                    let signature = match read::<KeypairSignature>(&mut stream) {
                        Ok(signature) => signature,
                        Err(error) => {
                            info!("Failed to read signature from {address}, {error}");
                            continue;
                        }
                    };

                    verify_signature(&message.validator.account_id, &signature, &message)
                };

                if !signature_matches {
                    info!("{address} sent a signed message with an incorrect signature");
                    continue;
                }

                {
                    let neuron = &self.neurons[message.validator.uid as usize];

                    if !neuron.validator_permit {
                        info!("IP {address} with UID {uid} does not have validator permit, disconnecting", uid = message.validator.uid);
                        continue;
                    }

                    if neuron.stake.iter().map(|(_, stake)| stake.0).sum::<u64>() < *STAKE_THRESHOLD {
                        info!("IP {address} with UID {uid} does not have enough stake, disconnecting", uid = message.validator.uid);
                        continue;
                    }
                }

                info!("IP {address} is confirmed to be Validator UID {uid}", uid = message.validator.uid);

                if let Err(e) = stream.write(&SPEC_VERSION.to_le_bytes()) {
                    warn!(
                        "Failed to send version to validator {}, {}",
                        message.validator.uid, e
                    );
                }

                pool.execute(move || handle_connection(stream, message.validator.uid));
            }
        }
    }
}

#[tokio::main]
async fn main() {
    load_env();

    if *config::AUTO_UPDATE {
        let updater = Updater::new(Duration::from_secs(3600));
        updater.spawn();
    }

    let hotkey_location = hotkey_location(
        config::WALLET_PATH.clone(),
        &*config::WALLET_NAME,
        &*config::HOTKEY_NAME,
    );

    let account_id = load_key_account_id(&hotkey_location).expect(&format!(
        "Error loading hotkey! Please verify that it exists! Looking in: '{:?}'",
        hotkey_location
    ));

    setup_logging(&account_id, false, "miner");

    let mut miner = Miner::new(account_id).await;

    miner.run(*config::PORT).await;
}

#[cfg(test)]
mod test {
    use crate::{as_u8, AlignedChunk, Solver};
    use num_bigint::{BigInt, Sign};

    const STEPS: u64 = u16::MAX as u64;

    #[test]
    fn ensure_accurate_solver() {
        let mut solver = Solver::default();
        let bits = (STEPS * 2 + 1) as usize;
        let mut expected = BigInt::new(Sign::Plus, vec![1]);
        let vec_size = bits.div_ceil(8).div_ceil(size_of::<AlignedChunk>());

        let mut result = Vec::with_capacity(vec_size);
        result.resize_with(vec_size, || AlignedChunk::splat(0));
        result[0][0] = 1;

        for i in 0..STEPS - 1 {
            let byte_count = (i * 2 + 3).div_ceil(8);

            solver.solve(&mut result);

            expected ^= expected.clone() << 1 | expected.clone() << 2;

            assert_eq!(
                &expected.to_bytes_le().1,
                &as_u8(&result)[..byte_count as usize]
            );
        }
    }
}
