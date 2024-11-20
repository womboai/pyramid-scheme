extern crate core;

use opentelemetry::{KeyValue, Value as LogValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::logs::LoggerProvider;
use opentelemetry_sdk::Resource;
use rusttensor::AccountId;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

pub mod auth;
pub mod config;
pub mod updater;

const OPENTELEMETRY_EXPORT_ENDPOINT: &'static str = "http://18.215.170.244:4317";

#[cfg(not(target_pointer_width = "64"))]
compile_error!("Compilation is only allowed for 64-bit targets");

#[cfg(not(target_endian = "little"))]
compile_error!("Compilation is only allowed for little-endian based processors");

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
