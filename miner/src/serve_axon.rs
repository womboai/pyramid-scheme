use std::net::{IpAddr, SocketAddr};

use clap::Parser;
use dotenv::dotenv;
use neuron::{config, subtensor};
use rusttensor::axon::{serve_axon_payload, AxonProtocol};
use rusttensor::subtensor::Subtensor;
use rusttensor::wallet::{hotkey_location, load_key_seed, signer_from_seed};

#[derive(Parser)]
struct Cli {
    ip: IpAddr,
    port: Option<u16>,
}

#[tokio::main]
async fn main() {
    if let Err(e) = dotenv() {
        println!("Could not load .env: {e}");
    }

    let args: Cli = Cli::parse();

    let hotkey_location = hotkey_location(
        config::WALLET_PATH.clone(),
        &*config::WALLET_NAME,
        &*config::HOTKEY_NAME,
    );

    let seed = load_key_seed(&hotkey_location).unwrap();

    let signer = signer_from_seed(&seed).unwrap();

    let subtensor = subtensor().await.unwrap();

    let payload = serve_axon_payload(
        *config::NETUID,
        SocketAddr::new(args.ip, args.port.unwrap_or_else(|| *config::PORT)),
        AxonProtocol::Tcp,
    );

    subtensor
        .tx()
        .sign_and_submit_default(&payload, &signer)
        .await
        .unwrap();
}
