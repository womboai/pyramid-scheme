use substrate_interface::{SubstrateInterface, Keypair};
use ndarray::Array1;
use crate::models::ComputationData;
use crate::config;
use std::simd::{Simd, SimdUint};

pub struct Validator {
    keypair: Keypair,
    substrate: SubstrateInterface,
    // Add other fields as needed
}

impl Validator {
    pub fn new() -> Self {
        // Initialize the Validator
        // This will involve setting up the substrate connection,
        // loading the keypair, and initializing other fields
        Self {
            keypair: Keypair::new(), // You'll need to implement this
            substrate: SubstrateInterface::new(config::SUBTENSOR_ADDRESS), // You'll need to implement this
            // Initialize other fields
        }
    }

    pub async fn run(&mut self) {
        loop {
            if let Err(e) = self.do_step().await {
                log::error!("Error in evolution step: {:?}", e);
            }
        }
    }

    async fn do_step(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Implement the step logic
        Ok(())
    }

    async fn sync(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Implement sync logic
        Ok(())
    }

    fn set_weights(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Implement weight setting logic
        Ok(())
    }

    async fn make_request(&self, node: &Node, endpoint: &str, payload: Option<&ComputationData>) -> Result<(reqwest::Response, f64), Box<dyn std::error::Error>> {
        // Implement request logic
        unimplemented!()
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

// Implement error handling
#[derive(Debug)]
pub enum ValidatorError {
    UnrecoverableError(String),
    // Other error types
}

impl std::error::Error for ValidatorError {}

impl std::fmt::Display for ValidatorError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ValidatorError::UnrecoverableError(msg) => write!(f, "Unrecoverable error: {}", msg),
            // Handle other error types
        }
    }
}
