use std::env;
use std::path::PathBuf;
use std::sync::LazyLock;

pub static WALLET_NAME: LazyLock<String> = LazyLock::new(|| env::var("WALLET_NAME").unwrap());
pub static HOTKEY_NAME: LazyLock<String> = LazyLock::new(|| env::var("HOTKEY_NAME").unwrap());

pub static WALLET_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    env::var("WALLET_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_wallet_path().expect("No home directory nor WALLET_PATH set"))
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
