#![feature(portable_simd)]
#![feature(random)]
#![feature(sync_unsafe_cell)]
#![feature(ptr_as_ref_unchecked)]

use tokio;
use tracing::info;

mod validator;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_line_number(true)
        .with_thread_ids(true)
        .init();

    info!("Starting validator v{}", env!("CARGO_PKG_VERSION"));

    validator::Validator::new().await.run().await;
}
