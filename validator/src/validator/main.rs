use std::process;
use tokio;
use tracing_subscriber::{fmt, EnvFilter};
use tracing::{info, error};

mod validator;
mod config;
mod models;
mod substrate_interface;
mod metagraph;

#[tokio::main]
async fn main() {
    // Initialize logging with tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env()
            .add_directive(tracing::Level::INFO.into()))
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(false)
        .init();

    info!("Starting validator v{}", env!("CARGO_PKG_VERSION"));

    // Run the validator
    match run().await {
        Ok(_) => {
            info!("Validator shut down gracefully");
            process::exit(0);
        }
        Err(e) => {
            error!("Fatal error: {}", e);
            process::exit(1);
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Set up signal handling for graceful shutdown
    let (shutdown_sender, shutdown_receiver) = tokio::sync::broadcast::channel(1);
    
    // Handle Ctrl+C
    let shutdown_sender_clone = shutdown_sender.clone();
    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("Failed to listen for Ctrl+C: {}", e);
            return;
        }
        info!("Received shutdown signal");
        let _ = shutdown_sender_clone.send(());
    });

    // Create and initialize validator
    info!("Initializing validator...");
    let mut validator = match validator::Validator::new().await {
        Ok(v) => v,
        Err(e) => {
            error!("Failed to initialize validator: {}", e);
            return Err(e.into());
        }
    };

    // Run validator with shutdown handling
    let validator_handle = tokio::spawn(async move {
        let mut shutdown_rx = shutdown_receiver;
        
        tokio::select! {
            result = validator.run() => {
                if let Err(e) = result {
                    error!("Validator error: {}", e);
                    return Err(e);
                }
                Ok(())
            }
            _ = shutdown_rx.recv() => {
                info!("Shutting down validator...");
                Ok(())
            }
        }
    });

    // Wait for validator to complete
    match validator_handle.await {
        Ok(result) => result?,
        Err(e) => {
            error!("Validator task panicked: {}", e);
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Validator task panicked"
            )));
        }
    }

    info!("Validator shutdown complete");
    Ok(())
}

// Error handling for the validator
#[derive(Debug)]
pub enum MainError {
    ValidationError(String),
    InitializationError(String),
    ShutdownError(String),
}

impl std::error::Error for MainError {}

impl std::fmt::Display for MainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MainError::ValidationError(msg) => write!(f, "Validation error: {}", msg),
            MainError::InitializationError(msg) => write!(f, "Initialization error: {}", msg),
            MainError::ShutdownError(msg) => write!(f, "Shutdown error: {}", msg),
        }
    }
}

// Build information module
mod build_info {
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}