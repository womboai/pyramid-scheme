use neuron::{config, subtensor};
use rusttensor::{AccountId, Block, BlockNumber};

use crate::validator::memory_storage::{MemoryMapped, MemoryMappedFile, MemoryMappedStorage};
use crate::validator::metrics::ValidatorMetrics;
use crate::validator::neuron_data::{ConnectionState, NeuronData, Score};

use crate::validator::completion_event_handler::handle_completion_events;
use crate::validator::connection::{connect_to_miner, worker_count_hint};
use crate::validator::evolution_step::normalize_pair;
use crate::validator::worker::{do_work, ProcessingCompletionResult};
use anyhow::Result;
use rusttensor::api::apis;
use rusttensor::rpc::call_runtime_api_decoded;
use rusttensor::rpc::types::NeuronInfoLite;
use rusttensor::subtensor::Subtensor;
use rusttensor::wallet::Signer;
use rusttensor::weights::{set_weights_payload, NormalizedWeight};
use serde::{Deserialize, Serialize};
use std::cell::UnsafeCell;
use std::cmp::min;
use std::io::ErrorKind;
use std::mem::transmute;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpmc::{Receiver, Sender};
use std::sync::{mpmc, Arc};
use std::time::{Duration, Instant};
use std::{fs, thread};
use std::pin::Pin;
use tracing::log::warn;
use tracing::{debug, error, info};

mod completion_event_handler;
mod connection;
mod evolution_step;
mod memory_storage;
pub mod metrics;
mod neuron_data;
mod worker;

const WEIGHTS_VERSION: u64 = 1;
const GROW_BY: u64 = 1024 * 1024 * 8;
const DATA_SPEC_VERSION: u64 = 1;

pub(crate) const STATE_DATA_FILE: &'static str = "state/data.json";
pub(crate) const CURRENT_ROW_FILE: &'static str = "state/current_row.bin";
pub(crate) const CENTER_COLUMN_FILE: &'static str = "state/center_column.bin";

struct CurrentRow(UnsafeCell<MemoryMappedStorage>);

impl CurrentRow {
    fn get_mut(&mut self) -> &mut MemoryMappedStorage {
        self.0.get_mut()
    }

    unsafe fn get(&self) -> &mut MemoryMappedStorage {
        &mut *self.0.get()
    }
}

impl MemoryMapped for CurrentRow {
    fn open(path: impl AsRef<Path>, initial_capacity: u64) -> std::io::Result<Self> {
        Ok(Self(UnsafeCell::new(MemoryMappedStorage::open(
            path,
            initial_capacity,
        )?)))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.get_mut().flush()
    }

    fn ensure_capacity(&mut self, capacity: u64) -> std::io::Result<()> {
        self.get_mut().ensure_capacity(capacity)
    }
}

unsafe impl Send for CurrentRow {}
unsafe impl Sync for CurrentRow {}

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

pub struct Validator {
    signer: Signer,
    subtensor: Subtensor,
    neurons: Pin<Box<Vec<NeuronData>>>,
    pub(crate) uid: u16,

    current_row: Pin<Box<CurrentRow>>,
    center_column: MemoryMappedFile,
    step: u64,

    current_block: Block,
    last_block_fetch: Instant,
    attempted_set_weights: bool,

    work_queue_sender: Sender<Range<u64>>,
    work_queue_receiver: Receiver<Range<u64>>,
    completion_sender: Sender<ProcessingCompletionResult>,
    completion_receiver: Receiver<ProcessingCompletionResult>,

    worker_threads: Vec<thread::JoinHandle<()>>,

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

        let current_row =
            CurrentRow::open(CURRENT_ROW_FILE, Self::current_row_file_size(state.step)).unwrap();

        let mut current_row = Box::pin(current_row);

        let mut center_column = MemoryMappedFile::open(
            CENTER_COLUMN_FILE,
            Self::center_column_file_size(state.step),
        )
        .unwrap();

        if state.step == 1 {
            // Initial state
            current_row.get_mut()[0] = 1;
            center_column[0] = 1;
        }

        let neurons = thread::scope(|scope| {
            neurons
                .into_iter()
                .map(|info| {
                    scope.spawn(|| {
                        let state_info = &state.key_info.get(info.uid.0 as usize);

                        if state_info.is_some() && state_info.unwrap().hotkey == info.hotkey {
                            NeuronData {
                                score: state_info.unwrap().score.into(),
                                connection: connect_to_miner(
                                    &signer,
                                    neuron_info.uid.0,
                                    &info,
                                    matches!(state_info.unwrap().score, Score::Cheater),
                                    &metrics,
                                ).into(),
                                info,
                            }
                        } else {
                            NeuronData {
                                score: Score::default().into(),
                                connection: connect_to_miner(
                                    &signer,
                                    neuron_info.uid.0,
                                    &info,
                                    false,
                                    &metrics,
                                ).into(),
                                info,
                            }
                        }
                    })
                })
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        let mut neurons = Box::pin(neurons);

        let (work_queue_sender, work_queue_receiver) = mpmc::channel();
        let (completion_sender, completion_receiver) = mpmc::channel();

        let worker_count = worker_count_hint(&mut neurons);

        info!("Spawning {worker_count} worker threads");

        let worker_threads = (0..worker_count)
            .map(|_| {
                let current_row =
                    unsafe { transmute::<&CurrentRow, &'static CurrentRow>(&current_row) };
                let neurons =
                    unsafe { transmute::<&Vec<NeuronData>, &'static Vec<NeuronData>>(&neurons) };
                let signer = signer.clone();
                let work_queue_receiver = work_queue_receiver.clone();
                let completion_sender = completion_sender.clone();
                let metrics = metrics.clone();

                thread::spawn(move || {
                    do_work(
                        current_row,
                        neurons,
                        signer,
                        work_queue_receiver,
                        completion_sender,
                        metrics,
                    )
                })
            })
            .collect();

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

            work_queue_sender,
            work_queue_receiver,
            completion_sender,
            completion_receiver,

            worker_threads,

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
                        connection: connect_to_miner(
                            &self.signer,
                            self.uid,
                            &info,
                            false,
                            &self.metrics,
                        )
                        .into(),
                        info,
                    };
                } else {
                    if matches!(
                        neuron.connection.get_mut().expect("Lock poisoned"),
                        ConnectionState::Disconnected
                    ) || neurons[i].axon_info != neuron.info.axon_info
                    {
                        neuron.connection = connect_to_miner(
                            &self.signer,
                            self.uid,
                            &neurons[i],
                            matches!(
                                *neuron.score.get_mut().expect("Lock poisoned"),
                                Score::Cheater
                            ),
                            &self.metrics,
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
                        connection: connect_to_miner(
                            &self.signer,
                            self.uid,
                            &info,
                            false,
                            &self.metrics,
                        )
                        .into(),
                        info,
                    }
                });
            }

            let worker_count = worker_count_hint(&mut self.neurons);

            if self.worker_threads.len() < worker_count {
                let extra_workers = worker_count - self.worker_threads.len();

                info!("Spawning {extra_workers} extra worker threads");

                for _ in 0..extra_workers {
                    let current_row = unsafe { transmute::<&CurrentRow, &'static CurrentRow>(&self.current_row) };
                    let neurons = unsafe { transmute::<&Vec<NeuronData>, &'static Vec<NeuronData>>(&self.neurons) };
                    let signer = self.signer.clone();
                    let work_queue_receiver = self.work_queue_receiver.clone();
                    let completion_sender = self.completion_sender.clone();
                    let metrics = self.metrics.clone();

                    self.worker_threads.push(thread::spawn(move || {
                        do_work(
                            current_row,
                            neurons,
                            signer,
                            work_queue_receiver,
                            completion_sender,
                            metrics,
                        )
                    }))
                }
            }
        }

        self.metrics
            .sync_duration
            .record(start.elapsed().as_secs_f64(), &[]);

        Ok(set_weights)
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

        let connection_count = worker_count_hint(&mut self.neurons) as u64;

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

        self.current_row.ensure_capacity(Self::current_row_file_size(self.step))?;

        self.center_column
            .ensure_capacity(Self::center_column_file_size(self.step))?;

        info!("Splitting work into {connection_count} chunks");

        let byte_count = (self.step * 2 + 3).div_ceil(8);

        let chunk_size = byte_count.div_ceil(connection_count);

        for index in 0..connection_count {
            let start = index * chunk_size;
            let end = min((index + 1) * chunk_size, byte_count);

            let range = start..end;

            debug!("Adding {range:?} to work queue");

            self.work_queue_sender
                .send(range)
                .expect("Work queue channel should not be closed");

            if end == byte_count {
                break;
            }
        }

        handle_completion_events(
            &self.neurons,
            self.completion_receiver.clone(),
            self.work_queue_sender.clone(),
            byte_count,
            self.metrics.clone(),
        )
        .await;

        for i in 0..connection_count - 1 {
            let end = ((i + 1) * chunk_size) as usize;

            if end >= byte_count as usize {
                break;
            }

            let [a, b] = self.current_row.get_mut()[end..end + 1] else {
                break;
            };

            let (a, b) = normalize_pair(a, b);

            self.current_row.get_mut()[end..end + 1].copy_from_slice(&[a, b]);
        }

        let bit_index = self.step % 8;
        let part = self.step / 8;
        let current_row_part = (self.current_row.get_mut()[part as usize] >> bit_index) & 0b1;

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
}
