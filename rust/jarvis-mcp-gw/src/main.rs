//! Main binary entry point

use anyhow::Result;
use jarvis_mcp_gw::McpGateway;
use jarvis_policy::PolicyClient;
use std::sync::Arc;
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
    init_tracing("jarvis-mcp-gw")?;
    jarvis_mcp_gw::http::install_metrics()?;

    let config = jarvis_mcp_gw::config::McpGatewayConfig::load()?;
    let policy_client = Arc::new(PolicyClient::new(&config.policy_grpc_addr)?);

    let gateway = McpGateway::new(config.clone(), policy_client).await?;

    info!(
        "Starting JARVIS MCP Gateway on gRPC {} HTTP {}",
        config.grpc_addr, config.http_addr
    );

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    tokio::try_join!(gateway.serve_grpc(grpc_addr), gateway.serve_http(http_addr),)?;

    Ok(())
}
