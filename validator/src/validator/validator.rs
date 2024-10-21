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

    fn normalize_response_data<'a, I>(&self, responses: I, dest: &mut [u8])
    where
        I: Iterator<Item = &'a [u8]>,
    {
        fn rule_30_simd<const N: usize>(a: Simd<u64, N>) -> Simd<u64, N>
        where
            Simd<u64, N>: SimdUint,
        {
            a ^ ((a << Simd::splat(1)) | (a << Simd::splat(2)))
        }

        fn normalize_pair_simd<const N: usize>(a: Simd<u64, N>, b: Simd<u64, N>) -> (Simd<u64, N>, Simd<u64, N>)
        where
            Simd<u64, N>: SimdUint,
        {
            let carry = a & Simd::splat(1);
            let mut a = a >> Simd::splat(1);
            let mut b = (carry << Simd::splat(63)) | b;
            a = rule_30_simd(a);
            b = rule_30_simd(b);
            let msb = b >> Simd::splat(63);
            b &= Simd::splat((1 << 63) - 1);
            a = (a << Simd::splat(1)) | msb;
            (a, b)
        }

        let mut dest_index = 0;
        let mut last_byte = 0u8;
        let mut is_first = true;

        for response in responses {
            if response.is_empty() {
                continue;
            }

            if is_first {
                // Copy the first byte of the first response as-is
                dest[dest_index] = response[0];
                dest_index += 1;
                is_first = false;
            } else {
                // Normalize the first byte with the last byte of the previous response
                let (_, new_first) = normalize_pair_simd(
                    Simd::from_array([last_byte as u64]),
                    Simd::from_array([response[0] as u64])
                );
                dest[dest_index] = new_first.to_array()[0] as u8;
                dest_index += 1;
            }

            // Copy the middle bytes
            let middle_len = response.len() - 2;
            dest[dest_index..dest_index + middle_len].copy_from_slice(&response[1..response.len() - 1]);
            dest_index += middle_len;

            // Store the last byte for the next iteration
            last_byte = response[response.len() - 1];
        }

        // Write the final byte
        if dest_index < dest.len() {
            dest[dest_index] = last_byte;
        }
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
