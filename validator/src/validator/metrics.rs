use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::Config;
use opentelemetry_sdk::metrics::reader::DefaultTemporalitySelector;
use std::sync::Arc;

const OPENTELEMETRY_EXPORT_ENDPOINT: &str = "http://18.215.170.244:4317";

#[derive(Clone)]
pub struct ValidatorMetrics {
    pub evolution_steps: Counter<u64>,
    pub connected_miners: Counter<u64>,
    pub cheater_count: Counter<u64>,
    pub step_duration: Histogram<f64>,
    pub sync_duration: Histogram<f64>,
}

impl ValidatorMetrics {
    pub fn new(meter: Meter) -> Self {
        Self {
            evolution_steps: meter
                .u64_counter("evolution_steps")
                .with_description("Total number of evolution steps completed")
                .init(),
            
            connected_miners: meter
                .u64_counter("connected_miners")
                .with_description("Number of miners currently connected")
                .init(),

            cheater_count: meter
                .u64_counter("cheater_count")
                .with_description("Number of miners caught cheating")
                .init(),

            step_duration: meter
                .f64_histogram("step_duration")
                .with_description("Duration of each evolution step")
                .init(),

            sync_duration: meter
                .f64_histogram("sync_duration")
                .with_description("Duration of metagraph sync operations")
                .init(),
        }
    }
}

pub fn setup_metrics() -> Arc<ValidatorMetrics> {
    // Set up tracing first
    opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(OPENTELEMETRY_EXPORT_ENDPOINT)
        )
        .with_trace_config(Config::default())
        .install_batch(opentelemetry_sdk::runtime::Tokio)
        .unwrap();

    // Set up global metrics
    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(OPENTELEMETRY_EXPORT_ENDPOINT)
        .build_metrics_exporter(Box::new(DefaultTemporalitySelector::default()))
        .unwrap();

    let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(
        exporter,
        opentelemetry_sdk::runtime::Tokio
    ).build();

    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .build();

    // Set as global provider
    opentelemetry::global::set_meter_provider(provider);
    
    // Use global meter
    let meter = opentelemetry::global::meter("rule_30_validator");
    Arc::new(ValidatorMetrics::new(meter))
}
