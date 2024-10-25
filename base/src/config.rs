use std::env;
use std::sync::LazyLock;

pub static EPOCH_LENGTH: LazyLock<u64> = LazyLock::new(|| {
    env::var("EPOCH_LENGTH")
        .map(|var| var.parse().unwrap())
        .unwrap_or(100)
});

pub static CHAIN_ENDPOINT: LazyLock<String> = LazyLock::new(|| env::var("CHAIN_ENDPOINT").unwrap());

pub static NETUID: LazyLock<u16> = LazyLock::new(|| {
    env::var("NETUID")
        .as_ref()
        .map(|var| var.parse().unwrap())
        .unwrap()
});

pub static WALLET_NAME: LazyLock<String> = LazyLock::new(|| env::var("WALLET_NAME").unwrap());
pub static HOTKEY_NAME: LazyLock<String> = LazyLock::new(|| env::var("HOTKEY_NAME").unwrap());
