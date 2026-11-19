//! Main binary entry point

use anyhow::Result;
use jarvis_event::EventNormalizer;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::info;

/// Fixed category for a tracer-provider shutdown failure (I51-B).
///
/// The raw SDK error can embed configuration details, so only this constant is
/// ever rendered into the warning — never the error value itself.
const TRACER_SHUTDOWN_ERROR_CATEGORY: &str =
    "opentelemetry_sdk::trace::SdkTracerProvider::shutdown_failed";

/// Emit the non-fatal tracer-shutdown warning using fixed fields only.
///
/// `_err` is intentionally unused: its `Display` text can carry endpoint or
/// other configuration details, and this warning is exported through JSON logs
/// to the central OTLP/Grafana pipeline. Passing it in keeps the regression
/// test on the same call site as production code.
fn warn_tracer_shutdown_failure(_err: &dyn std::fmt::Display) {
    tracing::warn!(
        error_category = TRACER_SHUTDOWN_ERROR_CATEGORY,
        "tracer provider shutdown failed; queued spans may be dropped"
    );
}

/// Keeps the [`SdkTracerProvider`] alive for the lifetime of `main` and makes
/// sure the batch exporter is shut down on every exit path.
///
/// The provider is cloned into the tracer held by the global tracing layer, so
/// it is not the last `Arc` reference once `init_tracing` returns. Dropping the
/// local handle there would never trigger shutdown, and spans queued at process
/// exit would be lost. Holding this guard in `main` and calling `shutdown`
/// explicitly flushes them; [`Drop`] is the safety net for early `?` returns.
struct TracerGuard {
    provider: SdkTracerProvider,
}

impl TracerGuard {
    /// Flush queued spans. A failure is intentionally non-fatal and is reported
    /// with a fixed error category only, never with the raw SDK error text.
    fn shutdown(&self) {
        if let Err(err) = self.provider.shutdown() {
            warn_tracer_shutdown_failure(&err);
        }
    }
}

impl Drop for TracerGuard {
    fn drop(&mut self) {
        // Safety net for error paths that return early via `?`. A repeated
        // shutdown is a harmless `AlreadyShutdown` error and is ignored here so
        // the normal path, where `main` already called `shutdown()`, stays quiet.
        let _ = self.provider.shutdown();
    }
}

/// Collector endpoint used when `OTEL_EXPORTER_OTLP_ENDPOINT` is unset.
const OTLP_DEFAULT_ENDPOINT: &str = "http://127.0.0.1:4317";

/// Fixed category for a failed OTLP exporter build (I49-A).
///
/// The raw `ExporterBuildError` can embed the configured endpoint, so only
/// this constant is ever rendered into the warning. The name matches
/// `jarvis-policy`'s `OTLP_EXPORTER_ERROR_CATEGORY` for a shared vocabulary.
const OTLP_EXPORTER_ERROR_CATEGORY: &str = "opentelemetry_otlp::SpanExporter::build_failed";

/// Emit the non-fatal OTLP exporter failure warning using fixed fields only.
///
/// `_endpoint` is intentionally unused: the URL may contain userinfo
/// (`https://user:token@host`), a query token, or a path, and this warning is
/// exported through JSON logs to the central OTLP/Grafana pipeline. Passing it
/// in keeps the regression test on the same call site as production code.
fn warn_otlp_exporter_failure(endpoint_configured: bool, _endpoint: &str) {
    tracing::warn!(
        endpoint_configured = endpoint_configured,
        error_category = OTLP_EXPORTER_ERROR_CATEGORY,
        "OTLP trace exporter initialisation failed; continuing without trace export"
    );
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

    // I49-A: record only whether a custom endpoint was configured. The URL
    // itself may carry credentials and must never become a log field.
    let endpoint_result = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT");
    let endpoint_configured = endpoint_result.is_ok();
    let endpoint = endpoint_result.unwrap_or_else(|_| OTLP_DEFAULT_ENDPOINT.to_string());

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build();

    // I49-A: keep the build Result and degrade to a warning instead of
    // propagating `ExporterBuildError` through `?` into stderr/journal. The raw
    // error may embed the endpoint, so it is discarded rather than logged.
    // Keep a provider on every path so `TracerGuard::shutdown` stays
    // meaningful. Without an exporter it simply has no span processors.
    let (provider, telemetry, exporter_failed) = match exporter {
        Ok(exporter) => {
            let provider = SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .build();
            let tracer = provider.tracer(service);
            let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
            (provider, Some(telemetry), false)
        }
        Err(_) => (SdkTracerProvider::builder().build(), None, true),
    };

    // The global default subscriber is installed exactly once, as before.
    tracing_subscriber::registry()
        .with(telemetry)
        .with(tracing_subscriber::fmt::layer().json())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Warn only after the fmt layer is installed; before that there is no
    // subscriber and the message would be lost.
    if exporter_failed {
        warn_otlp_exporter_failure(endpoint_configured, &endpoint);
    }

    Ok(TracerGuard { provider })
}

#[tokio::main]
async fn main() -> Result<()> {
    let tracer = init_tracing("jarvis-event")?;
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

    // Explicit shutdown on the normal exit path; `TracerGuard::drop` remains
    // the safety net for the early `?` returns above.
    tracer.shutdown();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        TRACER_SHUTDOWN_ERROR_CATEGORY, TracerGuard, warn_otlp_exporter_failure,
        warn_tracer_shutdown_failure,
    };
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    /// In-memory sink that lets a test read back the JSON warning bytes.
    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer mutex poisoned")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Render the real warning through a JSON fmt layer into `LogBuffer`.
    fn capture_failure_warning(endpoint: &str) -> String {
        let buffer = LogBuffer::default();
        {
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(buffer.clone()),
            );
            tracing::subscriber::with_default(subscriber, || {
                warn_otlp_exporter_failure(true, endpoint);
            });
        }
        String::from_utf8(buffer.0.lock().expect("log buffer mutex poisoned").clone())
            .expect("captured log must be UTF-8")
    }

    /// I49-A regression: a credential-bearing endpoint must not leak its
    /// userinfo, query token, or path into the OTLP failure warning.
    #[test]
    fn otlp_failure_warning_does_not_leak_endpoint_probe_token() {
        const PROBE: &str = "PROBE-I49A-3c1e7a52";
        let endpoint = format!("https://u:{PROBE}@h.example:4318/v1/traces?t={PROBE}&s={PROBE}");

        let log = capture_failure_warning(&endpoint);

        assert!(
            !log.contains(PROBE),
            "PROBE token leaked into OTLP failure warning: {log}"
        );
        assert!(
            !log.contains("h.example"),
            "endpoint host leaked into OTLP failure warning: {log}"
        );
        assert!(
            log.contains("endpoint_configured"),
            "fixed endpoint field missing: {log}"
        );
        assert!(
            log.contains(super::OTLP_EXPORTER_ERROR_CATEGORY),
            "fixed error category missing: {log}"
        );
    }

    /// Render the real tracer-shutdown warning through a JSON fmt layer.
    fn capture_shutdown_warning(raw_err: &str) -> String {
        let buffer = LogBuffer::default();
        {
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(buffer.clone()),
            );
            tracing::subscriber::with_default(subscriber, || {
                warn_tracer_shutdown_failure(&raw_err);
            });
        }
        String::from_utf8(buffer.0.lock().expect("log buffer mutex poisoned").clone())
            .expect("captured log must be UTF-8")
    }

    /// I51-B regression: `init_tracing` returns a guard that owns the provider,
    /// and dropping that guard shuts the provider down so queued spans flush on
    /// every early-return path.
    #[test]
    fn tracer_guard_holds_provider_and_drop_shuts_it_down() {
        let provider = SdkTracerProvider::builder().build();
        // `SdkTracerProvider` is a clonable handle to the same provider state.
        // Keeping a clone lets us observe that the guard's `Drop` called
        // `shutdown` (the second call then reports `AlreadyShutdown`).
        let observer = provider.clone();

        {
            let _guard = TracerGuard { provider };
        }

        assert!(
            observer.shutdown().is_err(),
            "TracerGuard::drop did not shut down the retained provider"
        );
    }

    /// I51-B regression: a shutdown error must never leak its raw `Display`
    /// text (which may embed endpoint/config details) into the JSON warning.
    #[test]
    fn tracer_shutdown_warning_uses_fixed_vocabulary_only() {
        const PROBE: &str = "PROBE-I51B-2f8d6b31";
        let raw_err =
            format!("sdk shutdown failed at https://u:{PROBE}@h.example:4317/v1/traces?t={PROBE}");

        let log = capture_shutdown_warning(&raw_err);

        assert!(
            !log.contains(PROBE),
            "raw shutdown error leaked into warning: {log}"
        );
        assert!(
            !log.contains("h.example"),
            "endpoint host leaked into warning: {log}"
        );
        assert!(
            log.contains(TRACER_SHUTDOWN_ERROR_CATEGORY),
            "fixed error category missing: {log}"
        );
    }
}
