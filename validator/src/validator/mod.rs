use std::cell::UnsafeCell;
use std::cmp::min;
use std::fs;
use std::io::{Read, Write};
use std::mem::transmute;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::random::random;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use threadpool::ThreadPool;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::validator::memory_storage::{MemoryMapped, MemoryMappedFile, MemoryMappedStorage};
use neuron::auth::{sign_message, KeyRegistrationInfo, VerificationMessage};
use neuron::{
    config, hotkey_location, load_key_seed, signer_from_seed, AccountId, NeuronInfoLite, Signer,
    Subtensor,
};

mod memory_storage;

const VERSION_KEY: u64 = 1;
const GROW_BY: u64 = 1024 * 1024 * 8;
const VALIDATION_CHANCE: f32 = 0.05;
const VALIDATION_BYTES: u64 = 256;

struct CurrentRow(Arc<UnsafeCell<MemoryMappedStorage>>);

impl CurrentRow {
    unsafe fn share(&self) -> Self {
        Self(self.0.clone())
    }
}

impl MemoryMapped for CurrentRow {
    fn open(path: impl AsRef<Path>, initial_capacity: u64) -> std::io::Result<Self> {
        Ok(Self(Arc::new(UnsafeCell::new(MemoryMappedStorage::open(
            path,
            initial_capacity,
        )?))))
    }

    fn flush(&self) -> std::io::Result<()> {
        self.deref().flush()
    }

    fn ensure_capacity(&mut self, capacity: u64) -> std::io::Result<()> {
        self.deref_mut().ensure_capacity(capacity)
    }
}

unsafe impl Send for CurrentRow {}
unsafe impl Sync for CurrentRow {}

impl Deref for CurrentRow {
    type Target = MemoryMappedStorage;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}

impl DerefMut for CurrentRow {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct KeyScoreInfo {
    score: u128,
    cheater: bool,
    hotkey: AccountId,
}

#[derive(Debug, Serialize, Deserialize)]
struct ValidatorState {
    step: u64,
    key_info: Vec<KeyScoreInfo>,
}

impl ValidatorState {
    fn for_hotkeys(hotkeys: impl Iterator<Item = AccountId>) -> Self {
        let key_info = hotkeys
            .map(|hotkey| KeyScoreInfo {
                hotkey,
                score: 0,
                cheater: false,
            })
            .collect();

        Self { step: 1, key_info }
    }
}

impl Default for ValidatorState {
    fn default() -> Self {
        Self {
            step: 1,
            key_info: Vec::new(),
        }
    }
}

struct NeuronData {
    score: UnsafeCell<u128>,
    cheater: UnsafeCell<bool>,
    connection: UnsafeCell<Option<TcpStream>>,
    info: NeuronInfoLite,
}

trait UnsafeCellImmutableBorrow<T> {
    fn as_ref(&self) -> &T;
}

trait UnsafeCellImmutableCopy<T> {
    fn inner(&self) -> T;
}

impl<T> UnsafeCellImmutableBorrow<T> for UnsafeCell<T> {
    fn as_ref(&self) -> &T {
        unsafe { &*self.get() }
    }
}

impl<T> UnsafeCellImmutableCopy<T> for UnsafeCell<T> where T : Copy {
    fn inner(&self) -> T {
        *self.as_ref()
    }
}

struct SendPtr<T>(*mut T);

unsafe impl<T> Send for SendPtr<T> {}

impl<T> From<*mut T> for SendPtr<T> {
    fn from(value: *mut T) -> Self {
        SendPtr(value)
    }
}

impl<T> Deref for SendPtr<T> {
    type Target = *mut T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct Validator {
    signer: Signer,
    subtensor: Subtensor,
    neurons: Vec<NeuronData>,
    uid: u16,

    current_row: CurrentRow,
    center_column: MemoryMappedFile,
    step: u64,

    last_metagraph_sync: u64,

    thread_pool: ThreadPool,
}

impl Validator {
    fn find_neuron_info<'a>(
        neurons: &'a [NeuronInfoLite],
        account_id: &AccountId,
    ) -> Option<&'a NeuronInfoLite> {
        neurons.iter().find(|neuron| &neuron.hotkey == account_id)
    }

    fn not_registered(account_id: &AccountId) -> ! {
        panic!(
            "Hotkey {account_id} is not registered in sn{}",
            *config::NETUID
        );
    }

    fn step_to_grow_to(step: u64) -> u64 {
        let step = step + 1;
        let remainder = step % GROW_BY;

        if remainder == 0 {
            step
        } else {
            step - remainder + GROW_BY
        }
    }

    fn current_row_file_size(step: u64) -> u64 {
        (Self::step_to_grow_to(step) * 2 + 1).div_ceil(8)
    }

    fn center_column_file_size(step: u64) -> u64 {
        Self::step_to_grow_to(step).div_ceil(8)
    }

    pub async fn new() -> Self {
        let hotkey_location = hotkey_location(
            config::WALLET_PATH.clone(),
            &*config::WALLET_NAME,
            &*config::HOTKEY_NAME,
        );

        let seed = load_key_seed(&hotkey_location).unwrap();

        let signer = signer_from_seed(&seed).unwrap();

        let subtensor = Subtensor::new(&*config::CHAIN_ENDPOINT).await.unwrap();

        let neurons: Vec<NeuronInfoLite> = subtensor.get_neurons(*config::NETUID).await.unwrap();

        let last_metagraph_sync = subtensor.get_block_number().await.unwrap();
        let neuron_info = Self::find_neuron_info(&neurons, signer.account_id());

        let uid = if let Some(neuron_info) = neuron_info {
            neuron_info.uid.0
        } else {
            Self::not_registered(signer.account_id());
        };

        let state = Self::load_state(neurons.iter().map(|neuron| neuron.hotkey.clone())).unwrap();

        fs::create_dir_all("state").unwrap();

        let mut current_row = CurrentRow::open(
            "state/current_row.bin",
            Self::current_row_file_size(state.step),
        )
        .unwrap();

        let mut center_column = MemoryMappedFile::open(
            "state/center_column.bin",
            Self::center_column_file_size(state.step),
        )
        .unwrap();

        if state.step == 1 {
            // Initial state
            current_row[0] = 1;
            center_column[0] = 1;
        }

        let neurons = neurons
            .into_iter()
            .map(|info| {
                let state_info = &state.key_info[info.uid.0 as usize];

                if state_info.hotkey == info.hotkey {
                    NeuronData {
                        score: state_info.score.into(),
                        cheater: state_info.cheater.into(),
                        connection: Self::connect_to_miner(&signer, uid, &info, state_info.cheater).into(),
                        info,
                    }
                } else {
                    NeuronData {
                        score: 0.into(),
                        cheater: false.into(),
                        connection: Self::connect_to_miner(&signer, uid, &info, false).into(),
                        info,
                    }
                }
            })
            .collect();

        Self {
            signer,
            subtensor,
            neurons,
            uid,
            current_row,
            center_column,
            step: state.step,
            last_metagraph_sync,
            thread_pool: ThreadPool::new(256),
        }
    }

    fn state_path() -> PathBuf {
        PathBuf::from("state/data.json")
    }

    fn save_state(&self) -> Result<()> {
        let path = Self::state_path();

        self.center_column.flush()?;
        self.current_row.flush()?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let state = ValidatorState {
            key_info: self
                .neurons
                .iter()
                .map(|data| KeyScoreInfo {
                    hotkey: data.info.hotkey.clone(),
                    cheater: data.cheater.inner(),
                    score: data.score.inner(),
                })
                .collect(),
            step: self.step,
        };

        let json = serde_json::to_string(&state)?;

        fs::write(&path, json)?;

        Ok(())
    }

    fn load_state(hotkeys: impl Iterator<Item = AccountId>) -> Result<ValidatorState> {
        let path = Self::state_path();

        if !path.exists() {
            return Ok(ValidatorState::for_hotkeys(hotkeys));
        }

        let json = fs::read_to_string(&path)?;

        Ok(serde_json::from_str(&json)?)
    }

    async fn set_weights(&self) -> Result<()> {
        if self.neurons.is_empty() {
            return Ok(());
        }

        let max_score = self
            .neurons
            .iter()
            .map(|info| if info.cheater.inner() { 0 } else { info.score.inner() })
            .max()
            .unwrap();

        let scores = self.neurons.iter().map(|neuron| {
            let normalized_score = if neuron.cheater.inner() {
                0
            } else if max_score == 0 {
                u16::MAX
            } else {
                ((neuron.score.inner() * u16::MAX as u128) / max_score) as u16
            };

            (neuron.info.uid.0, normalized_score)
        });

        self.subtensor
            .set_weights(&self.signer, *config::NETUID, scores, VERSION_KEY)
            .await?;

        Ok(())
    }

    async fn sync(&mut self, block: Option<u64>) -> Result<()> {
        let neurons = self.subtensor.get_neurons(*config::NETUID).await?;

        let block = if let Some(block) = block {
            block
        } else {
            self.subtensor.get_block_number().await?
        };

        self.last_metagraph_sync = block;

        let neuron_info = Self::find_neuron_info(&neurons, self.signer.account_id());

        let neuron_info = if let Some(neuron_info) = neuron_info {
            neuron_info
        } else {
            Self::not_registered(self.signer.account_id());
        };

        self.uid = neuron_info.uid.0;

        // Update changed hotkeys
        for i in 0..self.neurons.len() {
            if self.neurons[i].info.hotkey != neurons[i].hotkey {
                let info = neurons[i].clone();

                self.neurons[i] = NeuronData {
                    score: 0.into(),
                    cheater: false.into(),
                    connection: Self::connect_to_miner(&self.signer, self.uid, &info, false).into(),
                    info,
                };
            }
        }

        // Update scores array size if needed
        if self.neurons.len() != neurons.len() {
            let mut uid_iterator = (self.neurons.len()..neurons.len()).into_iter();

            self.neurons.resize_with(neurons.len(), || {
                let info = neurons[uid_iterator.next().unwrap()].clone();

                NeuronData {
                    score: 0.into(),
                    cheater: false.into(),
                    connection: Self::connect_to_miner(&self.signer, self.uid, &info, false).into(),
                    info,
                }
            });
        }

        // Set weights if enough time has passed
        if block - neuron_info.last_update.0 >= *config::EPOCH_LENGTH {
            self.set_weights().await?;
        }

        Ok(())
    }

    fn verify_result(original: &[u8], result: &[u8]) -> bool {
        let mut last = 0;

        for (i, x) in original.iter().enumerate() {
            let expected = x ^ (x << 1 | x << 2 | last >> 7 | last >> 6);

            if result[i] != expected {
                return false;
            }

            last = *x;
        }

        true
    }

    fn handle_connection(
        mut current_row: CurrentRow,
        connection_ref: &mut Option<TcpStream>,
        score: &mut u128,
        cheater: &mut bool,
        start: u64,
        end: u64,
    ) {
        let connection = connection_ref.as_mut().unwrap();

        let buffer_size = min(end - start, 8 * 4 * 256);

        let iterations = (end - start).div_ceil(buffer_size);

        let chance =
            VALIDATION_CHANCE * iterations as f32 - VALIDATION_CHANCE.powi(iterations as i32);

        let validation_start_index = if random::<u16>() as f32 / u16::MAX as f32 <= chance {
            let t = random::<u16>() as f32 / u16::MAX as f32;
            let validation_start_index = ((end - start) as f32 * t) as u64;

            Some(validation_start_index)
        } else {
            None
        };

        let mut added = 0;

        for i in 0..iterations {
            let from = (start + i * buffer_size) as usize;
            let to = (start + (i + 1) * buffer_size) as usize;

            while added != to - from {
                let written = match connection.write(&current_row[from + added..to]) {
                    Ok(len) => {
                        if len == 0 {
                            warn!(
                                "Failed to write to miner connection while there's more to process"
                            );

                            return;
                        }

                        len
                    }
                    Err(error) => {
                        error!("Error occurred writing to miner: {error}");

                        return;
                    }
                };

                let mut read = 0;

                while read < written {
                    let range = from + read + added..from + added + written + read;

                    let validation_range = if let Some(validation_start_index) =
                        validation_start_index
                    {
                        if range.contains(&(validation_start_index as usize)) {
                            Some(validation_start_index..validation_start_index + VALIDATION_BYTES)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let len = match connection.read(&mut current_row[range]) {
                        Ok(len) => {
                            if len == 0 {
                                warn!("Failed to read from miner connection");

                                return;
                            }

                            len
                        }
                        Err(error) => {
                            error!("Error occurred reading from miner: {error}");

                            return;
                        }
                    };

                    if let Some(validation_range) = validation_range {
                        let mapped = current_row.map_original(validation_range.clone());

                        if !Self::verify_result(
                            &mapped,
                            &current_row
                                [validation_range.start as usize..validation_range.end as usize],
                        ) {
                            *cheater = true;
                            *connection_ref = None;

                            let mapped = current_row.map_original(start..end);

                            (&mut current_row[start as usize..end as usize])
                                .copy_from_slice(&mapped);

                            return;
                        }
                    }

                    read += len;
                }

                added += written;
            }
        }

        *score += added as u128;
    }

    fn connect_to_miner(signer: &Signer, uid: u16, neuron: &NeuronInfoLite, cheater: bool) -> Option<TcpStream> {
        if cheater {
            return None;
        }

        let ip: IpAddr = if neuron.axon_info.ip_type == 4 {
            Ipv4Addr::from(neuron.axon_info.ip as u32).into()
        } else {
            Ipv6Addr::from(neuron.axon_info.ip).into()
        };

        let address = SocketAddr::new(ip, neuron.axon_info.port);

        info!("Attempting to connect to {address}");

        match TcpStream::connect(address) {
            Ok(mut stream) => {
                let message = VerificationMessage {
                    nonce: 0,
                    netuid: *config::NETUID,

                    miner: KeyRegistrationInfo {
                        uid: neuron.uid.0,
                        account_id: neuron.hotkey.clone(),
                    },

                    validator: KeyRegistrationInfo {
                        uid,
                        account_id: signer.account_id().clone(),
                    },
                };

                let signature = sign_message(signer, &message);

                if let Err(e) = stream.write((&message).as_ref()) {
                    error!("Failed to write to miner {uid}, {e}", uid = neuron.uid.0);

                    return None;
                };

                if let Err(e) = stream.write(&signature) {
                    error!("Failed to write to miner {uid}, {e}", uid = neuron.uid.0);

                    return None;
                }

                Some(stream)
            }
            Err(error) => {
                warn!(
                    "Couldn't connect to neuron {uid}, {error}",
                    uid = neuron.uid.0
                );

                None
            }
        }
    }

    // SAFETY: Unsafe because it extends the lifetime
    unsafe fn find_connections(&mut self) -> Vec<(u16, &'static mut Option<TcpStream>)> {
        self.neurons
            .iter_mut()
            .enumerate()
            .filter_map(|(uid, neuron)| {
                if neuron.connection.as_ref().is_none() {
                    None
                } else {
                    Some((uid as u16, transmute(neuron.connection.get_mut())))
                }
            })
            .collect()
    }

    async fn do_step(&mut self) -> Result<()> {
        info!("Evolution step {}", self.step);

        let current_block = self.subtensor.get_block_number().await?;
        let mut elapsed_blocks = current_block - self.last_metagraph_sync;

        if elapsed_blocks >= *config::EPOCH_LENGTH {
            self.sync(Some(current_block)).await?;
            elapsed_blocks = 0;
        }

        while self
            .neurons
            .iter()
            .all(|neuron| neuron.connection.as_ref().is_none())
        {
            let sleep_for = *config::EPOCH_LENGTH - elapsed_blocks;
            info!("No miners found, waiting {} blocks", sleep_for);

            sleep(Duration::from_secs(sleep_for * 12)).await;
            self.sync(Some(current_block)).await?;
        }

        let connections = unsafe {
            // SAFETY: Safe because we do not resync(which modifies the references) until the connections are no longer used.
            self.find_connections()
        };

        self.current_row
            .ensure_capacity(Self::current_row_file_size(self.step))?;

        self.center_column
            .ensure_capacity(Self::center_column_file_size(self.step))?;

        let connection_count = connections.len() as u64;

        let byte_count = (self.step * 2 + 1).div_ceil(8);

        let chunk_size = byte_count.div_ceil(connection_count);

        for (index, (uid, connection)) in connections.into_iter().enumerate() {
            unsafe {
                // SAFETY: This is safe as the data read/written does not overlap between threads
                let row = self.current_row.share();
                let cheater = SendPtr::from(self.neurons[uid as usize].cheater.get());
                let score = SendPtr::from(self.neurons[uid as usize].score.get());

                let end = min((index as u64 + 1) * chunk_size, byte_count);

                self.thread_pool.execute(move || {
                    Self::handle_connection(row, connection, score.as_mut_unchecked(), cheater.as_mut_unchecked(), index as u64 * chunk_size, end)
                });
            }
        }

        self.thread_pool.join();

        for i in 1..connection_count - 1 {
            let end = ((i + 1) * chunk_size) as usize;

            let [a, b] = self.current_row[end..end + 1] else {
                unreachable!()
            };

            let (a, b) = Self::normalize_pair(a, b);

            self.current_row[end..end + 1].copy_from_slice(&[a, b]);
        }

        let bit_index = self.step % 8;
        let part = self.step / 8;
        let current_row_part = (self.current_row[part as usize] >> bit_index) & 0b1;

        self.center_column[part as usize] |= current_row_part << bit_index;

        self.step += 1;
        self.save_state()?;

        Ok(())
    }

    pub(crate) async fn run(&mut self) {
        loop {
            if let Err(e) = self.do_step().await {
                error!("Error during evolution step {step}, {e}", step = self.step);
            }
        }
    }

    fn normalize_pair(a: u8, b: u8) -> (u8, u8) {
        fn rule_30(a: u8) -> u8 {
            a ^ ((a << 1) | (a << 2))
        }

        let carry = a & 1;
        let mut a = a >> 1;
        let mut b = (carry << 7) | b;
        a = rule_30(a);
        b = rule_30(b);
        let msb = b >> 7;
        a = (a << 1) | msb;

        (a, b)
    }
}
