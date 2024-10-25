use std::convert::TryInto;
use std::env;

pub static EPOCH_LENGTH: u64 = env::var("EPOCH_LENGTH")
    .map(|epoch| <String as TryInto<u64>>::try_into(epoch).unwrap())
    .unwrap_or(100);

pub static CHAIN_ENDPOINT: String = env::var("CHAIN_ENDPOINT").unwrap();

pub static NETUID: u16 = env::var("NETUID")
    .map(|epoch| <String as TryInto<u16>>::try_into(epoch).unwrap())
    .unwrap();

pub static WALLET_NAME: String = env::var("WALLET_NAME").unwrap();
pub static HOTKEY_NAME: String = env::var("HOTKEY_NAME").unwrap();
