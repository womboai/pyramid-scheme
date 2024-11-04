#![feature(portable_simd)]
#![feature(random)]
#![feature(mpmc_channel)]

use std::net::Ipv4Addr;
use axum::Router;
use axum::routing::get;
use dotenv::dotenv;
use tokio;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use neuron::config;
use crate::api::{current_step, last_n_bits};

mod validator;
mod api;

async fn api_main() {
    let ip: Ipv4Addr = [0u8, 0, 0, 0].into();

    let app = Router::new()
        .route("/step", get(current_step))
        .route("/bits", get(last_n_bits));

    let listener = TcpListener::bind((ip, *config::PORT)).await.unwrap();

    info!("Starting axon listener on {:?}", listener.local_addr());
    axum::serve(listener, app).await.unwrap();
}

#[tokio::main]
async fn main() {
    if let Err(e) = dotenv() {
        warn!("Could not load .env: {e}");
    }

    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    let fmt = fmt::layer()
        .with_line_number(true)
        .with_thread_ids(true);

    tracing_subscriber::registry()
        .with(fmt)
        .with(filter_layer)
        .init();

    info!("Starting validator v{}", env!("CARGO_PKG_VERSION"));

    tokio::task::spawn(api_main());

    let mut validator = validator::Validator::new().await;

    validator.run().await;
}
