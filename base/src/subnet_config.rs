use std::borrow::ToOwned;
use std::env;
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
