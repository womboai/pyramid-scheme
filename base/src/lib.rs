use anyhow::Result;
use parity_scale_codec::{Compact, Decode};
use subxt::{Config, SubstrateConfig};
use subxt::client::OnlineClient;

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

        let response = self.client.tx();

        Ok(())
    }

    pub async fn get_block_number(&self) -> Result<u64> {
        Ok(self.client.blocks().at_latest().await?.number().into())
    }
}
