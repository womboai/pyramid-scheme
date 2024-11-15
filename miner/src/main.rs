#![feature(portable_simd)]

use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::simd::{u64x4, LaneCount, Simd, SupportedLaneCount};
use std::time::{Duration, Instant};
use std::{io, slice};

use crate::signature_checking::info_matches;
use anyhow::Result;
use dotenv::dotenv;
use neuron::auth::{signature_matches, KeypairSignature, VerificationMessage};
use neuron::{
    config, hotkey_location, load_key_account_id, setup_opentelemetry, AccountId, NeuronInfoLite,
    Subtensor,
};
use threadpool::ThreadPool;
use tracing::{error, info};
use crate::updater::Updater;

mod signature_checking;
mod updater;

fn as_u8<T>(data: &[T]) -> &[u8] {
    // SAFETY: Every &_ is representable as &[u8], lifetimes match
    unsafe { slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * size_of::<T>()) }
}

fn as_u8_mut<T>(data: &mut [T]) -> &mut [u8] {
    // SAFETY: Every &mut _ is representable as &mut [u8], lifetimes match
    unsafe { slice::from_raw_parts_mut(data.as_mut_ptr() as *mut u8, data.len() * size_of::<T>()) }
}

// Ensure that we're always aligned for SIMD access
#[repr(transparent)]
struct AlignedChunk(u64x4);

#[derive(Default)]
struct Solver {
    last: u64,
}

impl Solver {
    fn new() -> Self {
        Solver::default()
    }

    /// Solve a chunk of memory aligned to `Simd<u64, N>` in `size_of::<Simd<u64, N>>` chunks
    /// SAFETY: Safe if `data` is aligned, otherwise the behavior is undefined due to unaligned access
    unsafe fn solve_chunked<const N: usize>(&mut self, data: &mut [u8])
    where
        LaneCount<N>: SupportedLaneCount,
    {
        let data = slice::from_raw_parts_mut(
            data.as_mut_ptr() as *mut Simd<u64, N>,
            data.len() / size_of::<Simd<u64, N>>(),
        );

        for i in 0..data.len() {
            let mut modified_chunk = Simd::<u64, N>::splat(0);

            for j in 0..N {
                let x = data[i][j];

                modified_chunk[j] = x << 1 | x << 2 | self.last >> 63 | self.last >> 62;

                self.last = x;
            }

            data[i] ^= modified_chunk
        }
    }

    fn solve(&mut self, data: &mut [AlignedChunk], offset: usize, read_len: usize) {
        if read_len == 0 {
            return;
        }

        let data_u8 = &mut as_u8_mut(data)[offset..offset + read_len];

        if data_u8.len() <= 8 {
            let mut x = 0u64;

            for i in 0..data_u8.len() {
                x |= (data_u8[i] as u64) << (8 * i)
            }

            data_u8.copy_from_slice(
                &(x ^ (x << 1 | x << 2 | self.last >> 63 | self.last >> 62))
                    .to_le_bytes()
                    .as_slice()[0..data_u8.len()],
            );

            self.last = x;
        } else if data_u8.len() < 8 * 2 {
            unsafe {
                self.solve_chunked::<1>(data_u8);
            }

            self.solve(data, offset + read_len - read_len % 8, read_len % 8);
        } else if data_u8.len() < 8 * 4 {
            unsafe {
                self.solve_chunked::<2>(data_u8);
            }

            self.solve(
                data,
                offset + read_len - read_len % (8 * 2),
                read_len % (8 * 2),
            );
        } else {
            unsafe {
                self.solve_chunked::<4>(data_u8);
            }

            self.solve(
                data,
                offset + read_len - read_len % (8 * 4),
                read_len % (8 * 4),
            );
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

fn handle_connection(mut stream: TcpStream, validator_uid: u16) {
    let mut buffer = Vec::with_capacity(512);
    let mut solver = Solver::new();

    unsafe {
        buffer.set_len(buffer.capacity());
    }

    let mut total_solved = 0;

    loop {
        let len = match stream.read(as_u8_mut(&mut buffer)) {
            Ok(len) => len,
            Err(error) => {
                error!("Failed to read from validator {validator_uid}, {error}");
                return;
            }
        };

        if len == 0 {
            break;
        }

        solver.solve(&mut buffer, 0, len);
        solver.last = 0;

        let mut written = 0;

        while written < len {
            match stream.write(&as_u8(&buffer)[written..len]) {
                Ok(len) => {
                    if len == 0 {
                        error!(
                            "Validator {validator_uid}'s connection does not appear to be writable"
                        );
                    } else {
                        written += len;
                    }
                }
                Err(error) => {
                    error!("Failed to write to validator {validator_uid}, {error}");
                    return;
                }
            }
        }

        total_solved += written;
    }

    info!("Solved {total_solved} bytes for validator {validator_uid}")
}

struct Miner {
    account_id: AccountId,
    subtensor: Subtensor,
    current_block: u64,
    last_block_fetch: Instant,
    neurons: Vec<NeuronInfoLite>,
    last_metagraph_sync: u64,
    uid: u16,
}

impl Miner {
    async fn new(account_id: AccountId) -> Self {
        let subtensor = Subtensor::new(&*config::CHAIN_ENDPOINT).await.unwrap();

        let current_block = subtensor.get_block_number().await.unwrap();
        let last_block_fetch = Instant::now();
        let neurons = subtensor.get_neurons(*config::NETUID).await.unwrap();

        let neuron = neurons
            .iter()
            .find(|&info| info.hotkey == account_id)
            .expect("Not registered");

        let uid = neuron.uid.0;

        Self {
            account_id,
            subtensor,
            current_block,
            last_block_fetch,
            neurons,
            last_metagraph_sync: current_block,
            uid,
        }
    }

    async fn sync(&mut self, now: Instant) -> Result<()> {
        self.current_block = self.subtensor.get_block_number().await?;
        self.last_block_fetch = now;

        if self.current_block - self.last_metagraph_sync >= *config::EPOCH_LENGTH {
            self.neurons = self.subtensor.get_neurons(*config::NETUID).await?;

            let neuron = self
                .neurons
                .iter()
                .find(|&info| info.hotkey == self.account_id)
                .expect("Not registered");

            self.last_metagraph_sync = self.current_block;
            self.uid = neuron.uid.0;
        }

        Ok(())
    }

    async fn run(&mut self, port: u16) {
        let ip: Ipv4Addr = [0u8, 0, 0, 0].into();
        let listener = TcpListener::bind((ip, port)).unwrap();
        let pool = ThreadPool::new(32);

        listener.set_nonblocking(true).unwrap();

        loop {
            let now = Instant::now();

            if now - self.last_block_fetch >= Duration::from_secs(12) {
                if let Err(e) = self.sync(now).await {
                    error!("Failed to sync metagraph: {e}");
                }
            }

            if let Ok((mut stream, address)) = listener.accept() {
                info!("Validator {address} has connected");

                let message = match read::<VerificationMessage>(&mut stream) {
                    Ok(message) => message,
                    Err(error) => {
                        info!("Failed to read signed message from {address}, {error}");
                        continue;
                    }
                };

                if let Err(e) = info_matches(&message, &self.neurons, &self.account_id, self.uid) {
                    info!("{address} sent a signed message with incorrect information, {e}");
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

                    signature_matches(&signature, &message)
                };

                if !signature_matches {
                    info!("{address} sent a signed message with an incorrect signature");
                    continue;
                }

                pool.execute(move || handle_connection(stream, message.validator.uid));
            }
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = dotenv() {
        println!("Could not load .env: {e}");
    }

    let updater = Updater::new(
        Duration::from_secs(3600),
        "main".to_string(),
    );
    updater.spawn();

    let hotkey_location = hotkey_location(
        config::WALLET_PATH.clone(),
        &*config::WALLET_NAME,
        &*config::HOTKEY_NAME,
    );

    let account_id = load_key_account_id(&hotkey_location).unwrap();

    setup_opentelemetry(&account_id, "miner");

    let mut miner = Miner::new(account_id).await;

    miner.run(*config::PORT).await;
}

#[cfg(test)]
mod test {
    use crate::{as_u8, AlignedChunk, Solver};
    use num_bigint::{BigInt, Sign};
    use std::simd::u64x4;

    const STEPS: u64 = u16::MAX as u64;

    #[test]
    fn ensure_accurate_solver() {
        let mut solver = Solver::new();
        let bits = (STEPS * 2 + 1) as usize;
        let mut expected = BigInt::new(Sign::Plus, vec![1]);
        let vec_size = bits.div_ceil(8).div_ceil(size_of::<AlignedChunk>());

        let mut result = Vec::with_capacity(vec_size);
        result.resize_with(vec_size, || AlignedChunk(u64x4::splat(0)));
        result[0] = AlignedChunk(u64x4::from_array([1, 0, 0, 0]));

        for i in 0..STEPS - 1 {
            let byte_count = (i * 2 + 3).div_ceil(8);

            solver.solve(&mut result, 0, byte_count as usize);

            expected ^= expected.clone() << 1 | expected.clone() << 2;

            assert_eq!(
                &expected.to_bytes_le().1,
                &as_u8(&result)[..byte_count as usize]
            );
        }
    }
}
