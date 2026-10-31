//! Main binary entry point

use anyhow::Result;
use jarvis_event::EventNormalizer;
use tracing::info;

/// Initialise JSON logging with OTLP trace export.
///
/// The collector endpoint comes from `OTEL_EXPORTER_OTLP_ENDPOINT` and
/// defaults to the local collector. Export failures do not stop the service.
fn init_tracing(service: &'static str) -> Result<()> {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:4317".to_string());

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build()?;

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();

    let tracer = provider.tracer(service);
    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

    tracing_subscriber::registry()
        .with(telemetry)
        .with(tracing_subscriber::fmt::layer().json())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing("jarvis-event")?;
    jarvis_event::http::install_metrics()?;

    let config = jarvis_event::config::EventConfig::load()?;
    let normalizer = EventNormalizer::new(config.clone()).await?;

    info!(
        "Starting JARVIS Event Normalizer on gRPC {} HTTP {} Webhook {}",
        config.grpc_addr, config.http_addr, config.webhook_addr
    );

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    tokio::try_join!(
        normalizer.serve_grpc(grpc_addr),
        normalizer.serve_http(http_addr),
        normalizer.start_webhook_server(),
    )?;

    Ok(())
}
