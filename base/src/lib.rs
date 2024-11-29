extern crate core;

use std::cell::LazyCell;
use std::convert::Into;
use anyhow::{anyhow, Result};
use dotenv::from_filename;
use opentelemetry::{KeyValue, Value as LogValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::logs::LoggerProvider;
use opentelemetry_sdk::Resource;
use rusttensor::subtensor::Subtensor;
use rusttensor::AccountId;
use std::env;
use std::iter::Iterator;
use std::str::FromStr;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

pub mod auth;
pub mod config;
pub mod updater;

const OPENTELEMETRY_EXPORT_ENDPOINT: &'static str = "http://18.215.170.244:4317";

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
#[repr(transparent)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl FromStr for Version {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mut parts = s.split('.').map(|n| n.parse().unwrap());

        let major = parts.next().unwrap_or(0);
        let minor = parts.next().unwrap_or(0);
        let patch = parts.next().unwrap_or(0);

        if parts.next().is_some() {
            return Err(anyhow!("Crate version string {s} has too many parts, needs to be x.y.z"));
        }

        Ok(Version {
            major,
            minor,
            patch,
        })
    }
}

impl From<Version> for u64 {
    fn from(value: Version) -> Self {
        (value.major as u64) << 32 | (value.minor as u64) << 16 | (value.patch as u64)
    }
}

pub const SPEC_VERSION: u32 = 1;

pub const VERSION_STRING: &'static str = env!("CARGO_PKG_VERSION");

pub static VERSION: LazyCell<Version> = LazyCell::new(|| Version::from_str(VERSION_STRING).unwrap());

pub static INTEGRAL_VERSION: LazyCell<u64> = LazyCell::new(|| VERSION.into());

#[cfg(not(target_pointer_width = "64"))]
compile_error!("Compilation is only allowed for 64-bit targets");

#[cfg(not(target_endian = "little"))]
compile_error!("Compilation is only allowed for little-endian based processors");

pub async fn subtensor() -> Result<Subtensor> {
    if *config::INSECURE_CHAIN_SCHEME {
        Ok(Subtensor::from_insecure_url(&*config::CHAIN_ENDPOINT).await?)
    } else {
        Ok(Subtensor::from_url(&*config::CHAIN_ENDPOINT).await?)
    }
}

pub fn load_env() {
    let prefix = env::var("DOT_ENV_FILE_PREFIX").unwrap_or(String::new());
    let file_name = format!("{prefix}.env");

    if let Err(e) = from_filename(&file_name) {
        println!("Could not load {file_name}: {e}");
    }
}

pub fn setup_opentelemetry(account_id: &AccountId, neuron_type: &'static str) {
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    let exporter_builder = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(OPENTELEMETRY_EXPORT_ENDPOINT);

    let provider: LoggerProvider = LoggerProvider::builder()
        .with_resource(Resource::new(vec![
            KeyValue::new("service.name", "pyramid-scheme"),
            KeyValue::new("neuron.type", neuron_type),
            KeyValue::new("netuid", LogValue::I64(*config::NETUID as i64)),
            KeyValue::new("neuron.hotkey", account_id.to_string()),
        ]))
        .with_batch_exporter(
            exporter_builder.build_log_exporter().unwrap(),
            opentelemetry_sdk::runtime::Tokio,
        )
        .build();

    let otel = OpenTelemetryTracingBridge::new(&provider);

    let fmt = fmt::layer().with_line_number(true).with_thread_ids(true);

    tracing_subscriber::registry()
        .with(fmt)
        .with(filter_layer)
        .with(otel)
        .init();

    info!("Starting {} v{}", neuron_type, env!("CARGO_PKG_VERSION"));
}
