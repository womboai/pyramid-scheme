use std::{fs, thread};
use std::cell::{Cell, UnsafeCell};
use std::cmp::min;
use std::io::{Read, Write};
use std::mem::transmute;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::ops::{Deref, DerefMut, Range};
use std::path::{Path, PathBuf};
use std::random::random;
use std::sync::{Arc, mpmc};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing::{error, info, warn};

use neuron::{
    AccountId, config, hotkey_location, load_key_seed, NeuronInfoLite, Signer, signer_from_seed,
    Subtensor,
};
use neuron::auth::{KeyRegistrationInfo, sign_message, VerificationMessage};

use crate::validator::event_future::EventFuture;
use crate::validator::memory_storage::{MemoryMapped, MemoryMappedFile, MemoryMappedStorage};
use crate::validator::neuron_data::{NeuronData, Score, UnsafeCellImmutableBorrow, UnsafeSendRef};

mod memory_storage;
mod event_future;
mod neuron_data;

const VERSION_KEY: u64 = 1;
const GROW_BY: u64 = 1024 * 1024 * 8;
const VALIDATION_CHANCE: f32 = 0.05;

pub(crate) const STATE_DATA_FILE: &'static str = "state/data.json";
pub(crate) const CURRENT_ROW_FILE: &'static str = "state/current_row.bin";
pub(crate) const CENTER_COLUMN_FILE: &'static str = "state/center_column.bin";

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
    score: Score,
    hotkey: AccountId,
}

#[derive(Debug, Serialize, Deserialize)]
struct ValidatorState {
    step: u64,
    key_info: Vec<KeyScoreInfo>,
}

impl ValidatorState {
    fn for_hotkeys(hotkeys: impl Iterator<Item=AccountId>) -> Self {
        let key_info = hotkeys
            .map(|hotkey| KeyScoreInfo {
                hotkey,
                score: Score::default(),
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

pub struct Validator {
    signer: Signer,
    subtensor: Subtensor,
    neurons: Vec<NeuronData>,
    pub(crate) uid: u16,

    current_row: CurrentRow,
    center_column: MemoryMappedFile,
    step: u64,

    last_metagraph_sync: u64,
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

        let neuron_info = if let Some(neuron_info) = neuron_info {
            neuron_info.clone()
        } else {
            Self::not_registered(signer.account_id());
        };

        let state = Self::load_state(neurons.iter().map(|neuron| neuron.hotkey.clone())).unwrap();

        fs::create_dir_all("state").unwrap();

        let mut current_row = CurrentRow::open(
            CURRENT_ROW_FILE,
            Self::current_row_file_size(state.step),
        )
            .unwrap();

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
                let state_info = &state.key_info[info.uid.0 as usize];

                if state_info.hotkey == info.hotkey {
                    NeuronData {
                        score: state_info.score.into(),
                        connection: Self::connect_to_miner(&signer, neuron_info.uid.0, &info, matches!(state_info.score, Score::Cheater)).into(),
                        info,
                    }
                } else {
                    NeuronData {
                        score: Score::default().into(),
                        connection: Self::connect_to_miner(&signer, neuron_info.uid.0, &info, false).into(),
                        info,
                    }
                }
            })
            .collect::<Vec<_>>();

        // Set weights if enough time has passed
        if last_metagraph_sync - neuron_info.last_update.0 >= *config::EPOCH_LENGTH {
            if let Err(e) = Self::set_weights(&neurons, &subtensor, &signer).await {
                println!("Failed to set weights. {e}");
            }
        }

        Self {
            signer,
            subtensor,
            neurons,
            uid: neuron_info.uid.0,
            current_row,
            center_column,
            step: state.step,
            last_metagraph_sync,
        }
    }

    fn save_state(&self) -> Result<()> {
        let path = PathBuf::from(STATE_DATA_FILE);

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
                    score: data.score.get(),
                })
                .collect(),
            step: self.step,
        };

        let json = serde_json::to_string(&state)?;

        fs::write(&path, json)?;

        Ok(())
    }

    fn load_state(hotkeys: impl Iterator<Item=AccountId>) -> Result<ValidatorState> {
        info!("Loading state");

        let path = PathBuf::from(STATE_DATA_FILE);

        if !path.exists() {
            return Ok(ValidatorState::for_hotkeys(hotkeys));
        }

        let json = fs::read_to_string(&path)?;

        Ok(serde_json::from_str(&json)?)
    }

    async fn set_weights(neurons: &[NeuronData], subtensor: &Subtensor, signer: &Signer) -> Result<()> {
        if neurons.is_empty() {
            return Ok(());
        }

        info!("Setting weights");

        let max_score: u128 = neurons
            .iter()
            .map(|info| info.score.get().into())
            .max()
            .unwrap();

        let scores = neurons.iter().map(|neuron| {
            let normalized_score = match neuron.score.get() {
                Score::Legitimate(score) => if max_score == 0 {
                    u16::MAX
                } else {
                    ((score * u16::MAX as u128) / max_score) as u16
                },
                Score::Cheater => 0,
            };

            (neuron.info.uid.0, normalized_score)
        });

        subtensor
            .set_weights(&signer, *config::NETUID, scores, VERSION_KEY)
            .await?;

        Ok(())
    }

    async fn sync(&mut self, block: Option<u64>) -> Result<()> {
        info!("Syncing metagraph and connecting to miners");

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
            let neuron = &mut self.neurons[i];

            if neuron.info.hotkey != neurons[i].hotkey {
                let info = neurons[i].clone();

                self.neurons[i] = NeuronData {
                    score: Score::default().into(),
                    connection: Self::connect_to_miner(&self.signer, self.uid, &info, false).into(),
                    info,
                };
            } else {
                if neuron.connection.as_ref().is_none() || neurons[i].axon_info != neuron.info.axon_info {
                    neuron.connection = Self::connect_to_miner(&self.signer, self.uid, &neurons[i], matches!(neuron.score.get(), Score::Cheater)).into()
                }

                neuron.info = neurons[i].clone();
            }
        }

        // Update scores array size if needed
        if self.neurons.len() != neurons.len() {
            let mut uid_iterator = (self.neurons.len()..neurons.len()).into_iter();

            self.neurons.resize_with(neurons.len(), || {
                let info = neurons[uid_iterator.next().unwrap()].clone();

                NeuronData {
                    score: Score::default().into(),
                    connection: Self::connect_to_miner(&self.signer, self.uid, &info, false).into(),
                    info,
                }
            });
        }

        // Set weights if enough time has passed
        if block - neuron_info.last_update.0 >= *config::EPOCH_LENGTH {
            if let Err(e) = Self::set_weights(&self.neurons, &self.subtensor, &self.signer).await {
                error!("Failed to set weights. {e}");
            }
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
        score: UnsafeSendRef<Cell<Score>>,
        start: u64,
        end: u64,
        uid: u16,
        end_trigger: &mut EventFuture<(u64, bool)>,
    ) {
        let connection = connection_ref.as_mut().unwrap();

        let buffer_size = min(end - start, 8 * 4 * 256);
        let mut buffer = Vec::with_capacity(buffer_size as usize);

        unsafe {
            buffer.set_len(buffer_size as usize)
        }

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

        let mut added = 0;

        for i in 0..iterations {
            let from = (start + i * buffer_size) as usize;
            let to = (start + (i + 1) * buffer_size) as usize;

            while added as usize != to - from {
                let written = match connection.write(&current_row[from + added as usize..to]) {
                    Ok(len) => {
                        if len == 0 {
                            warn!(
                                "Failed to write to miner {uid} connection while there's more to process"
                            );

                            score.set(score.get() + added as u128);
                            end_trigger.complete((added, false));

                            return;
                        }

                        len
                    }
                    Err(error) => {
                        warn!("Error occurred writing to miner {uid}: {error}");

                        score.set(score.get() + added as u128);
                        end_trigger.complete((added, false));

                        return;
                    }
                };

                let mut read = 0;

                while read < written {
                    let range = from + read + added as usize..from + added as usize + written + read;

                    let should_validate = if let Some(validation_start_index) =
                        validation_start_index
                    {
                        if range.contains(&(validation_start_index as usize)) {
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    let len = match connection.read(&mut buffer) {
                        Ok(len) => {
                            if len == 0 {
                                warn!("Failed to read from miner {uid} connection");

                                score.set(score.get() + added as u128);
                                end_trigger.complete((added, false));

                                return;
                            }

                            len
                        }
                        Err(error) => {
                            warn!("Error occurred reading from miner {uid}: {error}");

                            score.set(score.get() + added as u128);
                            end_trigger.complete((added, false));

                            return;
                        }
                    };

                    if should_validate {
                        info!("Verifying results of {uid}");

                        if !Self::verify_result(&current_row[range.start..range.start + len], &buffer[..len]) {
                            info!("{uid} marked as cheater");

                            score.set(Score::Cheater);
                            *connection_ref = None;

                            end_trigger.complete((0, false));

                            return;
                        }
                    }

                    (&mut current_row[range.start..range.start + len]).copy_from_slice(&buffer[..len]);
                    read += len;
                }

                added += written as u64;
            }
        }

        score.set(score.get() + added as u128);
        end_trigger.complete((added, true));
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

        match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            Ok(mut stream) => {
                if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
                    warn!("Not continuing connection to uid {uid} at {address} because could not configure read timeout, {e}", uid = neuron.uid.0);

                    return None;
                }

                if let Err(e) = stream.set_write_timeout(Some(Duration::from_secs(5))) {
                    warn!("Not continuing connection to uid {uid} at {address} because could not configure write timeout, {e}", uid = neuron.uid.0);

                    return None;
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

                    return None;
                };

                if let Err(e) = stream.write(&signature) {
                    warn!("Failed to write to miner {uid}, {e}", uid = neuron.uid.0);

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
                if matches!(neuron.score.get(), Score::Cheater) {
                    return None;
                }

                if neuron.connection.as_ref().is_none() {
                    None
                } else {
                    Some((uid as u16, transmute(neuron.connection.get_mut())))
                }
            })
            .collect()
    }

    async fn handle_connection_events(neurons: &'static [NeuronData], completion_events: Vec<(u16, Range<u64>, UnsafeCell<EventFuture<(u64, bool)>>)>, current_row: CurrentRow) {
        let (unfinished_sender, unfinished_receiver) = mpmc::channel();
        let (finished_sender, finished_receiver) = mpmc::channel();

        JoinSet::from_iter(completion_events.into_iter().map(|(uid, range, future)| {
            let unfinished_sender = unfinished_sender.clone();
            let finished_sender = finished_sender.clone();
            let unfinished_receiver = unfinished_receiver.clone();
            let finished_receiver = finished_receiver.clone();

            let neurons = UnsafeSendRef::from(neurons);
            let row = unsafe { current_row.share() };

            async move {
                let (written, connected) = future.into_inner().await;

                let required = range.end - range.start;

                if written < required {
                    loop {
                        match finished_receiver.try_recv() {
                            Ok(finished_uid) => {
                                let connection = unsafe { &mut *neurons[finished_uid as usize].connection.get() };

                                let score = UnsafeSendRef::from(&neurons[finished_uid as usize].score);

                                let mut event = EventFuture::new();

                                Validator::handle_connection(row, connection, score, range.start + written, range.end, finished_uid, &mut event);

                                event.await;

                                break;
                            }
                            Err(_) => {
                                unfinished_sender.send(range.start + written..range.end).unwrap();
                                break;
                            }
                        }
                    }
                } else {
                    if !connected {
                        return;
                    }

                    match unfinished_receiver.try_recv() {
                        Ok(unfinished_range) => {
                            let connection = unsafe { &mut *neurons[uid as usize].connection.get() };

                            let score = UnsafeSendRef::from(&neurons[uid as usize].score);

                            let mut event = EventFuture::new();

                            Validator::handle_connection(row, connection, score, unfinished_range.start, unfinished_range.end, uid, &mut event);

                            event.await;
                        }
                        Err(_) => {
                            finished_sender.send(uid).unwrap();
                        }
                    }
                }
            }
        })).join_all().await;
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
            self.sync(None).await?;
            elapsed_blocks = 0;
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

        let byte_count = (self.step * 2 + 3).div_ceil(8);

        let chunk_size = byte_count.div_ceil(connection_count);
        let mut completion_events = Vec::with_capacity(connections.len());

        thread::scope(|scope| {
            let mut handles = Vec::with_capacity(connections.len());

            for (index, (uid, connection)) in connections.into_iter().enumerate() {
                unsafe {
                    // SAFETY: This is safe as the data read/written does not overlap between threads
                    let row = self.current_row.share();

                    let score = UnsafeSendRef::from(&self.neurons[uid as usize].score);

                    let start = index as u64 * chunk_size;
                    let end = min((index as u64 + 1) * chunk_size, byte_count);

                    if start == end {
                        break;
                    }

                    let event = UnsafeCell::new(EventFuture::<(u64, bool)>::new());
                    completion_events.push((uid, start..end, event));

                    // Get unsafe mutable reference, allows compiler to incorrectly assume inside other thread that it is the only reference that exists
                    // SAFETY: Safe because the other reference(from completion_events) simply awaits this event future, which is a Sync operation
                    let event = &mut *completion_events.last().unwrap().2.get();

                    let handle = scope.spawn(move || {
                        Self::handle_connection(row, connection, score, start, end, uid, event);
                    });

                    handles.push(handle);
                }
            }

            tokio::task::spawn_local(
                Self::handle_connection_events(
                    unsafe { transmute::<&[NeuronData], &'static [NeuronData]>(&self.neurons) },
                    completion_events,
                    unsafe { self.current_row.share() },
                ),
            )
        }).await?;

        for i in 0..connection_count - 1 {
            let end = ((i + 1) * chunk_size) as usize;

            if end >= byte_count as usize {
                break
            }

            let [a, b] = self.current_row[end..end + 1] else {
                break
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
