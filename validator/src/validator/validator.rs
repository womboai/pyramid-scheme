use std::path::{Path, PathBuf};
use std::time::Instant;
use std::fs;
use tokio::time::{sleep, Duration};
use serde::{Serialize, Deserialize};
use ndarray::Array1;
use reqwest::Client;
use substrate_interface::{SubstrateInterface, Keypair};
use crate::models::ComputationData;
use crate::config::{self, EPOCH_LENGTH, WANDB_REFRESH_INTERVAL};

#[derive(Debug, Serialize, Deserialize)]
struct ValidatorState {
    step: u64,
    current_row: Vec<u64>,
    center_column: Vec<u64>,
    valid_miners: Vec<u32>,
    hotkeys: Vec<String>,
}

pub struct Validator {
    keypair: Keypair,
    substrate: SubstrateInterface,
    metagraph: Metagraph,
    client: Client,

    step: u64,
    current_row: Array1<u64>,
    center_column: Array1<u64>,
    scores: Vec<f64>,

    valid_miners: Vec<u32>,
    hotkeys: Vec<String>,

    last_metagraph_sync: u32,
    last_wandb_run: Option<u32>,
    wandb_run: Option<Run>,
}

impl Validator {
    pub async fn new() -> Result<Self, ValidatorError> {
        let keypair = load_hotkey_keypair(
            &config::WALLET_NAME,
            &config::HOTKEY_NAME,
        ).map_err(|e| ValidatorError::KeypairError(e.to_string()))?;

        let substrate = SubstrateInterface::new(&config::SUBTENSOR_ADDRESS)
            .map_err(|e| ValidatorError::SubstrateError(e.to_string()))?;

        let metagraph = Metagraph::new(
            substrate.clone(),
            config::NETUID,
            false, // load_old_nodes
        ).map_err(|e| ValidatorError::MetagraphError(e.to_string()))?;

        metagraph.sync_nodes().await?;

        let hotkeys: Vec<String> = metagraph.nodes.keys().cloned().collect();
        let scores = vec![0.0; hotkeys.len()];

        let mut validator = Self {
            keypair,
            substrate,
            metagraph,
            client: Client::new(),
            step: 1,
            current_row: Array1::from_vec(vec![1]),
            center_column: Array1::from_vec(vec![1]),
            scores,
            valid_miners: Vec::new(),
            hotkeys,
            last_metagraph_sync: 0,
            last_wandb_run: None,
            wandb_run: None,
        };

        validator.load_state()?;
        Ok(validator)
    }

    fn state_path(&self) -> PathBuf {
        let home = dirs::home_dir().expect("Could not find home directory");
        home.join(".bittensor")
            .join("miners")
            .join(&config::WALLET_NAME)
            .join(&config::HOTKEY_NAME)
            .join(format!("netuid{}", config::NETUID))
            .join("validator")
            .join("state.json")
    }

    fn save_state(&self) -> Result<(), ValidatorError> {
        let state = ValidatorState {
            step: self.step,
            current_row: self.current_row.to_vec(),
            center_column: self.center_column.to_vec(),
            valid_miners: self.valid_miners.clone(),
            hotkeys: self.hotkeys.clone(),
        };

        let path = self.state_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ValidatorError::IoError(e.to_string()))?;
        }

        let json = serde_json::to_string(&state)
            .map_err(|e| ValidatorError::SerializationError(e.to_string()))?;
        
        fs::write(&path, json)
            .map_err(|e| ValidatorError::IoError(e.to_string()))?;

        Ok(())
    }

    fn load_state(&mut self) -> Result<(), ValidatorError> {
        let path = self.state_path();
        if !path.exists() {
            return Ok(());
        }

        let json = fs::read_to_string(&path)
            .map_err(|e| ValidatorError::IoError(e.to_string()))?;

        let state: ValidatorState = serde_json::from_str(&json)
            .map_err(|e| ValidatorError::SerializationError(e.to_string()))?;

        self.step = state.step;
        self.current_row = Array1::from_vec(state.current_row);
        self.center_column = Array1::from_vec(state.center_column);
        self.valid_miners = state.valid_miners;
        self.hotkeys = state.hotkeys;

        Ok(())
    }

    fn check_registered(&self) -> Result<(), ValidatorError> {
        if !self.metagraph.nodes.contains_key(&self.keypair.ss58_address()) {
            return Err(ValidatorError::UnrecoverableError(format!(
                "Hotkey {} is not registered in {}",
                self.keypair.ss58_address(),
                self.metagraph.netuid
            )));
        }
        Ok(())
    }

    async fn sync(&mut self, block: Option<u32>) -> Result<(), ValidatorError> {
        self.metagraph.sync_nodes().await?;

        let block = block.unwrap_or_else(|| self.substrate.get_block_number());
        self.last_metagraph_sync = block;

        self.check_registered()?;
        self.valid_miners.clear();

        // Update scores array size if needed
        if self.hotkeys.len() != self.metagraph.nodes.len() {
            let mut new_scores = vec![0.0; self.metagraph.nodes.len()];
            new_scores[..self.scores.len()].copy_from_slice(&self.scores);
            self.scores = new_scores;
        }

        // Check current steps of all miners
        let futures: Vec<_> = self.metagraph.nodes.values().map(|node| {
            self.make_request(node, "current_step", None)
        }).collect();

        let results = futures::future::join_all(futures).await;
        
        for (idx, result) in results.into_iter().enumerate() {
            if let Ok((response, _)) = result {
                if let Ok(step) = response.json::<u64>().await {
                    if step == 1 {
                        self.valid_miners.push(idx as u32);
                    }
                }
            }
        }

        // Set weights if enough time has passed
        let node = &self.metagraph.nodes[&self.keypair.ss58_address()];
        if block - node.last_updated >= EPOCH_LENGTH {
            self.set_weights()?;
        }

        Ok(())
    }

    async fn do_step(&mut self) -> Result<(), ValidatorError> {
        log::info!("Evolution step {}", self.step);

        let current_block = self.substrate.get_block_number();
        let elapsed_blocks = current_block - self.last_metagraph_sync;

        if elapsed_blocks >= EPOCH_LENGTH {
            self.sync(Some(current_block)).await?;
        }

        // Wait for miners if needed
        while self.valid_miners.is_empty() {
            log::info!("Not enough miners to compute step, waiting until next sync");
            sleep(Duration::from_secs((EPOCH_LENGTH - elapsed_blocks) as u64)).await;
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
                let node = &self.metagraph.nodes[&self.hotkeys[uid as usize]];
                let result = self.make_request(node, "compute", Some(&data)).await;
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
                        self.scores[uid as usize] = 1.0 / inference_time;
                        responses.push(data.parts);
                    } else {
                        self.scores[uid as usize] = 0.0;
                    }
                }
                Err(_) => {
                    self.scores[uid as usize] = 0.0;
                }
            }
        }

        // Update state
        if !responses.is_empty() {
            let mut simd_data: Vec<_> = responses.into_iter()
                .map(|v| Simd::from_array(v.try_into().unwrap()))
                .collect();

            let normalized = self.normalize_response_data(&mut simd_data);
            self.current_row = Array1::from_vec(normalized.into_iter().map(|x| x as u64).collect());

            let bit_index = self.step % 64;
            let row_part = self.current_row[self.step as usize / 64];
            
            self.center_column[self.center_column.len() - 1] |= 
                (row_part >> bit_index) << bit_index;
        }

        self.step += 1;
        self.save_state()?;
        self.check_wandb_run()?;

        Ok(())
    }

   .fn normalize_response_data(lists: &mut [Simd<u8, 32>]) -> Vec<u8> {
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
    for i in 0..lists.len()-1 {
        let mut current_list = lists[i].to_array();
        let mut next_list = lists[i+1].to_array();
        
        let (new_last, new_first) = normalize_pair(
            current_list[31],  // last element of current list
            next_list[0]       // first element of next list
        );
        
        current_list[31] = new_last;
        next_list[0] = new_first;
        
        // Update the lists with normalized values
        lists[i] = Simd::from_array(current_list);
        lists[i+1] = Simd::from_array(next_list);
        
        // Extend normalized_outputs with current list
        normalized_outputs.extend_from_slice(&current_list);
    }
    
    // Add the last list if there was more than one list
    if lists.len() > 1 {
        normalized_outputs.extend_from_slice(&lists[lists.len()-1].to_array());
    }
        
        normalized_outputs
    }
}

#[derive(Debug)]
pub enum ValidatorError {
    UnrecoverableError(String),
    KeypairError(String),
    SubstrateError(String),
    MetagraphError(String),
    IoError(String),
    SerializationError(String),
    NetworkError(String),
}

impl std::error::Error for ValidatorError {}

impl std::fmt::Display for ValidatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidatorError::UnrecoverableError(msg) => write!(f, "Unrecoverable error: {}", msg),
            ValidatorError::KeypairError(msg) => write!(f, "Keypair error: {}", msg),
            ValidatorError::SubstrateError(msg) => write!(f, "Substrate error: {}", msg),
            ValidatorError::MetagraphError(msg) => write!(f, "Metagraph error: {}", msg),
            ValidatorError::IoError(msg) => write!(f, "IO error: {}", msg),
            ValidatorError::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
            ValidatorError::NetworkError(msg) => write!(f, "Network error: {}", msg),
        }
    }
}