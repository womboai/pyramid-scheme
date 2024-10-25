use std::fs::File;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use anyhow::Result;
use hex;
use parity_scale_codec::{Compact, Decode};
use serde_json::Value;
use subxt::{Config, SubstrateConfig};
use subxt::client::OnlineClient;
use subxt::ext::sp_core::{Pair, sr25519};
use subxt::tx::PairSigner;

pub mod config;

include!(concat!(env!("OUT_DIR"), "/metadata.rs"));

type SubtensorConfig = SubstrateConfig;
pub type AccountId = <SubtensorConfig as Config>::AccountId;

#[derive(Decode, Default, Clone, Debug)]
pub struct AxonInfo {
    pub block: u64,
    pub version: u32,
    pub ip: u128,
    pub port: u16,
    pub ip_type: u8,
    pub protocol: u8,
    _placeholder1: u8,
    _placeholder2: u8,
}

#[derive(Decode, Default, Clone, Debug)]
pub struct PrometheusInfo {
    pub block: u64,
    pub version: u32,
    pub ip: u128,
    pub port: u16,
    pub ip_type: u8,
}

#[derive(Decode, Clone, Debug)]
pub struct NeuronInfoLite {
    pub hotkey: AccountId,
    pub coldkey: AccountId,
    pub uid: Compact<u16>,
    pub netuid: Compact<u16>,
    pub active: bool,
    pub axon_info: AxonInfo,
    pub prometheus_info: PrometheusInfo,
    pub stake: Vec<(AccountId, Compact<u64>)>,
    pub rank: Compact<u16>,
    pub emission: Compact<u64>,
    pub incentive: Compact<u16>,
    pub consensus: Compact<u16>,
    pub trust: Compact<u16>,
    pub validator_trust: Compact<u16>,
    pub dividends: Compact<u16>,
    pub last_update: Compact<u64>,
    pub validator_permit: bool,
    pub pruning_score: Compact<u16>,
}

pub struct Subtensor {
    client: OnlineClient<SubtensorConfig>,
}

pub struct Keypair(PairSigner<SubtensorConfig, sr25519::Pair>);

impl Deref for Keypair {
    type Target = PairSigner<SubtensorConfig, sr25519::Pair>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub fn hotkey_location_with_home(
    mut home_directory: PathBuf,
    wallet_name: impl AsRef<Path>,
    hotkey_name: impl AsRef<Path>,
) -> PathBuf {
    home_directory.push(".bittensor");
    home_directory.push("wallets");
    home_directory.push(wallet_name);
    home_directory.push("hotkeys");
    home_directory.push(hotkey_name);

    home_directory
}

pub fn hotkey_location(
    wallet_name: impl AsRef<Path>,
    hotkey_name: impl AsRef<Path>,
) -> Option<PathBuf> {
    Some(hotkey_location_with_home(
        dirs::home_dir()?,
        wallet_name,
        hotkey_name,
    ))
}

pub fn load_key_seed(path: impl AsRef<Path>) -> Result<[u8; 32]> {
    let json: Value = serde_json::from_reader(File::open(path)?)?;

    let seed = json.as_object()?.get("secretSeed")?.as_str().unwrap();

    let mut decoded = [0; 32];
    hex::decode_to_slice(&seed[2..], &mut decoded)?;

    Ok(decoded)
}

impl Keypair {
    pub fn from_seed(seed: &[u8]) -> Result<Self> {
        Ok(Self(PairSigner::new(sr25519::Pair::from_seed_slice(seed)?)))
    }
}

impl Subtensor {
    pub async fn new(url: impl AsRef<str>) -> Result<Self> {
        Ok(Self {
            client: OnlineClient::from_url(url).await?,
        })
    }

    pub async fn get_neurons(&self, netuid: u16) -> Result<Vec<NeuronInfoLite>> {
        let neurons_payload = api::apis()
            .neuron_info_runtime_api()
            .get_neurons_lite(netuid);

        let response = self
            .client
            .runtime_api()
            .at_latest()
            .await?
            .call(neurons_payload)
            .await?;

        Ok(Vec::<NeuronInfoLite>::decode(&mut response.as_ref())?)
    }

    pub async fn set_weights(
        &self,
        keypair: &Keypair,
        netuid: u16,
        weights: Vec<(u16, u16)>,
        version_key: u64,
    ) -> Result<()> {
        let set_weights_payload = api::tx().subtensor_module().set_weights(
            netuid,
            weights.iter().map(|(uid, _)| *uid).collect(),
            weights.iter().map(|(_, weight)| *weight).collect(),
            version_key,
        );

        self.client
            .tx()
            .sign_and_submit_default(&set_weights_payload, &keypair.0)
            .await?;

        Ok(())
    }

    pub async fn get_block_number(&self) -> Result<u64> {
        Ok(self.client.blocks().at_latest().await?.number().into())
    }
}
