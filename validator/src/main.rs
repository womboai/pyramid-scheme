#![feature(portable_simd)]
#![feature(random)]
#![feature(mpmc_channel)]

use std::net::Ipv4Addr;
use axum::Router;
use axum::routing::get;
use dotenv::dotenv;
use opentelemetry::{KeyValue, Value};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::LoggerProvider;
use opentelemetry_sdk::Resource;
use tokio;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use neuron::config;
use crate::api::{current_step, last_n_bits};

mod validator;
mod api;

async fn api_main() {
    let ip: Ipv4Addr = [0u8, 0, 0, 0].into();

    let app = Router::new()
        .route("/step", get(current_step))
        .route("/bits", get(last_n_bits));

    let listener = TcpListener::bind((ip, *config::PORT)).await.unwrap();

    info!("Starting axon listener on {:?}", listener.local_addr());
    axum::serve(listener, app).await.unwrap();
}

#[tokio::main]
async fn main() {
    dotenv().unwrap();

    let mut validator = validator::Validator::new().await;

    let exporter_builder = opentelemetry_otlp::new_exporter().tonic();

    let provider: LoggerProvider = LoggerProvider::builder()
        .with_resource(Resource::new(vec![
            KeyValue::new("service.name", "pyramid-scheme-validator"),
            KeyValue::new("neuron.type", "validator"),
            KeyValue::new("neuron.uid", Value::I64(validator.uid as i64)),
            KeyValue::new("neuron.hotkey", validator.account_id().to_string()),
        ]))
        .with_batch_exporter(exporter_builder.build_log_exporter().unwrap(), opentelemetry_sdk::runtime::Tokio)
        .build();

    let otel = OpenTelemetryTracingBridge::new(&provider);

    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    let fmt = fmt::layer()
        .with_line_number(true)
        .with_thread_ids(true);

    tracing_subscriber::registry()
        .with(fmt)
        .with(otel)
        .with(filter_layer)
        .init();

    info!("Starting validator v{}", env!("CARGO_PKG_VERSION"));

    tokio::task::spawn(api_main());

    validator.run().await;
}
