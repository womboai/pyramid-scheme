use neuron::auth::{KeyRegistrationInfo, VerificationMessage};
use neuron::{config, subtensor};
use rusttensor::{AccountId, Block, BlockNumber};

use crate::validator::memory_storage::{MemoryMapped, MemoryMappedFile, MemoryMappedStorage};
use crate::validator::metrics::ValidatorMetrics;
use crate::validator::neuron_data::{ConnectionGuard, ConnectionState, NeuronData, Score};

use anyhow::Result;
use rusttensor::api::apis;
use rusttensor::rpc::call_runtime_api_decoded;
use rusttensor::rpc::types::NeuronInfoLite;
use rusttensor::sign::sign_message;
use rusttensor::subtensor::Subtensor;
use rusttensor::wallet::Signer;
use rusttensor::weights::{set_weights_payload, NormalizedWeight};
use serde::{Deserialize, Serialize};
use std::cell::UnsafeCell;
use std::cmp::min;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::ops::{Deref, DerefMut, Range};
use std::path::{Path, PathBuf};
use std::random::random;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpmc::Sender;
use std::sync::{mpmc, Arc};
use std::time::{Duration, Instant};
use std::{fs, thread};
use tracing::log::warn;
use tracing::{debug, error, info};

mod memory_storage;
pub mod metrics;
mod neuron_data;

const WEIGHTS_VERSION: u64 = 1;
const GROW_BY: u64 = 1024 * 1024 * 8;
const VALIDATION_CHANCE: f32 = 0.3;
const DATA_SPEC_VERSION: u64 = 1;

pub(crate) const STATE_DATA_FILE: &'static str = "state/data.json";
pub(crate) const CURRENT_ROW_FILE: &'static str = "state/current_row.bin";
pub(crate) const CENTER_COLUMN_FILE: &'static str = "state/center_column.bin";

struct CurrentRow(UnsafeCell<MemoryMappedStorage>);

impl CurrentRow {
    fn inner_mut(&self) -> &mut MemoryMappedStorage {
        unsafe { &mut *self.0.get() }
    }
}

impl MemoryMapped for CurrentRow {
    fn open(path: impl AsRef<Path>, initial_capacity: u64) -> std::io::Result<Self> {
        Ok(Self(UnsafeCell::new(MemoryMappedStorage::open(
            path,
            initial_capacity,
        )?)))
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
        self.inner_mut()
    }
}

impl DerefMut for CurrentRow {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner_mut()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct KeyScoreInfo {
    score: Score,
    hotkey: AccountId,
}

#[derive(Debug, Serialize, Deserialize)]
struct ValidatorState {
    step: u64,
    key_info: Vec<KeyScoreInfo>,
    version: Option<u64>,
}

impl ValidatorState {
    fn for_hotkeys(hotkeys: impl Iterator<Item = AccountId>) -> Self {
        let key_info = hotkeys
            .map(|hotkey| KeyScoreInfo {
                hotkey,
                score: Score::default(),
            })
            .collect();

        Self {
            step: 1,
            key_info,
            version: Some(DATA_SPEC_VERSION),
        }
    }
}

impl Default for ValidatorState {
    fn default() -> Self {
        Self {
            step: 1,
            key_info: Vec::new(),
            version: Some(DATA_SPEC_VERSION),
        }
    }
}

enum ProcessingCompletionState {
    Completed(u64),
    Failed(u64, Range<u64>),
    Cheated(Range<u64>),
}

struct ProcessingCompletionResult {
    uid: u16,
    state: ProcessingCompletionState,
}

pub struct Validator {
    signer: Signer,
    subtensor: Subtensor,
    neurons: Vec<NeuronData>,
    pub(crate) uid: u16,

    current_row: CurrentRow,
    center_column: MemoryMappedFile,
    step: u64,

    current_block: Block,
    last_block_fetch: Instant,
    attempted_set_weights: bool,

    metrics: Arc<ValidatorMetrics>,
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
        (Self::step_to_grow_to(step) * 2 - 1).div_ceil(8)
    }

    fn center_column_file_size(step: u64) -> u64 {
        Self::step_to_grow_to(step).div_ceil(8)
    }

    pub async fn new(signer: Signer, metrics: Arc<ValidatorMetrics>) -> Self {
        let subtensor = subtensor().await.unwrap();

        let current_block = subtensor.blocks().at_latest().await.unwrap();
        let runtime_api = subtensor.runtime_api().at(current_block.reference());
        let neurons_payload = apis()
            .neuron_info_runtime_api()
            .get_neurons_lite(*config::NETUID);
        let neurons = call_runtime_api_decoded(&runtime_api, neurons_payload)
            .await
            .unwrap();

        let neuron_info = Self::find_neuron_info(&neurons, signer.account_id());
        let last_block_fetch = Instant::now();

        let neuron_info = if let Some(neuron_info) = neuron_info {
            neuron_info.clone()
        } else {
            Self::not_registered(signer.account_id());
        };

        let mut state =
            Self::load_state(neurons.iter().map(|neuron| neuron.hotkey.clone())).unwrap();

        let valid_state = if let Some(version) = state.version {
            version == DATA_SPEC_VERSION
        } else {
            false
        };

        fs::create_dir_all("state").unwrap();

        if !valid_state {
            let result = fs::remove_file(CURRENT_ROW_FILE);

            if let Err(e) = &result {
                if e.kind() != ErrorKind::NotFound {
                    result.unwrap();
                }
            }

            let result = fs::remove_file(CENTER_COLUMN_FILE);

            if let Err(e) = &result {
                if e.kind() != ErrorKind::NotFound {
                    result.unwrap();
                }
            }

            state = ValidatorState::for_hotkeys(neurons.iter().map(|neuron| neuron.hotkey.clone()));
        }

        let mut current_row =
            CurrentRow::open(CURRENT_ROW_FILE, Self::current_row_file_size(state.step)).unwrap();

        let mut center_column = MemoryMappedFile::open(
            CENTER_COLUMN_FILE,
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
                let state_info = &state.key_info.get(info.uid.0 as usize);

                if state_info.is_some() && state_info.unwrap().hotkey == info.hotkey {
                    NeuronData {
                        score: state_info.unwrap().score.into(),
                        connection: Self::connect_to_miner(
                            &signer,
                            neuron_info.uid.0,
                            &info,
                            matches!(state_info.unwrap().score, Score::Cheater),
                            metrics.clone(),
                        )
                        .into(),
                        info,
                    }
                } else {
                    NeuronData {
                        score: Score::default().into(),
                        connection: Self::connect_to_miner(
                            &signer,
                            neuron_info.uid.0,
                            &info,
                            false,
                            metrics.clone(),
                        )
                        .into(),
                        info,
                    }
                }
            })
            .collect::<Vec<_>>();

        Self {
            signer,
            subtensor,
            neurons,
            uid: neuron_info.uid.0,
            current_row,
            center_column,
            step: state.step,

            current_block,
            last_block_fetch,
            attempted_set_weights: false,

            metrics,
        }
    }

    fn save_state(&mut self) -> Result<()> {
        let path = PathBuf::from(STATE_DATA_FILE);

        self.center_column.flush()?;
        self.current_row.flush()?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let state = ValidatorState {
            key_info: self
                .neurons
                .iter_mut()
                .map(|data| KeyScoreInfo {
                    hotkey: data.info.hotkey.clone(),
                    score: *data.score.get_mut().unwrap(),
                })
                .collect(),
            step: self.step,
            version: Some(DATA_SPEC_VERSION),
        };

        let json = serde_json::to_string(&state)?;

        fs::write(&path, json)?;

        Ok(())
    }

    fn load_state(hotkeys: impl Iterator<Item = AccountId>) -> Result<ValidatorState> {
        info!("Loading state");

        let path = PathBuf::from(STATE_DATA_FILE);

        if !path.exists() {
            return Ok(ValidatorState::for_hotkeys(hotkeys));
        }

        let json = fs::read_to_string(&path)?;

        Ok(serde_json::from_str(&json)?)
    }

    async fn set_weights(
        neurons: &mut [NeuronData],
        subtensor: &Subtensor,
        signer: &Signer,
    ) -> Result<()> {
        if neurons.is_empty() {
            return Ok(());
        }

        info!("Setting weights");

        let max_score: u128 = neurons
            .iter_mut()
            .map(|info| u128::from(*info.score.get_mut().unwrap()))
            .max()
            .expect("Neurons should never be empty at this point, thus max should never be None");

        let scores = neurons
            .iter_mut()
            .map(|neuron| {
                let normalized_score = match *neuron.score.get_mut().unwrap() {
                    Score::Legitimate(score) => {
                        if max_score == 0 {
                            u16::MAX
                        } else {
                            ((score * u16::MAX as u128) / max_score) as u16
                        }
                    }
                    Score::Cheater => 0,
                };

                NormalizedWeight {
                    uid: neuron.info.uid.0,
                    weight: normalized_score,
                }
            })
            .collect();

        let payload =
            set_weights_payload(*config::NETUID, scores, DATA_SPEC_VERSION + WEIGHTS_VERSION);

        subtensor
            .tx()
            .sign_and_submit_default(&payload, signer)
            .await?;

        Ok(())
    }

    async fn sync(&mut self) -> Result<bool> {
        let set_weights = if let Err(e) =
            Self::set_weights(&mut self.neurons, &self.subtensor, &self.signer).await
        {
            self.attempted_set_weights = true;

            error!("Failed to set weights. {e}");

            false
        } else {
            true
        };

        let start = Instant::now();

        if set_weights {
            info!("Syncing metagraph and connecting to miners");

            let runtime_api = self
                .subtensor
                .runtime_api()
                .at(self.current_block.reference());
            let neurons = call_runtime_api_decoded(
                &runtime_api,
                apis()
                    .neuron_info_runtime_api()
                    .get_neurons_lite(*config::NETUID),
            )
            .await?;

            let neuron_info = Self::find_neuron_info(&neurons, self.signer.account_id());

            let neuron_info = if let Some(neuron_info) = neuron_info {
                neuron_info
            } else {
                Self::not_registered(self.signer.account_id());
            };

            self.uid = neuron_info.uid.0;

            // Update changed hotkeys
            for i in 0..self.neurons.len() {
                let neuron = &mut self.neurons[i];

                if neuron.info.hotkey != neurons[i].hotkey {
                    let info = neurons[i].clone();

                    self.neurons[i] = NeuronData {
                        score: Score::default().into(),
                        connection: Self::connect_to_miner(
                            &self.signer,
                            self.uid,
                            &info,
                            false,
                            self.metrics.clone(),
                        )
                        .into(),
                        info,
                    };
                } else {
                    if matches!(
                        neuron.connection.get_mut().unwrap(),
                        ConnectionState::Disconnected
                    ) || neurons[i].axon_info != neuron.info.axon_info
                    {
                        neuron.connection = Self::connect_to_miner(
                            &self.signer,
                            self.uid,
                            &neurons[i],
                            matches!(*neuron.score.get_mut().unwrap(), Score::Cheater),
                            self.metrics.clone(),
                        )
                        .into()
                    }

                    neuron.info = neurons[i].clone();
                }
            }

            // Update scores array size if needed
            if self.neurons.len() != neurons.len() {
                let mut uid_iterator = (self.neurons.len()..neurons.len()).into_iter();

                self.neurons.resize_with(neurons.len(), || {
                    let info =
                        neurons[uid_iterator.next().expect("There is more new UIDs")].clone();

                    NeuronData {
                        score: Score::default().into(),
                        connection: Self::connect_to_miner(
                            &self.signer,
                            self.uid,
                            &info,
                            false,
                            self.metrics.clone(),
                        )
                        .into(),
                        info,
                    }
                });
            }
        }

        self.metrics
            .sync_duration
            .record(start.elapsed().as_secs_f64(), &[]);

        Ok(set_weights)
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
        current_row: &mut MemoryMappedStorage,
        mut connection: ConnectionGuard,
        start: u64,
        end: u64,
        uid: u16,
        completion_events: Sender<ProcessingCompletionResult>,
        metrics: Arc<ValidatorMetrics>,
    ) {
        let buffer_size = min(end - start, 8 * 4 * 256);
        let mut buffer = Vec::with_capacity(buffer_size as usize);

        unsafe { buffer.set_len(buffer_size as usize) }

        let iterations = (end - start).div_ceil(buffer_size);

        let chance = if iterations > 1 {
            VALIDATION_CHANCE * iterations as f32 - VALIDATION_CHANCE.powi(iterations as i32)
        } else {
            VALIDATION_CHANCE
        };

        let validation_start_index = if random::<u16>() as f32 / u16::MAX as f32 <= chance {
            let t = random::<u16>() as f32 / u16::MAX as f32;
            let validation_start_index = ((end - start) as f32 * t) as u64;

            Some(validation_start_index)
        } else {
            None
        };

        let mut processed = 0;

        for i in 0..iterations {
            let from = (start + i * buffer_size) as usize;
            let to = (start + (i + 1) * buffer_size) as usize;

            while processed as usize != to - from {
                let written = match connection.write(&current_row[from + processed as usize..to]) {
                    Ok(len) => {
                        if len == 0 {
                            warn!(
                                "Failed to write to miner {uid} connection while there's more to process"
                            );

                            completion_events
                                .send(ProcessingCompletionResult {
                                    uid,
                                    state: ProcessingCompletionState::Failed(
                                        processed,
                                        start + processed..end,
                                    ),
                                })
                                .unwrap();

                            return;
                        }

                        len
                    }
                    Err(error) => {
                        warn!("Error occurred writing to miner {uid}: {error}");

                        completion_events
                            .send(ProcessingCompletionResult {
                                uid,
                                state: ProcessingCompletionState::Failed(
                                    processed,
                                    start + processed..end,
                                ),
                            })
                            .unwrap();

                        return;
                    }
                };

                let mut read = 0;

                while read < written {
                    let range =
                        from + read + processed as usize..from + written + processed as usize;

                    let should_validate =
                        if let Some(validation_start_index) = validation_start_index {
                            if range.contains(&(validation_start_index as usize)) {
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                    let len = match connection.read(&mut buffer[0..range.len()]) {
                        Ok(len) => {
                            if len == 0 {
                                warn!("Failed to read from miner {uid} connection");

                                completion_events
                                    .send(ProcessingCompletionResult {
                                        uid,
                                        state: ProcessingCompletionState::Failed(
                                            processed,
                                            start + processed..end,
                                        ),
                                    })
                                    .unwrap();

                                return;
                            }

                            len
                        }
                        Err(error) => {
                            warn!("Error occurred reading from miner {uid}: {error}");

                            completion_events
                                .send(ProcessingCompletionResult {
                                    uid,
                                    state: ProcessingCompletionState::Failed(
                                        processed,
                                        start + processed..end,
                                    ),
                                })
                                .unwrap();

                            return;
                        }
                    };

                    let read_chunk_range = range.start..range.start + len;

                    if should_validate {
                        info!("Verifying results of {uid}");
                        if !Self::verify_result(
                            &current_row[range.start..range.start + len],
                            &buffer[..len],
                        ) {
                            info!("{uid} marked as cheater");
                            metrics.cheater_count.add(1, &[]);

                            completion_events
                                .send(ProcessingCompletionResult {
                                    uid,
                                    state: ProcessingCompletionState::Cheated(start..end),
                                })
                                .unwrap();

                            return;
                        }
                    }

                    (&mut current_row[read_chunk_range]).copy_from_slice(&buffer[..len]);
                    read += len;
                }

                processed += written as u64;
            }
        }

        completion_events
            .send(ProcessingCompletionResult {
                uid,
                state: ProcessingCompletionState::Completed(processed),
            })
            .unwrap();
    }

    fn connect_to_miner(
        signer: &Signer,
        uid: u16,
        neuron: &NeuronInfoLite,
        cheater: bool,
        metrics: Arc<ValidatorMetrics>,
    ) -> ConnectionState {
        if cheater {
            return ConnectionState::Unusable;
        }

        let ip: IpAddr = if neuron.axon_info.ip_type == 4 {
            Ipv4Addr::from(neuron.axon_info.ip as u32).into()
        } else {
            Ipv6Addr::from(neuron.axon_info.ip).into()
        };

        let address = SocketAddr::new(ip, neuron.axon_info.port);

        info!("Attempting to connect to {address}");

        match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            Ok(mut stream) => {
                if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
                    warn!("Not continuing connection to uid {uid} at {address} because could not configure read timeout, {e}", uid = neuron.uid.0);

                    return ConnectionState::Unusable;
                }

                if let Err(e) = stream.set_write_timeout(Some(Duration::from_secs(5))) {
                    warn!("Not continuing connection to uid {uid} at {address} because could not configure write timeout, {e}", uid = neuron.uid.0);

                    return ConnectionState::Unusable;
                }

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
                    warn!("Failed to write to miner {uid}, {e}", uid = neuron.uid.0);

                    return ConnectionState::Unusable;
                };

                if let Err(e) = stream.write(&signature) {
                    warn!("Failed to write to miner {uid}, {e}", uid = neuron.uid.0);

                    return ConnectionState::Unusable;
                }

                metrics.connected_miners.add(1, &[]);
                ConnectionState::connected(stream)
            }
            Err(error) => {
                warn!(
                    "Couldn't connect to neuron {uid}, {error}",
                    uid = neuron.uid.0
                );

                ConnectionState::Unusable
            }
        }
    }

    fn worker_count_hint(&mut self) -> usize {
        let mut count = 0;

        for x in self.neurons.iter_mut() {
            if matches!(
                x.connection.get_mut().unwrap(),
                ConnectionState::Connected { .. }
            ) {
                count += 1;
            }
        }

        count
    }

    fn find_suitable_connection<'a, 'b>(&'a self) -> (u16, ConnectionGuard<'b>)
    where
        'a: 'b,
    {
        let free_connection = self
            .neurons
            .iter()
            .enumerate()
            .find_map(|(uid, &ref neuron)| {
                if let Some(mut guard) = neuron.connection.try_lock().ok() {
                    if let ConnectionState::Connected { free: true, .. } = &mut *guard {
                        Some((uid as u16, ConnectionGuard::<'b>::new(guard)))
                    } else {
                        None
                    }
                } else {
                    None
                }
            });

        if let Some(result) = free_connection {
            return result;
        };

        info!("No suitable miners found, reconnecting");

        for (uid, neuron) in self.neurons.iter().enumerate() {
            if let Ok(mut guard) = neuron.connection.try_lock() {
                if let ConnectionState::Disconnected = &*guard {
                    *guard = Self::connect_to_miner(
                        &self.signer,
                        uid as u16,
                        &neuron.info,
                        false,
                        self.metrics.clone(),
                    );

                    if matches!(*guard, ConnectionState::Connected { .. }) {
                        return (uid as u16, ConnectionGuard::<'b>::new(guard));
                    }
                }
            }
        }

        panic!(
            "No suitable miners remaining for this step, crashing to revert to a previous state."
        );
    }

    fn release_connection(&self, uid: u16) {
        let mut guard = self.neurons[uid as usize].connection.lock().unwrap();

        if let ConnectionState::Connected { free, .. } = &mut *guard {
            *free = true
        }
    }

    async fn do_step(&mut self) -> Result<()> {
        let start = Instant::now();

        info!("Evolution step {}", self.step);

        let mut elapsed_blocks = self.current_block.number()
            - self.neurons[self.uid as usize].info.last_update.0 as BlockNumber;

        let should_sleep = if !self.attempted_set_weights {
            info!(
                "Current block is {block}, it has been {elapsed_blocks} blocks since last update",
                block = self.current_block.number()
            );

            if elapsed_blocks >= *config::EPOCH_LENGTH {
                let set_weights = self.sync().await?;

                elapsed_blocks = 0;

                set_weights
            } else {
                true
            }
        } else {
            true
        };

        let connection_count = self.worker_count_hint() as u64;

        if connection_count == 0 {
            if should_sleep {
                let blocks = *config::EPOCH_LENGTH - elapsed_blocks;

                warn!("No connections available, sleeping for {blocks} to resync");

                tokio::time::sleep(Duration::from_secs(blocks as u64 * 12)).await;
            } else {
                warn!("No connections available, retrying");
            }

            return Ok(());
        }

        self.current_row
            .ensure_capacity(Self::current_row_file_size(self.step))?;

        self.center_column
            .ensure_capacity(Self::center_column_file_size(self.step))?;

        info!("Splitting work into {connection_count} chunks");

        let byte_count = (self.step * 2 + 3).div_ceil(8);

        let chunk_size = byte_count.div_ceil(connection_count);

        let (work_queue_sender, work_queue_receiver) = mpmc::channel();
        let (completion_sender, completion_receiver) = mpmc::channel();

        let mut concurrent_worker_count = connection_count;

        for index in 0..connection_count {
            let start = index * chunk_size;
            let end = min((index + 1) * chunk_size, byte_count);

            let range = start..end;

            debug!("Adding {range:?} to work queue");

            work_queue_sender
                .send(range)
                .expect("Work queue channel should not be closed");

            if end == byte_count {
                concurrent_worker_count = index + 1;
                break;
            }
        }

        let data_processed = AtomicU64::new(0);

        thread::scope(|scope| {
            debug!("Spawning completion event thread");

            let _handle = scope.spawn(|| {
                while data_processed.load(Ordering::Relaxed) < byte_count {
                    let Ok(event): Result<ProcessingCompletionResult, _> =
                        completion_receiver.try_recv()
                    else {
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    };

                    let uid = event.uid;

                    let mut score = self.neurons[uid as usize].score.write().unwrap();

                    match event.state {
                        ProcessingCompletionState::Completed(processed) => {
                            debug!("Miner {uid} finished assigned work");

                            self.release_connection(uid);
                            data_processed.fetch_add(processed, Ordering::Relaxed);
                            *score += processed as u128;
                        }
                        ProcessingCompletionState::Failed(processed, remaining) => {
                            debug!("Miner {uid} failed at assigned work");

                            *self.neurons[uid as usize].connection.lock().unwrap() =
                                ConnectionState::Disconnected;
                            self.metrics.connected_miners.add(-1, &[]);
                            data_processed.fetch_add(processed, Ordering::Relaxed);
                            *score += processed as u128;

                            work_queue_sender
                                .send(remaining)
                                .expect("Work queue channel should not be closed");
                        }
                        ProcessingCompletionState::Cheated(range) => {
                            debug!("Miner {uid} marked as cheater");

                            *self.neurons[uid as usize].connection.lock().unwrap() =
                                ConnectionState::Unusable;
                            self.metrics.connected_miners.add(-1, &[]);
                            *score = Score::Cheater;

                            work_queue_sender
                                .send(range)
                                .expect("Work queue channel should not be closed");
                        }
                    }
                }
            });

            info!("Spawning {concurrent_worker_count} worker threads");

            let mut handles = Vec::with_capacity(concurrent_worker_count as usize);

            for i in 0..concurrent_worker_count {
                debug!("Spawning worker thread {i}");

                let handle = scope.spawn(|| {
                    while data_processed.load(Ordering::Relaxed) < byte_count {
                        let Ok(range) = work_queue_receiver.try_recv() else {
                            thread::sleep(Duration::from_millis(100));
                            continue;
                        };

                        debug!("Finding suitable miner for {range:?}");

                        let (uid, connection) = self.find_suitable_connection();

                        debug!("Assigned {range:?} to miner {uid}");

                        Self::handle_connection(
                            self.current_row.inner_mut(),
                            connection,
                            range.start,
                            range.end,
                            uid,
                            completion_sender.clone(),
                            self.metrics.clone(),
                        );
                    }
                });

                handles.push(handle);
            }
        });

        for i in 0..connection_count - 1 {
            let end = ((i + 1) * chunk_size) as usize;

            if end >= byte_count as usize {
                break;
            }

            let [a, b] = self.current_row[end..end + 1] else {
                break;
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

        self.metrics.evolution_steps.record(self.step, &[]);
        self.metrics
            .step_duration
            .record(start.elapsed().as_secs_f64(), &[]);
        Ok(())
    }

    pub(crate) async fn run(&mut self) {
        loop {
            if self.last_block_fetch.elapsed() > Duration::from_secs(12) {
                let block = match self.subtensor.blocks().at_latest().await {
                    Ok(block) => block,
                    Err(e) => {
                        error!("Failed to fetch block, {e}. Retrying");

                        tokio::time::sleep(Duration::from_secs(12)).await;

                        return;
                    }
                };

                self.current_block = block;
                self.last_block_fetch = Instant::now();
                self.attempted_set_weights = false;
            }

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
