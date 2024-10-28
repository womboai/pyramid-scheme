extern crate core;

use std::fmt::{Display, Formatter};
use std::fs::File;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::Result;
use hex;
use parity_scale_codec::{Compact, Decode};
use serde_json::Value;
use subxt::client::OnlineClient;
use subxt::ext::sp_core::{Pair, sr25519};
use subxt::tx::PairSigner;
use subxt::{Config, SubstrateConfig};
use thiserror::Error;

pub mod config;
pub mod auth;

include!(concat!(env!("OUT_DIR"), "/metadata.rs"));

type SubtensorConfig = SubstrateConfig;
pub type AccountId = <SubtensorConfig as Config>::AccountId;

pub type Keypair = sr25519::Pair;
pub type PublicKey = sr25519::Public;
pub type Signer = PairSigner<SubtensorConfig, Keypair>;

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

#[derive(Error, Debug)]
struct InvalidAccountJsonError(PathBuf);

impl Display for InvalidAccountJsonError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Invalid wallet account file: {:?}", self.0))
    }
}

#[cfg(not(target_pointer_width = "64"))]
compile_error!("Compilation is only allowed for 64-bit targets");

#[cfg(not(target_endian = "little"))]
compile_error!("Compilation is only allowed for little-endian based processors");

pub fn hotkey_location(
    mut wallet_path: PathBuf,
    wallet_name: impl AsRef<Path>,
    hotkey_name: impl AsRef<Path>,
) -> PathBuf {
    wallet_path.push(wallet_name);
    wallet_path.push("hotkeys");
    wallet_path.push(hotkey_name);

    wallet_path
}

pub fn load_key_seed(path: impl AsRef<Path>) -> Result<[u8; 32]> {
    let json: Value = serde_json::from_reader(File::open(&path)?)?;

    let seed = json
        .as_object()
        .ok_or_else(|| InvalidAccountJsonError(path.as_ref().to_path_buf()))?
        .get("secretSeed")
        .ok_or_else(|| InvalidAccountJsonError(path.as_ref().to_path_buf()))?
        .as_str()
        .ok_or_else(|| InvalidAccountJsonError(path.as_ref().to_path_buf()))?;

    let mut decoded = [0; 32];
    hex::decode_to_slice(&seed[2..], &mut decoded)?;

    Ok(decoded)
}

pub fn signer_from_seed(seed: &[u8]) -> Result<Signer> {
    Ok(Signer::new(sr25519::Pair::from_seed_slice(seed)?))
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
        signer: &Signer,
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
            .sign_and_submit_default(&set_weights_payload, signer)
            .await?;

        Ok(())
    }

    pub async fn serve_axon(
        &self,
        signer: &Signer,
        netuid: u16,
        ip: IpAddr,
        port: u16,
        udp: bool,
    ) -> Result<()> {
        let (ip, ip_type) = match ip {
            IpAddr::V4(v4) => (u32::from(v4) as u128, 4),
            IpAddr::V6(v6) => (u128::from(v6), 6),
        };

        let serve_axon_payload = api::tx().subtensor_module().serve_axon(
            netuid,
            008_000_000, // Masquerade as version 8.0.0 of btcli, for no particular reason,
            ip,
            port,
            ip_type,
            u8::from(udp),
            0,
            0,
        );

        self.client
            .tx()
            .sign_and_submit_default(&serve_axon_payload, signer)
            .await?;

        Ok(())
    }

    pub async fn get_block_number(&self) -> Result<u64> {
        Ok(self.client.blocks().at_latest().await?.number().into())
    }
}
