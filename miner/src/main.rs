#![feature(portable_simd)]

use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::simd::{LaneCount, Simd, SupportedLaneCount, u64x4};
use std::slice;
use std::time::{Duration, Instant};

use anyhow::Result;
use threadpool::ThreadPool;
use tracing::{error, info};

use neuron::{AccountId, NeuronInfoLite, config, Subtensor};
use neuron::auth::{KeypairSignature, VerificationMessage, signature_matches};

mod miner_config;

fn as_u8<T>(data: &[T]) -> &[u8] {
    unsafe { slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * size_of::<T>()) }
}

fn as_u8_mut<T>(data: &mut [T]) -> &mut [u8] {
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
    /// SAFETY: Safe if `data` is aligned, otherwise the behavior is undefined
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

    fn solve(&mut self, data: &mut [AlignedChunk], read_len: usize) {
        let len = data.len();
        let data_u8 = &mut as_u8_mut(data)[..read_len];

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

            self.solve(&mut data[1..], read_len % (8));
        } else if data_u8.len() < 8 * 4 {
            unsafe {
                self.solve_chunked::<2>(data_u8);
            }

            self.solve(&mut data[len % 2..], read_len % (8 * 2));
        } else {
            unsafe {
                self.solve_chunked::<4>(data_u8);
            }

            self.solve(&mut data[len % 4..], read_len % (8 * 4));
        }
    }
}

fn read<T>(stream: &mut TcpStream) -> T {
    let mut data = MaybeUninit::<T>::uninit();

    unsafe {
        stream.read(slice::from_raw_parts_mut(data.as_mut_ptr() as *mut u8, size_of::<AccountId>())).unwrap();

        data.assume_init()
    }
}

fn info_matches(message: &VerificationMessage, neurons: &[NeuronInfoLite], account_id: &AccountId, miner_uid: u16) -> bool {
    if message.netuid != *config::NETUID {
        return false;
    }

    if &message.miner.account_id != account_id {
        return false;
    }

    if message.miner.uid != miner_uid {
        return false;
    }

    if neurons[message.validator.uid as usize].hotkey != message.validator.account_id {
        return false;
    }

    return true
}

fn handle_connection(mut stream: TcpStream, validator_uid: u16) {
    let mut buffer = Vec::with_capacity(512);
    let mut solver = Solver::new();

    unsafe {
        buffer.set_len(buffer.capacity());
    }

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

        solver.solve(&mut buffer, len);

        match stream.write(&as_u8(&buffer)[..len]) {
            Ok(len) => {
                if len == 0 {
                    error!("Validator {validator_uid}'s connection does not appear to be writable");
                }
            }
            Err(error) => {
                error!("Failed to write to validator {validator_uid}, {error}");
                return;
            }
        }
    }
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
    async fn new() -> Self {
        let account_id: AccountId = todo!();

        let subtensor = Subtensor::new(&*config::CHAIN_ENDPOINT)
            .await
            .unwrap();

        let current_block = subtensor.get_block_number().await.unwrap();
        let last_block_fetch = Instant::now();
        let neurons = subtensor.get_neurons(*config::NETUID).await.unwrap();

        let neuron = neurons.iter().find(|&info| info.hotkey == account_id).expect("Not registered");

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

            let neuron = self.neurons.iter().find(|&info| info.hotkey == self.account_id).expect("Not registered");

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

                let message = read::<VerificationMessage>(&mut stream);

                if !info_matches(&message, &self.neurons, &self.account_id, self.uid) {
                    info!("{address} sent a signed message with incorrect information");
                    return;
                }

                let signature_matches = {
                    let signature = read::<KeypairSignature>(&mut stream);

                    signature_matches(&signature, &message)
                };

                if !signature_matches {
                    info!("{address} sent a signed message with an incorrect signature");
                    return;
                }

                pool.execute(move || handle_connection(stream, message.validator.uid));
            }
        }
    }
}

#[tokio::main]
async fn main() {
    Miner::new().await.run(*miner_config::PORT).await;
}
