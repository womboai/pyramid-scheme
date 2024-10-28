use std::cell::UnsafeCell;
use std::cmp::min;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use threadpool::ThreadPool;
use tokio::time::sleep;
use tracing::{error, info};

use neuron::{AccountId, config, hotkey_location, load_key_seed, NeuronInfoLite, Signer, signer_from_seed, Subtensor};
use neuron::auth::{KeyRegistrationInfo, sign_message, VerificationMessage};

use crate::validator::memory_storage::{MemoryMapped, MemoryMappedFile, MemoryMappedStorage};

mod memory_storage;

const VERSION_KEY: u64 = 1;
const GROW_BY: u64 = 1024 * 1024 * 8;

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
    hotkey: AccountId,
    score: u16,
}

#[derive(Debug, Serialize, Deserialize)]
struct ValidatorState {
    step: u64,
    key_info: Vec<KeyScoreInfo>,
}

impl ValidatorState {
    fn for_hotkeys(hotkeys: impl Iterator<Item = AccountId>) -> Self {
        let key_info = hotkeys.map(|hotkey| KeyScoreInfo { hotkey, score: 0 }).collect();

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

pub struct Validator {
    signer: Signer,
    subtensor: Subtensor,
    neurons: Vec<NeuronInfoLite>,
    uid: u16,

    current_row: CurrentRow,
    center_column: MemoryMappedFile,
    state: ValidatorState,

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
        let hotkey_location =
            hotkey_location(config::WALLET_PATH.clone(), &*config::WALLET_NAME, &*config::HOTKEY_NAME);

        let seed = load_key_seed(&hotkey_location).unwrap();

        let keypair = signer_from_seed(&seed).unwrap();

        let subtensor = Subtensor::new(&*config::CHAIN_ENDPOINT)
            .await
            .unwrap();

        let neurons: Vec<NeuronInfoLite> =
            subtensor.get_neurons(*config::NETUID).await.unwrap();

        let last_metagraph_sync = subtensor.get_block_number().await.unwrap();
        let neuron_info = Self::find_neuron_info(&neurons, keypair.account_id());

        let uid = if let Some(neuron_info) = neuron_info {
            neuron_info.uid.0
        } else {
            Self::not_registered(keypair.account_id());
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

        Self {
            signer: keypair,
            subtensor,
            neurons,
            uid,
            current_row,
            center_column,
            state,
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

        let json = serde_json::to_string(&self.state)?;

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

    async fn sync(&mut self, block: Option<u64>) -> Result<()> {
        self.neurons = self.subtensor.get_neurons(*config::NETUID).await?;

        let block = if let Some(block) = block {
            block
        } else {
            self.subtensor.get_block_number().await?
        };

        self.last_metagraph_sync = block;

        let neuron_info = Self::find_neuron_info(&self.neurons, self.signer.account_id());

        let neuron_info = if let Some(neuron_info) = neuron_info {
            neuron_info
        } else {
            Self::not_registered(self.signer.account_id());
        };

        self.uid = neuron_info.uid.0;

        // Update scores array size if needed
        if self.state.key_info.len() != self.neurons.len() {
            let mut uid_iterator = (self.state.key_info.len()..self.neurons.len()).into_iter();

            self.state
                .key_info
                .resize_with(self.neurons.len(), || KeyScoreInfo {
                    hotkey: self.neurons[uid_iterator.next().unwrap()].hotkey.clone(),
                    score: 0,
                });
        }

        // Set weights if enough time has passed
        if block - neuron_info.last_update.0 >= *config::EPOCH_LENGTH {
            self.subtensor
                .set_weights(
                    &self.signer,
                    *config::NETUID,
                    self.state
                        .key_info
                        .iter()
                        .enumerate()
                        .map(|(uid, &ref info)| (uid as u16, info.score))
                        .collect(),
                    VERSION_KEY,
                )
                .await?;
        }

        Ok(())
    }

    fn handle_connection(
        mut current_row: CurrentRow,
        mut connection: TcpStream,
        start: u64,
        end: u64,
    ) {
        let buffer_size = min(end - start, 8 * 4 * 256);

        let iterations = (end - start).div_ceil(buffer_size);

        for i in 0..iterations {
            let from = (start + i * buffer_size) as usize;
            let to = (start + (i + 1) * buffer_size) as usize;

            // TODO error handle
            connection.write(&current_row[from..to]).unwrap();
            connection.read(&mut current_row[from..to]).unwrap();
        }
    }

    fn connect_to_miners(&self) -> Vec<(u16, AccountId, TcpStream)> {
        let mut connections = Vec::with_capacity(256);

        for neuron in &self.neurons {
            let ip: IpAddr = if neuron.axon_info.ip_type == 4 {
                Ipv4Addr::from(neuron.axon_info.ip as u32).into()
            } else {
                Ipv6Addr::from(neuron.axon_info.ip).into()
            };

            let address = SocketAddr::new(ip, neuron.axon_info.port);

            info!("Attempting to connect to {address}");

            if let Ok(stream) = TcpStream::connect(address) {
                connections.push((neuron.uid.0, neuron.hotkey.clone(), stream));
            }
        }

        connections
    }

    async fn do_step(&mut self) -> Result<()> {
        info!("Evolution step {}", self.state.step);

        let current_block = self.subtensor.get_block_number().await?;
        let elapsed_blocks = current_block - self.last_metagraph_sync;

        if elapsed_blocks >= *config::EPOCH_LENGTH {
            self.sync(Some(current_block)).await?;
        }

        self.current_row
            .ensure_capacity(Self::current_row_file_size(self.state.step))?;
        self.center_column
            .ensure_capacity(Self::center_column_file_size(self.state.step))?;

        let mut connections = self.connect_to_miners();

        while connections.is_empty() {
            info!("No miners found, waiting 60 seconds");
            sleep(Duration::from_secs(60)).await;
            connections = self.connect_to_miners();
        }

        let connection_count = connections.len() as u64;

        let byte_count = (self.state.step * 2 + 1).div_ceil(8);

        let chunk_size = byte_count.div_ceil(connection_count);

        // TODO Handle connection prematurely dying or giving invalid results
        for (index, (uid, account_id, mut connection)) in connections.into_iter().enumerate() {
            let message = VerificationMessage {
                nonce: 0,
                netuid: *config::NETUID,

                miner: KeyRegistrationInfo {
                    uid,
                    account_id,
                },

                validator: KeyRegistrationInfo {
                    uid: self.uid,
                    account_id: self.signer.account_id().clone(),
                },
            };

            let signature = sign_message(&self.signer, &message);

            if let Err(e) = connection.write((&message).as_ref()) {
                error!("Failed to write to miner {uid}, {e}");

                continue
            };

            if let Err(e) = connection.write(&signature) {
                error!("Failed to write to miner {uid}, {e}");

                continue
            };

            unsafe {
                // SAFETY: This is safe as the data read/written does not overlap between threads
                let row = self.current_row.share();

                self.thread_pool.execute(move || {
                    Self::handle_connection(
                        row,
                        connection,
                        index as u64 * chunk_size,
                        (index as u64 + 1) * chunk_size,
                    )
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

        let bit_index = self.state.step % 8;
        let part = self.state.step / 8;
        let current_row_part = (self.current_row[part as usize] >> bit_index) & 0b1;

        self.center_column[part as usize] |= current_row_part << bit_index;

        self.state.step += 1;
        self.save_state()?;

        Ok(())
    }

    pub(crate) async fn run(&mut self) {
        loop {
            if let Err(e) = self.do_step().await {
                error!(
                    "Error during evolution step {step}, {e}",
                    step = self.state.step
                );
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
