//! Main binary entry point

use anyhow::Result;
use jarvis_policy::{PolicyEngine, config};
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::sync::Arc;
use tracing::info;

/// Keeps the [`SdkTracerProvider`] alive for the lifetime of `main` and makes
/// sure the batch exporter is shut down on every exit path.
///
/// The provider is cloned into the tracer held by the global tracing layer, so
/// it is not the last `Arc` reference once `init_tracing` returns. Dropping the
/// local handle there therefore never triggers shutdown, and spans queued at
/// process exit are lost. Holding this guard in `main` and calling `shutdown`
/// explicitly flushes them; [`Drop`] is the safety net for early `?` returns.
struct TracerGuard {
    provider: SdkTracerProvider,
}

impl TracerGuard {
    /// Flush queued spans. Errors are reported on stderr because this runs at
    /// process teardown, where the tracing layer may already be gone. A
    /// failure is intentionally non-fatal, matching the exporter policy.
    fn shutdown(&self) {
        if let Err(err) = self.provider.shutdown() {
            eprintln!("jarvis-policy: tracer provider shutdown failed: {err}");
        }
    }
}

impl Drop for TracerGuard {
    fn drop(&mut self) {
        // Safety net for error paths that return early via `?`. A repeated
        // shutdown is a harmless `AlreadyShutdown` error and is ignored here.
        let _ = self.provider.shutdown();
    }
}

/// Initialise JSON logging with optional OTLP trace export.
///
/// The collector endpoint comes from `OTEL_EXPORTER_OTLP_ENDPOINT` and
/// defaults to the local collector. An invalid endpoint (or any other
/// exporter build failure) is downgraded to a warning: the service still
/// starts with JSON logging and metrics intact, only trace export is lost.
/// The provider is returned so its lifetime can be tied to `main`.
fn init_tracing(service: &'static str) -> Result<TracerGuard> {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:4317".to_string());

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build();

    // Keep a provider on every path so `TracerGuard::shutdown` stays
    // meaningful. Without an exporter it simply has no span processors.
    let (provider, telemetry, exporter_error) = match exporter {
        Ok(exporter) => {
            let provider = SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .build();
            let tracer = provider.tracer(service);
            let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
            (provider, Some(telemetry), None)
        }
        Err(err) => (
            SdkTracerProvider::builder().build(),
            None,
            Some(err.to_string()),
        ),
    };

    // The global default subscriber is installed exactly once, as before.
    tracing_subscriber::registry()
        .with(telemetry)
        .with(tracing_subscriber::fmt::layer().json())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Warn only after the fmt layer is installed; before that there is no
    // subscriber and the message would be lost.
    if let Some(err) = exporter_error {
        tracing::warn!(
            error = %err,
            endpoint = %endpoint,
            "OTLP trace exporter initialisation failed; continuing without trace export"
        );
    }

    Ok(TracerGuard { provider })
}

#[tokio::main]
async fn main() -> Result<()> {
    let tracer = init_tracing("jarvis-policy")?;
    jarvis_policy::http::install_metrics()?;

    let config = config::PolicyConfig::load()?;
    let engine = Arc::new(PolicyEngine::new(config.clone()).await?);

    info!(
        "Starting JARVIS Policy Engine on gRPC {} HTTP {}",
        config.grpc_addr, config.http_addr
    );

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    // `join!` polls both listeners to completion, so a failure in one never
    // drops the other's future. Errors are aggregated only after both have
    // finished.
    let grpc = engine.serve_grpc(grpc_addr);
    let http = engine.serve_http(http_addr);
    let (grpc_result, http_result) = tokio::join!(grpc, http);

    // Explicit shutdown on the normal path; `TracerGuard::drop` covers the
    // early `?` returns above as well as any unwinding.
    tracer.shutdown();

    match (grpc_result, http_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(err), Ok(())) | (Ok(()), Err(err)) => Err(err),
        (Err(grpc_err), Err(http_err)) => Err(anyhow::anyhow!(
            "gRPC listener failed: {grpc_err}; HTTP listener failed: {http_err}"
        )),
    }
}
