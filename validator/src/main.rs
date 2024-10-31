#![feature(portable_simd)]
#![feature(random)]
#![feature(mpmc_channel)]

use dotenv::dotenv;
use tokio;
use tracing::info;

mod validator;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_line_number(true)
        .with_thread_ids(true)
        .init();

    dotenv().unwrap();

    info!("Starting validator v{}", env!("CARGO_PKG_VERSION"));

    validator::Validator::new().await.run().await;
}
