#![feature(portable_simd)]
#![feature(random)]
#![feature(mpmc_channel)]

use crate::api::{current_step, last_n_bits};
use axum::routing::get;
use axum::Router;
use dotenv::dotenv;
use neuron::{config, hotkey_location, load_key_seed, setup_opentelemetry, signer_from_seed};
use tracing::info;

use std::net::Ipv4Addr;
use tokio;
use tokio::net::TcpListener;

mod api;
mod validator;

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
        println!("Could not load .env: {e}");
    }

    let hotkey_location = hotkey_location(
        config::WALLET_PATH.clone(),
        &*config::WALLET_NAME,
        &*config::HOTKEY_NAME,
    );

    let seed = load_key_seed(&hotkey_location).unwrap();

    let signer = signer_from_seed(&seed).unwrap();

    setup_opentelemetry(&signer.account_id(), "validator");

    let mut validator = validator::Validator::new(signer).await;

    tokio::task::spawn(api_main());

    validator.run().await;
}
