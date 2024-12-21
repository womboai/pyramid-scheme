#![feature(portable_simd)]
#![feature(random)]
#![feature(mpmc_channel)]
#![feature(sync_unsafe_cell)]

use crate::api::{current_step, last_n_bits};
use axum::routing::get;
use axum::Router;
use neuron::updater::Updater;
use neuron::{config, load_env, setup_logging};
use tracing::info;

use rusttensor::wallet::{hotkey_location, load_key_seed, signer_from_seed};
use std::net::Ipv4Addr;
use tokio;
use tokio::net::TcpListener;
use tokio::time::Duration;

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
    load_env();

    if *config::AUTO_UPDATE {
        let updater = Updater::new(Duration::from_secs(3600));
        updater.spawn();
    }

    let metrics = validator::metrics::setup_metrics();

    let hotkey_location = hotkey_location(
        config::WALLET_PATH.clone(),
        &*config::WALLET_NAME,
        &*config::HOTKEY_NAME,
    );

    let seed = load_key_seed(&hotkey_location).unwrap();

    let signer = signer_from_seed(&seed).unwrap();

    setup_logging(&signer.account_id(), false, "validator");

    let mut validator = validator::Validator::new(signer, metrics.clone()).await;

    tokio::task::spawn(api_main());

    validator.run().await;

    opentelemetry::global::shutdown_tracer_provider();
}
