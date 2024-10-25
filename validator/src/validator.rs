use std::fs;
use std::fs::File;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::simd::Simd;

use anyhow::Result;
use memmap2::{MmapMut, MmapOptions};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::time::{Duration, sleep};
use tracing::{error, info};

use neuron::{AccountId, config, NeuronInfoLite, Subtensor};
use substrate_interface::Keypair;

use crate::models::ComputationData;

const VERSION_KEY: u64 = 1;

struct MemoryMappedFile {
    mmap: MmapMut,
    file: File,
    path: PathBuf,
}

impl MemoryMappedFile {
    fn new(file: File, path: impl AsRef<Path>) -> Self {
        // SAFETY: This would typically not be safe as this is technically a self-referential
        //  struct. Mmap is using a reference of `&file` without a lifetime. However this works as
        //  mmap uses the file descriptor internally, so even if {File} is moved, the descriptor
        //  remains the same, and the file descriptor is closed at the same time the mmap is closed.
        let mmap = unsafe { MmapOptions::new().map_mut(&file) }.unwrap();

        Self {
            mmap,
            file,
            path: path.as_ref().to_path_buf(),
        }
    }
}

struct MemoryMappedStorage {
    temporary_file: MemoryMappedFile,
    storage_path: PathBuf,
}

impl MemoryMappedStorage {
    fn new(storage_path: impl AsRef<Path>) -> Self {
        let temporary_file_path = NamedTempFile::new().unwrap();

        fs::rename(&storage_path, &temporary_file_path);

        let memored_mapped_temporary_file = MemoryMappedFile::new(temporary_file_path.reopen().unwrap(), temporary_file_path.path().to_owned());

        Self {
            temporary_file: memored_mapped_temporary_file,
            storage_path: storage_path.as_ref().to_path_buf(),

        }
    }

    fn flush(&self) -> Result<()> {
        self.temporary_file.mmap.flush()?;
        fs::rename(&self.temporary_file.path, &self.storage_path)?;

        Ok(())
    }
}

impl Deref for MemoryMappedStorage {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.temporary_file.mmap
    }
}

impl DerefMut for MemoryMappedStorage {

    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.temporary_file.mmap
    }
}

impl Drop for MemoryMappedStorage {
    fn drop(&mut self) {
        // TODO Handle drop errors
        self.flush().unwrap();
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ValidatorState {
    step: u64,
    hotkeys: Vec<AccountId>,
    scores: Vec<u16>,
}

pub struct Validator {
    keypair: Keypair,
    subtensor: Subtensor,
    neurons: Vec<NeuronInfoLite>,
    uid: u16,
    client: Client,

    current_row: MemoryMappedStorage,
    center_column: MemoryMappedStorage,
    state: ValidatorState,

    last_metagraph_sync: u64,
}

impl Validator {
    fn find_neuron_info(neurons: &[NeuronInfoLite], account_id: AccountId) -> Option<&NeuronInfoLite> {
        neurons
            .iter()
            .find(|neuron| neuron.hotkey == account_id)
    }

    fn not_registered(account_id: AccountId) -> ! {
        panic!("Hotkey {account_id} is not registered in {}", *config::NETUID);
    }

    pub async fn new() -> Self {
        let keypair = load_hotkey_keypair(
            &config::WALLET_NAME,
            &config::HOTKEY_NAME,
        ).unwrap();

        let subtensor = Subtensor::new(&*config::CHAIN_ENDPOINT).await.unwrap();

        let neurons: Vec<NeuronInfoLite> = subtensor.get_neurons(*config::NETUID).await.unwrap();

        let neuron_info = Self::find_neuron_info(&neurons, keypair.account_id());

        let uid = if let Some(neuron_info) = neuron_info {
            neuron_info.uid.0
        } else {
            Self::not_registered(keypair.account_id());
        };

        let hotkeys: Vec<String> = neurons.map(|neuron| neuron.hotkey);
        let scores = vec![0; hotkeys.len()];

        let state = ValidatorState {
            step: 1,
            scores,
            hotkeys,
        };

        let current_row = MemoryMappedStorage::new("current_row.bin");
        let center_column = MemoryMappedStorage::new("center_column.bin");

        let mut validator = Self {
            keypair,
            subtensor,
            neurons,
            uid,
            client: Client::new(),
            current_row,
            center_column,
            state,
            last_metagraph_sync: 0,
        };

        validator.load_state().unwrap();

        validator
    }

    fn state_path(&self) -> PathBuf {
        let home = dirs::home_dir().expect("Could not find home directory");
        home.join(".bittensor")
            .join("miners")
            .join(&config::WALLET_NAME)
            .join(&config::HOTKEY_NAME)
            .join(format!("netuid{}", *config::NETUID))
            .join("validator")
            .join("state.json")
    }

    fn save_state(&self) -> Result<()> {
        let path = self.state_path();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string(&self.state)?;

        fs::write(&path, json)?;

        Ok(())
    }

    fn load_state(&mut self) -> Result<()> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(());
        }

        let json = fs::read_to_string(&path)?;

        self.state = serde_json::from_str(&json)?;

        Ok(())
    }

    async fn sync(&mut self, block: Option<u64>) -> Result<()> {
        self.neurons = self.subtensor.get_neurons(*config::NETUID).await?;

        let block = if let Some(block) = block {
            block
        } else {
            self.subtensor.get_block_number().await?
        };

        self.last_metagraph_sync = block;

        let neuron_info = Self::find_neuron_info(&self.neurons, self.keypair.account_id());

        let neuron_info = if let Some(neuron_info) = neuron_info {
            neuron_info
        } else {
            Self::not_registered(self.keypair.account_id());
        };

        self.uid = neuron_info.uid.0;

        // Update scores array size if needed
        if self.state.hotkeys.len() != self.neurons.len() {
            let mut new_scores = vec![0; self.neurons.len()];
            new_scores[..self.state.scores.len()].copy_from_slice(&self.state.scores);
            self.state.scores = new_scores;
        }

        // Set weights if enough time has passed
        if block - neuron_info.last_update.0 >= *config::EPOCH_LENGTH {
            self.subtensor.set_weights(*config::NETUID, self.state.scores.iter().enumerate().map(|(uid, &score)| (uid as u16, score)).collect(), VERSION_KEY).await?;
        }

        Ok(())
    }

    async fn do_step(&mut self) -> Result<()> {
        info!("Evolution step {}", self.state.step);

        let current_block = self.subtensor.get_block_number().await?;
        let elapsed_blocks = current_block - self.last_metagraph_sync;

        if elapsed_blocks >= *config::EPOCH_LENGTH {
            self.sync(Some(current_block)).await?;
        }

        // Wait for miners if needed
        while self.valid_miners.is_empty() {
            info!("Not enough miners to compute step, waiting until next sync");
            sleep(Duration::from_secs((*config::EPOCH_LENGTH - elapsed_blocks) as u64)).await;
            self.sync(None).await?;
        }

        // Distribute work among miners
        let chunk_size = (self.current_row.len() as f64 / self.valid_miners.len() as f64).ceil() as usize;
        let mut chunks = Vec::new();
        let mut current_pos = 0;

        for &uid in &self.valid_miners {
            let end = (current_pos + chunk_size).min(self.current_row.len());
            let chunk = self.current_row.slice(s![current_pos..end]).to_vec();
            chunks.push((uid, chunk));
            current_pos = end;
            if current_pos >= self.current_row.len() {
                break;
            }
        }

        // Make concurrent requests
        let futures: Vec<_> = chunks.into_iter().map(|(uid, chunk)| {
            let data = ComputationData { parts: chunk };
            async move {
                let node = &self.neurons[uid];
                let result = self.make_request(node, Some(&data)).await;
                (uid, result)
            }
        }).collect();

        let results = futures::future::join_all(futures).await;

        // Process results
        let mut responses = Vec::new();
        for (uid, result) in results {
            match result {
                Ok((response, inference_time)) => {
                    if let Ok(data) = response.json::<ComputationData>().await {
                        self.state.scores[uid as usize] = (u16::MAX / inference_time) as u16;
                        responses.push(data.parts);
                    } else {
                        self.state.scores[uid as usize] = 0;
                    }
                }
                Err(_) => {
                    self.state.scores[uid as usize] = 0;
                }
            }
        }

        // Update state
        if !responses.is_empty() {
            let mut simd_data: Vec<_> = responses.into_iter()
                .map(|v| Simd::from_array(v.try_into().unwrap()))
                .collect();

            let normalized = Self::normalize_response_data(&mut simd_data);
            self.current_row = Array1::from_vec(normalized.into_iter().map(|x| x as u64).collect());

            let bit_index = self.state.step % 64;
            let row_part = self.current_row[self.state.step as usize / 64];

            self.center_column[self.center_column.len() - 1] |=
                (row_part >> bit_index) << bit_index;
        }

        self.state.step += 1;
        self.save_state()?;

        Ok(())
    }

    pub(crate) async fn run(&mut self) {
        loop {
            if let Err(e) = self.do_step().await {
                error!("Error during evolution step {step}, {e}", step=self.state.step);
            }
        }
    }

    fn normalize_response_data(lists: &mut [Simd<u8, 32>]) -> Vec<u8> {
        fn rule_30(a: u64) -> u64 {
            a ^ ((a << 1) | (a << 2))
        }

        fn normalize_pair(a: u8, b: u8) -> (u8, u8) {
            // Convert u8 to u64 for processing
            let (new_a, new_b) = {
                let a = a as u64;
                let b = b as u64;
                let carry = a & 1;
                let mut a = a >> 1;
                let mut b = (carry << 63) | b;
                a = rule_30(a);
                b = rule_30(b);
                let msb = b >> 63;
                b &= (1 << 63) - 1;
                a = (a << 1) | msb;
                (a as u8, b as u8)  // Convert back to u8
            };
            (new_a, new_b)
        }

        let mut normalized_outputs = Vec::new();

        // Process lists
        for i in 0..lists.len() - 1 {
            let mut current_list = lists[i].to_array();
            let mut next_list = lists[i + 1].to_array();

            let (new_last, new_first) = normalize_pair(
                current_list[31],  // last element of current list
                next_list[0],       // first element of next list
            );

            current_list[31] = new_last;
            next_list[0] = new_first;

            // Update the lists with normalized values
            lists[i] = Simd::from_array(current_list);
            lists[i + 1] = Simd::from_array(next_list);

            // Extend normalized_outputs with current list
            normalized_outputs.extend_from_slice(&current_list);
        }

        // Add the last list if there was more than one list
        if lists.len() > 1 {
            normalized_outputs.extend_from_slice(&lists[lists.len() - 1].to_array());
        }

        normalized_outputs
    }
}
