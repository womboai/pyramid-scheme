use tokio;

mod validator;
mod config;
mod models;

#[tokio::main]
async fn main() {
    // Initialize logging
    env_logger::init();

    // Create and run the validator
    let mut validator = validator::Validator::new();
    validator.run().await;
}
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct ComputationData {
    pub parts: Vec<u64>,
}

// Add other necessary structs and enumshow