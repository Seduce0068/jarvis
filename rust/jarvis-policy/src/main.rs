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

/// Why waiting for the two listeners ended.
enum ListenerOutcome {
    /// Both listeners ended on their own; their results still need the
    /// existing one-error / both-errors aggregation.
    Finished((Result<()>, Result<()>)),
    /// A termination signal arrived first. `select!` dropped the listener
    /// futures, which stops both accept loops before the shutdown path below.
    ///
    /// I54-D — adopted policy: IMMEDIATE STOP, not a graceful drain. The HTTP
    /// handlers (including an in-flight `ApprovalStore::decide` commit) are
    /// spawned tasks not owned by these listener futures; they are not tracked
    /// in a `JoinSet` here. This branch therefore does not wait for them, and
    /// they are not guaranteed to finish before runtime teardown or the unit's
    /// `TimeoutStopSec` SIGKILL. The shutdown log is worded accordingly and
    /// must not claim a clean completion of in-flight work.
    Shutdown,
}

/// Resolves when the process is asked to stop.
///
/// `Ctrl-C` covers interactive stops, while systemd sends `SIGTERM` from
/// `systemctl stop`. Both are registered here so the `select!` in `main` stays
/// platform-independent; a registration failure degrades to the other signal
/// instead of blocking a clean shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            eprintln!("jarvis-policy: failed to listen for Ctrl-C: {err}");
        }
    };

    #[cfg(unix)]
    {
        let terminate = tokio::signal::unix::SignalKind::terminate();
        let mut sigterm = match tokio::signal::unix::signal(terminate) {
            Ok(signal) => signal,
            Err(err) => {
                eprintln!("jarvis-policy: failed to listen for SIGTERM: {err}");
                ctrl_c.await;
                return;
            }
        };

        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}

/// Collector endpoint used when `OTEL_EXPORTER_OTLP_ENDPOINT` is unset.
const OTLP_DEFAULT_ENDPOINT: &str = "http://127.0.0.1:4317";

/// Fixed category for a failed OTLP exporter build (I47-B).
///
/// The raw `ExporterBuildError` can embed the configured endpoint, so only
/// this constant is ever rendered into the warning.
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

    // I47-B: record only whether a custom endpoint was configured. The URL
    // itself may carry credentials and must never become a log field.
    let endpoint_result = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT");
    let endpoint_configured = endpoint_result.is_ok();
    let endpoint = endpoint_result.unwrap_or_else(|_| OTLP_DEFAULT_ENDPOINT.to_string());

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build();

    // Keep a provider on every path so `TracerGuard::shutdown` stays
    // meaningful. Without an exporter it simply has no span processors.
    // I47-B: carry only a failure flag forward. The raw build error may embed
    // the endpoint, so it is discarded instead of being stored for logging.
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
    let tracer = init_tracing("jarvis-policy")?;
    jarvis_policy::http::install_metrics()?;

    let config = config::PolicyConfig::load()?;

    // I30-A: report the active approval backend without logging `pg_url` itself
    // (the URL may carry credentials). The actual injection happens in
    // `PolicyEngine::new`, which uses the in-memory/Noop default when unset.
    if config
        .pg_url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty())
    {
        info!("approval persistence backend: PostgreSQL (durable CAS + audit)");
    } else {
        info!("approval persistence backend: in-memory (pg_url unset)");
    }

    let engine = Arc::new(PolicyEngine::new(config.clone()).await?);

    info!(
        "Starting JARVIS Policy Engine on gRPC {} HTTP {}",
        config.grpc_addr, config.http_addr
    );

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    // I54-D — approval commit boundary contract (unchanged by immediate stop):
    // `ApprovalStore::decide` re-reads the synchronous `commit_gate` — the same
    // authoritative kill-switch instance the transports arm/disarm — once
    // immediately before the durable write, and the in-flight claim is held by
    // a generation-tagged `DecidingGuard` whose `Drop` releases it even when a
    // future is cancelled (a competing commit cannot clear another owner's
    // generation). The gate is deliberately NOT re-read after a successful
    // commit: the committed row is the authority and must be published. A
    // later re-evaluation via `PolicyEngine::is_allowed` re-checks the kill
    // switch and is fail-closed, so a decision that committed just before
    // SIGTERM cannot grant tool execution while the stop is engaged; a retry
    // on the same id stays `AlreadyDecided` (409), not a second commit.
    // Immediate stop below does not alter this contract; it only means a
    // spawned handler may be cut short at teardown / `TimeoutStopSec`.

    // `join!` polls both listeners to completion, so a failure in one never
    // drops the other's future. Racing it against the shutdown signal lets a
    // signal stop both listeners by dropping the `join!` future, while a
    // listener failure still completes the branch and keeps the existing
    // error aggregation. `biased;` gives the listener branch priority so a
    // real failure is reported rather than masked by a coincident signal.
    let grpc = engine.serve_grpc(grpc_addr);
    let http = engine.serve_http(http_addr);
    let outcome = tokio::select! {
        biased;
        results = async { tokio::join!(grpc, http) } => ListenerOutcome::Finished(results),
        _ = shutdown_signal() => {
            info!("shutdown signal received; stopping listeners");
            ListenerOutcome::Shutdown
        }
    };

    // Explicit shutdown on every exit path after the listeners stop;
    // `TracerGuard::drop` remains the safety net for the early `?` returns.
    tracer.shutdown();

    match outcome {
        ListenerOutcome::Finished((grpc_result, http_result)) => match (grpc_result, http_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(err), Ok(())) | (Ok(()), Err(err)) => Err(err),
            (Err(grpc_err), Err(http_err)) => Err(anyhow::anyhow!(
                "gRPC listener failed: {grpc_err}; HTTP listener failed: {http_err}"
            )),
        },
        ListenerOutcome::Shutdown => {
            // I54-D: fixed vocabulary for the immediate-stop policy. Do not
            // claim a clean exit: spawned handlers (including an in-flight
            // approval commit) are not drained here and may still be running
            // or be SIGKILLed at `TimeoutStopSec` after this line.
            info!(
                shutdown_policy = "immediate-stop",
                in_flight_requests_drained = false,
                "listeners stopped, exiting without draining in-flight requests"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::warn_otlp_exporter_failure;
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

    /// I47-B regression: a credential-bearing endpoint must not leak its
    /// userinfo, query token, or path into the OTLP failure warning.
    #[test]
    fn otlp_failure_warning_does_not_leak_endpoint_probe_token() {
        const PROBE: &str = "PROBE-I47B-9d4f2a7c";
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
}
