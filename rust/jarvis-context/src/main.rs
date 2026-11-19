//! Main binary entry point

use anyhow::Result;
use jarvis_context::ContextBuilder;
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

/// Handle the result of a `SdkTracerProvider::shutdown` attempt, shared by the
/// explicit path and [`TracerGuard::drop`].
///
/// `AlreadyShutdown` is the benign duplicate-shutdown signal produced when the
/// explicit path already flushed spans; it stays silent so the normal exit is
/// quiet. Every other error means queued spans may have been lost and is
/// surfaced through the fixed-category warning. This is the only trace left on
/// an early `?` return, where `Drop` is the sole shutdown attempt.
fn report_shutdown_result(result: opentelemetry_sdk::error::OTelSdkResult) {
    use opentelemetry_sdk::error::OTelSdkError;
    match result {
        Err(err) if !matches!(&err, OTelSdkError::AlreadyShutdown) => {
            warn_tracer_shutdown_failure(&err);
        }
        _ => {}
    }
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
    /// Flush queued spans. A failure is intentionally non-fatal. A repeat
    /// `AlreadyShutdown` is benign; any other failure is reported with a fixed
    /// error category only, never with the raw SDK error text.
    fn shutdown(&self) {
        report_shutdown_result(self.provider.shutdown());
    }
}

impl Drop for TracerGuard {
    fn drop(&mut self) {
        // Safety net for error paths that return early via `?`: `main` never
        // reaches `complete_shutdown`, so this is the only shutdown attempt and
        // a real failure must leave a warning. A repeat `AlreadyShutdown` after
        // the explicit path stays quiet via `report_shutdown_result`.
        self.shutdown();
    }
}

/// Why waiting for the listener futures ended.
enum ListenerOutcome {
    /// The listeners finished on their own. The existing `try_join!` result is
    /// preserved unchanged so its one-error aggregation stays intact.
    Finished(Result<()>),
    /// A termination signal arrived first. `select!` dropped the listener
    /// futures, which stops both accept loops before the shutdown path below.
    ///
    /// I54-D — adopted policy: IMMEDIATE STOP, not a graceful drain. The
    /// HTTP handler tasks spawned by the listeners are not owned by these
    /// futures and are not collected in a `JoinSet`, so this branch neither
    /// awaits nor explicitly cancels them. In-flight work is NOT guaranteed
    /// to complete: it can be cut short by runtime teardown or by the unit's
    /// `TimeoutStopSec` SIGKILL. The shutdown log is worded accordingly and
    /// must not claim that in-flight requests finished.
    Shutdown,
}

/// Resolves when the process is asked to stop.
///
/// `Ctrl-C` covers interactive stops, while systemd sends `SIGTERM` from
/// `systemctl stop`. Both are registered here so the race stays
/// platform-independent; a registration failure degrades to the other signal
/// instead of blocking a clean shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            eprintln!("jarvis-context: failed to listen for Ctrl-C: {err}");
        }
    };

    #[cfg(unix)]
    {
        let terminate = tokio::signal::unix::SignalKind::terminate();
        let mut sigterm = match tokio::signal::unix::signal(terminate) {
            Ok(signal) => signal,
            Err(err) => {
                eprintln!("jarvis-context: failed to listen for SIGTERM: {err}");
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

/// Race the listener futures against the shutdown signal.
///
/// `biased;` gives the listener branch priority so a real listener failure is
/// reported instead of being masked by a coincident signal. When the signal
/// wins, `select!` drops the pending listener future and every accept loop
/// stops before the explicit shutdown below.
async fn race_listeners_with_shutdown<F, S>(listeners: F, signal: S) -> ListenerOutcome
where
    F: std::future::Future<Output = Result<()>>,
    S: std::future::Future,
{
    tokio::select! {
        biased;
        result = listeners => ListenerOutcome::Finished(result),
        _ = signal => {
            info!("shutdown signal received; stopping listeners");
            ListenerOutcome::Shutdown
        }
    }
}

/// Shut the tracer down on every post-listener exit path, then translate the
/// outcome into the process exit status.
///
/// Fixed vocabulary only: a real listener error is returned unchanged to the
/// caller instead of being re-logged, and no endpoint/config value is ever
/// formatted into a shutdown message.
fn complete_shutdown(outcome: ListenerOutcome, tracer: &TracerGuard) -> Result<()> {
    // Explicit shutdown after the listeners stop; `TracerGuard::drop` remains
    // the safety net for the early `?` returns above.
    tracer.shutdown();

    match outcome {
        ListenerOutcome::Finished(result) => result,
        ListenerOutcome::Shutdown => {
            // I54-D: fixed vocabulary for the immediate-stop policy. Never log
            // "exiting cleanly" here: spawned handlers are not drained and may
            // still be running (or be SIGKILLed at `TimeoutStopSec`) after this
            // line, so claiming a clean completion of in-flight work is false.
            info!(
                shutdown_policy = "immediate-stop",
                in_flight_requests_drained = false,
                "listeners stopped, exiting without draining in-flight requests"
            );
            Ok(())
        }
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
    let tracer = init_tracing("jarvis-context")?;
    jarvis_context::http::install_metrics()?;

    let config = jarvis_context::config::ContextConfig::load()?;
    let builder = ContextBuilder::new(config.clone()).await?;

    info!(
        "Starting JARVIS Context Builder on gRPC {} HTTP {}",
        config.grpc_addr, config.http_addr
    );

    let grpc_addr = config.grpc_addr.parse()?;
    let http_addr = config.http_addr.parse()?;

    // Race both listeners against a termination signal. `biased;` keeps the
    // listener branch first so a real failure is reported instead of being
    // masked by a coincident signal; when the signal wins, the listener futures
    // are dropped and both accept loops stop before the explicit shutdown
    // below. No in-flight handler drain is performed (I54-D immediate stop).
    let outcome = race_listeners_with_shutdown(
        async {
            tokio::try_join!(builder.serve_grpc(grpc_addr), builder.serve_http(http_addr),)
                .map(|_| ())
        },
        shutdown_signal(),
    )
    .await;

    complete_shutdown(outcome, &tracer)
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
        const PROBE: &str = "PROBE-I49A-8b6d0f13";
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
        const PROBE: &str = "PROBE-I51B-7a4c1e09";
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

    /// A span processor whose shutdown always fails. Used to reproduce the
    /// early-`?` exit where `TracerGuard::drop` is the only shutdown attempt.
    #[derive(Debug)]
    struct FailingShutdownProcessor;

    impl opentelemetry_sdk::trace::SpanProcessor for FailingShutdownProcessor {
        fn on_start(
            &self,
            _span: &mut opentelemetry_sdk::trace::Span,
            _cx: &opentelemetry::Context,
        ) {
            // No-op: this processor exists only to make shutdown fail.
        }

        fn on_end(&self, _span: opentelemetry_sdk::trace::SpanData) {}

        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(
            &self,
            _timeout: std::time::Duration,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
                "I53D-drop-shutdown-failure".into(),
            ))
        }
    }

    /// I53-D regression: an early `?` return never reaches `complete_shutdown`,
    /// so `TracerGuard::drop` is the only shutdown attempt. A real failure there
    /// must leave the fixed warning instead of being silently discarded.
    #[test]
    fn drop_warns_on_shutdown_failure_on_early_return_path() {
        let buffer = LogBuffer::default();
        {
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(buffer.clone()),
            );
            tracing::subscriber::with_default(subscriber, || {
                let provider = SdkTracerProvider::builder()
                    .with_span_processor(FailingShutdownProcessor)
                    .build();
                // Simulate an early `?` return: no explicit shutdown, so Drop is
                // the sole shutdown attempt.
                {
                    let _guard = TracerGuard { provider };
                }
            });
        }
        let log = String::from_utf8(buffer.0.lock().expect("log buffer mutex poisoned").clone())
            .expect("captured log must be UTF-8");
        // The OpenTelemetry SDK itself emits a DEBUG line carrying the raw
        // error. Our contract covers our own WARN record only.
        let app_warn: String = log
            .lines()
            .filter(|line| line.contains("\"level\":\"WARN\""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            app_warn.contains(TRACER_SHUTDOWN_ERROR_CATEGORY),
            "early-return Drop swallowed the shutdown failure: {app_warn}"
        );
        assert!(
            !app_warn.contains("I53D-drop-shutdown-failure"),
            "raw SDK shutdown error leaked through Drop warning: {app_warn}"
        );
    }

    /// The normal path already flushed spans, so Drop's repeat attempt returns
    /// `AlreadyShutdown`; that benign case must not emit a duplicate warning.
    #[test]
    fn drop_stays_quiet_for_already_shutdown() {
        let buffer = LogBuffer::default();
        {
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(buffer.clone()),
            );
            tracing::subscriber::with_default(subscriber, || {
                let provider = SdkTracerProvider::builder().build();
                let guard = TracerGuard { provider };
                // First (explicit) shutdown succeeds; the Drop repeat is the
                // benign AlreadyShutdown case.
                guard.shutdown();
            });
        }
        let log = String::from_utf8(buffer.0.lock().expect("log buffer mutex poisoned").clone())
            .expect("captured log must be UTF-8");
        assert!(
            !log.contains(TRACER_SHUTDOWN_ERROR_CATEGORY),
            "benign AlreadyShutdown must stay silent: {log}"
        );
    }

    /// Drop probe used by the shutdown regression test: the flag is set when
    /// `tokio::select!` cancels (drops) the pending listener future.
    struct ListenerProbe(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for ListenerProbe {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// I52-C regression: a termination signal wins the listener race, the
    /// pending listener future is dropped (accept loops stop), and the
    /// shutdown outcome still routes through the explicit tracer shutdown so
    /// queued spans are flushed on the signal path.
    #[tokio::test]
    async fn signal_stops_listeners_and_shuts_tracer_down() {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = dropped.clone();
        let listeners = async move {
            let _probe = ListenerProbe(probe);
            std::future::pending::<anyhow::Result<()>>().await
        };

        // `biased;` polls the listener branch first; it never completes, so the
        // ready signal branch wins and `select!` drops the listener future.
        let outcome = super::race_listeners_with_shutdown(listeners, std::future::ready(())).await;
        assert!(
            matches!(&outcome, super::ListenerOutcome::Shutdown),
            "a ready shutdown signal must win over a pending listener"
        );
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "listener future was not dropped when the shutdown signal fired"
        );

        // The shutdown outcome must go through the explicit tracer shutdown.
        let provider = SdkTracerProvider::builder().build();
        let observer = provider.clone();
        let tracer = TracerGuard { provider };
        assert!(
            super::complete_shutdown(outcome, &tracer).is_ok(),
            "clean shutdown must exit successfully"
        );
        assert!(
            observer.shutdown().is_err(),
            "shutdown outcome did not shut the tracer down"
        );
    }
}
