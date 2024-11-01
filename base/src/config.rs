use std::borrow::ToOwned;
use std::env;
use std::path::PathBuf;
use std::sync::LazyLock;

pub static EPOCH_LENGTH: LazyLock<u64> = LazyLock::new(|| {
    env::var("EPOCH_LENGTH")
        .map(|var| var.parse().unwrap())
        .unwrap_or(100)
});

pub static CHAIN_ENDPOINT: LazyLock<String> = LazyLock::new(|| {
    env::var("CHAIN_ENDPOINT").unwrap_or("wss://entrypoint-finney.opentensor.ai:443".to_owned())
});

pub static NETUID: LazyLock<u16> = LazyLock::new(|| {
    env::var("NETUID")
        .as_ref()
        .map(|var| var.parse().unwrap())
        .unwrap_or(36)
});

pub static WALLET_NAME: LazyLock<String> = LazyLock::new(|| env::var("WALLET_NAME").unwrap());
pub static HOTKEY_NAME: LazyLock<String> = LazyLock::new(|| env::var("HOTKEY_NAME").unwrap());

pub static WALLET_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    env::var("WALLET_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_wallet_path().expect("No home directory nor WALLET_PATH set"))
});

pub static PORT: LazyLock<u16> = LazyLock::new(|| {
    env::var("PORT")
        .map(|port| port.parse().unwrap())
        .unwrap_or(8091)
});

fn default_wallet_path() -> Option<PathBuf> {
    if let Some(mut dir) = dirs::home_dir() {
        dir.push(".bittensor");
        dir.push("wallets");

        Some(dir)
    } else {
        None
    }
}
