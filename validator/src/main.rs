use std::process;

use tokio;
use tracing::{error, info};
use tracing_subscriber::{fmt};

mod validator;

#[tokio::main]
async fn main() {
    // Initialize logging with tracing
    tracing_subscriber::fmt()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(false)
        .init();

    info!("Starting validator v{}", env!("CARGO_PKG_VERSION"));

    // Create and initialize validator
    info!("Initializing validator...");
    validator::Validator::new().await.run().await;
}
