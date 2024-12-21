use neuron::{config, subtensor, INTEGRAL_VERSION};
use rusttensor::{AccountId, Block, BlockNumber};

use crate::validator::metrics::ValidatorMetrics;
use crate::validator::neuron_data::{ConnectionState, NeuronData, Score};

use crate::validator::completion_event_handler::handle_completion_events;
use crate::validator::connection::{connect_to_miner, worker_connections, worker_count_hint};
use crate::validator::worker::{do_work, ProcessingCompletionResult, ProcessingRequest};
use anyhow::Result;
use rusttensor::api::apis;
use rusttensor::rpc::call_runtime_api_decoded;
use rusttensor::rpc::types::NeuronInfoLite;
use rusttensor::subtensor::Subtensor;
use rusttensor::wallet::Signer;
use rusttensor::weights::{set_weights_payload, NormalizedWeight};
use serde::{Deserialize, Serialize};
use std::cell::SyncUnsafeCell;
use std::cmp::min;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::mem::transmute;
use std::net::TcpStream;
use std::num::NonZeroU8;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpmc::{Receiver, Sender};
use std::sync::{mpmc, Arc};
use std::time::{Duration, Instant};
use std::{fs, mem, thread};
use tracing::log::warn;
use tracing::{debug, error, info};

mod completion_event_handler;
mod connection;
pub mod metrics;
mod neuron_data;
mod worker;

const GROW_BY: u64 = 1024 * 1024 * 8;
const DATA_SPEC_VERSION: u32 = 4;
const SCORE_SPEC_VERSION: u32 = 1;

pub(crate) const STATE_DATA_FILE: &'static str = "state/data.json";
pub(crate) const CURRENT_ROW_FILE: &'static str = "state/current_row.bin";
pub(crate) const CENTER_COLUMN_FILE: &'static str = "state/center_column.bin";

#[derive(Debug, Serialize, Deserialize)]
struct KeyScoreInfo {
    score: Score,
    #[serde(default = "default_weight")]
    weight: NonZeroU8,
    hotkey: AccountId,
}

const fn default_weight() -> NonZeroU8 {
    NonZeroU8::MAX
}

const fn default_version() -> u32 {
    0
}

#[derive(Debug, Serialize, Deserialize)]
struct ValidatorState {
    step: u64,
    key_info: Vec<KeyScoreInfo>,

    #[serde(default = "default_version")]
    version: u32,

    #[serde(default = "default_version")]
    score_version: u32,
}

impl ValidatorState {
    fn for_hotkeys(hotkeys: impl Iterator<Item = AccountId>) -> Self {
        let key_info = hotkeys
            .map(|hotkey| KeyScoreInfo {
                hotkey,
                score: Score::default(),
                weight: NonZeroU8::MAX,
            })
            .collect();

        Self {
            step: 1,
            key_info,
            version: DATA_SPEC_VERSION,
            score_version: SCORE_SPEC_VERSION,
        }
    }
}

impl Default for ValidatorState {
    fn default() -> Self {
        Self {
            step: 1,
            key_info: Vec::new(),
            version: DATA_SPEC_VERSION,
            score_version: SCORE_SPEC_VERSION,
        }
    }
}

pub struct Validator {
    signer: Signer,
    subtensor: Subtensor,
    neurons: Pin<Box<Vec<NeuronData>>>,
    pub(crate) uid: u16,

    current_row: Pin<Box<SyncUnsafeCell<Vec<u8>>>>,
    center_column: Vec<u8>,
    step: u64,

    current_block: Block,
    last_block_fetch: Instant,
    attempted_set_weights: bool,

    work_queue_sender: Sender<ProcessingRequest>,
    work_queue_receiver: Receiver<ProcessingRequest>,

    available_worker_sender: Sender<(u16, &'static mut TcpStream)>,
    available_worker_receiver: Receiver<(u16, &'static mut TcpStream)>,

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
        (Self::step_to_grow_to(step) * 2 - 1).div_ceil(u8::BITS as u64)
    }

    fn center_column_file_size(step: u64) -> u64 {
        Self::step_to_grow_to(step).div_ceil(u8::BITS as u64)
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

        fs::create_dir_all("state").unwrap();

        if state.version != DATA_SPEC_VERSION {
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
        } else if state.score_version != SCORE_SPEC_VERSION {
            state.key_info.clear();
        }

        let current_row_size = Self::current_row_file_size(state.step) as usize;
        let center_column_size = Self::center_column_file_size(state.step) as usize;

        let mut current_row = if fs::exists(CURRENT_ROW_FILE).unwrap() {
            let mut current_row = Vec::with_capacity(current_row_size);

            let mut current_row_file = File::open(CURRENT_ROW_FILE).unwrap();

            current_row_file.read_to_end(&mut current_row).unwrap();

            current_row
        } else {
            vec![0; current_row_size]
        };

        let mut center_column = if fs::exists(CENTER_COLUMN_FILE).unwrap() {
            let mut current_row = Vec::with_capacity(center_column_size);

            let mut current_row_file = File::open(CENTER_COLUMN_FILE).unwrap();

            current_row_file.read_to_end(&mut current_row).unwrap();

            current_row
        } else {
            vec![0; center_column_size]
        };

        if state.step == 1 {
            // Initial state
            current_row[0] = 1;
            center_column[0] = 1;
        }

        let current_row = Box::pin(current_row.into());

        let neurons = thread::scope(|scope| {
            let handles = neurons
                .into_iter()
                .map(|info| {
                    scope.spawn(|| {
                        let state_info = &state.key_info.get(info.uid.0 as usize);

                        if state_info.is_some() && state_info.unwrap().hotkey == info.hotkey {
                            NeuronData {
                                score: state_info.unwrap().score.into(),
                                weight: state_info.unwrap().weight,
                                connection: connect_to_miner(
                                    &signer,
                                    neuron_info.uid.0,
                                    &info,
                                    matches!(state_info.unwrap().score, Score::Cheater),
                                    &metrics,
                                )
                                .into(),
                                info,
                            }
                        } else {
                            NeuronData {
                                score: Score::default().into(),
                                weight: NonZeroU8::MAX,
                                connection: connect_to_miner(
                                    &signer,
                                    neuron_info.uid.0,
                                    &info,
                                    false,
                                    &metrics,
                                )
                                .into(),
                                info,
                            }
                        }
                    })
                })
                .collect::<Vec<_>>();

            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        let mut neurons = Box::pin(neurons);

        let (work_queue_sender, work_queue_receiver) = mpmc::channel();
        let (available_worker_sender, available_worker_receiver) = mpmc::channel();
        let (completion_sender, completion_receiver) = mpmc::channel();

        let worker_count = worker_count_hint(&mut neurons);

        info!("Spawning {worker_count} worker threads");

        let worker_threads = (0..worker_count)
            .map(|_| {
                let current_row = unsafe {
                    transmute::<&SyncUnsafeCell<Vec<u8>>, &'static SyncUnsafeCell<Vec<u8>>>(
                        &current_row,
                    )
                };

                let available_worker_receiver = available_worker_receiver.clone();
                let work_queue_receiver = work_queue_receiver.clone();
                let completion_sender = completion_sender.clone();

                thread::spawn(move || {
                    do_work(
                        current_row,
                        available_worker_receiver,
                        work_queue_receiver,
                        completion_sender,
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

            available_worker_sender,
            available_worker_receiver,

            completion_sender,
            completion_receiver,

            worker_threads,

            metrics,
        }
    }

    fn save_state(&mut self) -> Result<()> {
        let path = PathBuf::from(STATE_DATA_FILE);

        {
            let mut file = File::create(CURRENT_ROW_FILE)?;

            file.write_all(self.current_row.get_mut())?;
        }

        {
            let mut file = File::create(CENTER_COLUMN_FILE)?;

            file.write_all(&self.center_column)?;
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let state = ValidatorState {
            key_info: self
                .neurons
                .iter_mut()
                .map(|data| KeyScoreInfo {
                    hotkey: data.info.hotkey.clone(),
                    score: *data.score.get_mut(),
                    weight: data.weight,
                })
                .collect(),
            step: self.step,
            version: DATA_SPEC_VERSION,
            score_version: SCORE_SPEC_VERSION,
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
            .map(|info| u128::from(*info.score.get_mut()))
            .max()
            .expect("Neurons should never be empty at this point, thus max should never be None");

        let scores = neurons
            .iter_mut()
            .map(|neuron| {
                let normalized_score = match *neuron.score.get_mut() {
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

        let payload = set_weights_payload(*config::NETUID, scores, *INTEGRAL_VERSION);

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

            self.save_state()?;

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
                        weight: NonZeroU8::MAX,
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
                    if matches!(neuron.connection.get_mut(), ConnectionState::Disconnected)
                        || neurons[i].axon_info != neuron.info.axon_info
                    {
                        neuron.connection = connect_to_miner(
                            &self.signer,
                            self.uid,
                            &neurons[i],
                            matches!(*neuron.score.get_mut(), Score::Cheater),
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
                        weight: NonZeroU8::MAX,
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
                    let current_row = unsafe {
                        transmute::<&SyncUnsafeCell<Vec<u8>>, &'static SyncUnsafeCell<Vec<u8>>>(
                            &self.current_row,
                        )
                    };

                    let available_worker_receiver = self.available_worker_receiver.clone();
                    let work_queue_receiver = self.work_queue_receiver.clone();
                    let completion_sender = self.completion_sender.clone();

                    self.worker_threads.push(thread::spawn(move || {
                        do_work(
                            current_row,
                            available_worker_receiver,
                            work_queue_receiver,
                            completion_sender,
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

        let mut elapsed_blocks = self.current_block.number()
            - self.neurons[self.uid as usize].info.last_update.0 as BlockNumber;

        let should_sleep = if !self.attempted_set_weights {
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

        let worker_connections = worker_connections(&mut self.neurons);

        if worker_connections.is_empty() {
            if should_sleep {
                let blocks = *config::EPOCH_LENGTH - elapsed_blocks;

                warn!("No connections available, sleeping for {blocks} to resync");

                tokio::time::sleep(Duration::from_secs(blocks as u64 * 12)).await;
            } else {
                warn!("No connections available, retrying");
            }

            return Ok(());
        }

        {
            let current_row = self.current_row.get_mut();
            let new_length = Self::current_row_file_size(self.step);

            if current_row.len() < new_length as usize {
                current_row.resize(new_length as usize, 0);
            }
        }

        {
            let new_length = Self::center_column_file_size(self.step);

            if self.center_column.len() < new_length as usize {
                self.center_column.resize(new_length as usize, 0);
            }
        }

        debug!(
            "Splitting work into {} [+1] chunks",
            worker_connections.len()
        );

        let byte_count = (self.step * 2 + 3).div_ceil(8);

        let weight_sum: u32 = worker_connections.iter().map(|x| x.weight as u32).sum();

        let mut position = 0;

        let mut worker_connections = worker_connections.into_iter();

        for connection in &mut worker_connections {
            let chunk_size = (byte_count * connection.weight as u64) / weight_sum as u64;

            if chunk_size == 0 {
                continue;
            }

            let cache_line_size_remainder = chunk_size % 64;

            let chunk_size = if cache_line_size_remainder == 0 {
                chunk_size
            } else {
                chunk_size - cache_line_size_remainder + 64
            };

            let end = min(position + chunk_size, byte_count);
            let range = position..end;

            debug!("Adding {range:?} to work queue");

            self.work_queue_sender
                .send(ProcessingRequest::new(range, self.current_row.get_mut()))
                .expect("Work queue channel should not be closed");

            self.available_worker_sender
                .send((connection.uid, connection.stream))
                .expect("Available worker channel should not be closed");

            position = end;

            if end == byte_count {
                break;
            }
        }

        for connection in worker_connections {
            self.available_worker_sender
                .send((connection.uid, connection.stream))
                .expect("Available worker channel should not be closed");
        }

        if position != byte_count {
            debug!(
                "Adding {range:?} to work queue",
                range = position..byte_count
            );

            self.work_queue_sender
                .send(ProcessingRequest::new(
                    position..byte_count,
                    self.current_row.get_mut(),
                ))
                .expect("Work queue channel should not be closed");
        }

        let weights = handle_completion_events(
            &self.neurons,
            &self.available_worker_sender,
            &self.completion_receiver,
            &self.work_queue_sender,
            byte_count,
            &self.metrics,
        )
        .await;

        while let Ok(_) = self.available_worker_receiver.try_recv() {
            // Clear available worker queue as to not pollute the next step
        }

        for (uid, weight) in weights.into_iter().enumerate() {
            let weight = NonZeroU8::new(weight).or(NonZeroU8::new(1)).unwrap();

            self.neurons[uid].weight = weight;
        }

        let bit_index = self.step % u8::BITS as u64;
        let part = self.step / u8::BITS as u64;
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
                        error!("Failed to fetch block, {e}");
                        info!("Restarting subtensor client");

                        self.subtensor = subtensor().await.unwrap();

                        tokio::time::sleep(Duration::from_secs(12)).await;

                        return;
                    }
                };

                self.current_block = block;
                self.last_block_fetch = Instant::now();
                self.attempted_set_weights = false;

                let elapsed_blocks = self.current_block.number()
                    - self.neurons[self.uid as usize].info.last_update.0 as BlockNumber;

                info!("Evolution step {}", self.step);

                info!(
                    "Current block is {block}, it has been {elapsed_blocks} blocks since last update",
                    block = self.current_block.number(),
                );
            }

            if let Err(e) = self.do_step().await {
                error!("Error during evolution step {step}, {e}", step = self.step);
            }
        }
    }
}

impl Drop for Validator {
    fn drop(&mut self) {
        let threads = mem::take(&mut self.worker_threads);

        for handle in threads.into_iter() {
            if let Err(e) = handle.join() {
                warn!("Failed to join a worker thread, {e:?}")
            }
        }
    }
}
