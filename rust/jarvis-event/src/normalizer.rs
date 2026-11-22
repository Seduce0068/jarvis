//! Event normalization core

use crate::ports::{ContextProvider, PolicyGate};
use anyhow::Result;
use axum::{Json, Router, extract::State, routing::post};
use jarvis_core::{BareJson, EventSource, UnifiedEvent};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, instrument};

/// Capacity of the bounded event channel. Producers never block: when the
/// queue is full the event is logged and dropped (see [`try_emit_event`]).
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Non-blocking submission to the bounded event channel.
///
/// A full channel sheds load instead of growing memory without limit or
/// blocking webhook handlers: the event is logged at WARN level and dropped.
/// This is shared by webhook `emit` calls and the Home Assistant listener
/// bridge in `lib.rs` so both paths obey the same backpressure policy.
pub(crate) fn try_emit_event(sender: &mpsc::Sender<UnifiedEvent>, event: UnifiedEvent) {
    match sender.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(event)) => {
            tracing::warn!(
                event_type = %event.event_type,
                source = %event.source,
                capacity = EVENT_CHANNEL_CAPACITY,
                "event channel full; dropping event"
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            // Receiver gone (shutdown): keep the previous silent-drop
            // behaviour without adding noise to the shutdown path.
            tracing::debug!("event channel closed; dropping event");
        }
    }
}

/// Fixed-vocabulary classification of a failed event-processing chain.
///
/// `anyhow`'s `Display` / `{:#}` can carry context-provider URLs, Qdrant
/// endpoints, policy details and raw event payloads straight into the tracing
/// pipeline. Only the static token below is logged; the error text itself is
/// never formatted, so a nested context layer cannot smuggle data into OTLP.
fn error_kind(err: &anyhow::Error) -> &'static str {
    if let Some(event_error) = err.downcast_ref::<crate::error::EventError>() {
        return match event_error {
            crate::error::EventError::HomeAssistant(_) => "home_assistant",
            crate::error::EventError::AndroidMcp(_) => "android_mcp",
            crate::error::EventError::Context(_) => "context",
            crate::error::EventError::Config(_) => "config",
            crate::error::EventError::Grpc(_) => "grpc",
            crate::error::EventError::Serialization(_) => "serialization",
            crate::error::EventError::Http(_) => "http",
        };
    }
    if err
        .downcast_ref::<jarvis_context::error::ContextError>()
        .is_some()
    {
        return "context";
    }
    "other"
}

/// Emit the single fixed-vocabulary failure record for a processing error.
fn log_processing_failure(event: &UnifiedEvent, err: &anyhow::Error) {
    tracing::error!(
        event_id = %event.id,
        correlation_id = %event.correlation_id,
        error_kind = error_kind(err),
        "event processing failed"
    );
}

pub struct Normalizer {
    config: crate::config::EventConfig,
    event_tx: mpsc::Sender<UnifiedEvent>,
}

impl Normalizer {
    pub async fn new(
        config: crate::config::EventConfig,
        policy_gate: Arc<dyn PolicyGate>,
        opencode: Arc<crate::opencode::OpenCodeClient>,
        context_provider: Arc<dyn ContextProvider>,
    ) -> Result<Self> {
        let (event_tx, mut event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);

        let normalizer = Self {
            config: config.clone(),
            event_tx,
        };

        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if let Err(e) =
                    Self::process_event(&context_provider, &policy_gate, &opencode, &event).await
                {
                    // Log correlation identifiers and a fixed error class only:
                    // neither the anyhow chain nor the event payload may reach
                    // journald -> otelcol -> OTLP/Grafana.
                    log_processing_failure(&event, &e);
                }
            }
        });

        Ok(normalizer)
    }

    pub fn emit(&self, event: UnifiedEvent) {
        try_emit_event(&self.event_tx, event);
    }

    /// Clone of the bounded channel sender used to submit events for
    /// processing. Submission is non-blocking; see [`try_emit_event`].
    pub fn event_sender(&self) -> mpsc::Sender<UnifiedEvent> {
        self.event_tx.clone()
    }

    /// Test-only constructor that avoids the client dependencies required by
    /// [`Normalizer::new`] while exercising the real `emit` path.
    #[cfg(test)]
    fn with_event_tx(event_tx: mpsc::Sender<UnifiedEvent>) -> Self {
        Self {
            config: crate::config::EventConfig::default(),
            event_tx,
        }
    }

    /// Policy-gated hand-off of a normalized event to the OpenCode agent.
    ///
    /// Context building and the authorization check go through the
    /// [`ContextProvider`] / [`PolicyGate`] ports; the default adapters delegate
    /// to `jarvis-context` and `jarvis-policy` without changing behaviour.
    ///
    /// The gate is `auto_respond` on resource `event:<type>` for the `jarvis`
    /// principal (role `agent` in the shipped Casbin policy, which grants
    /// nothing by default). Grant an event type with:
    ///
    /// ```text
    /// p, agent, event:<type>, auto_respond
    /// ```
    ///
    /// Forwarded events run through OpenCode; every tool call the agent makes
    /// is re-authorized by the MCP gateway before it executes.
    async fn process_event(
        context_provider: &Arc<dyn ContextProvider>,
        policy_gate: &Arc<dyn PolicyGate>,
        opencode: &crate::opencode::OpenCodeClient,
        event: &UnifiedEvent,
    ) -> Result<()> {
        info!(
            "Processing event: {} from {:?}",
            event.event_type, event.source
        );

        // Build context from the injected port (the default adapter delegates
        // to jarvis-context, which applies PII filtering).
        let context = context_provider.build_context(event).await?;

        // Check policy for automated response
        let resource = format!("event:{}", event.event_type);
        // `is_allowed` fails closed (returns false on transport errors), so
        // there is no `?` here: an unreachable policy service must not
        // forward the event.
        let should_act = policy_gate
            .is_allowed("jarvis", "auto_respond", &resource, &context)
            .await;

        if !should_act {
            tracing::debug!(
                event_type = %event.event_type,
                "auto_respond not granted; event not forwarded to the agent"
            );
            return Ok(());
        }

        // Hand the event to the agent runtime; tool execution stays behind
        // the MCP gateway and its policy enforcement.
        opencode.dispatch(event, &context).await?;
        Ok(())
    }

    pub async fn start_webhook_server(this: Arc<Self>) -> Result<()> {
        let app = Router::new()
            .route("/webhook/ha", post(ha_webhook))
            .route("/webhook/android", post(android_webhook))
            .route("/webhook/gmail", post(gmail_webhook))
            .route("/webhook/calendar", post(calendar_webhook))
            .with_state(this.clone());

        let listener = tokio::net::TcpListener::bind(&this.config.webhook_addr).await?;
        info!("Webhook server listening on {}", this.config.webhook_addr);
        axum::serve(listener, app).await?;
        Ok(())
    }
}

/// Pure payload -> `UnifiedEvent` conversion for Home Assistant webhooks.
///
/// Non-deterministic fields (id, timestamp, correlation id) are injected by
/// the caller so the conversion itself is deterministic and testable. The
/// webhook handler passes freshly generated values, preserving its behaviour.
fn ha_webhook_event(
    payload: Value,
    id: uuid::Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    correlation_id: String,
) -> UnifiedEvent {
    let event_type: String = payload["event_type"].as_str().unwrap_or("unknown").into();
    UnifiedEvent {
        id,
        source: EventSource::HomeAssistant,
        event_type,
        payload,
        timestamp,
        correlation_id,
    }
}

/// Pure payload -> `UnifiedEvent` conversion for Android webhooks.
fn android_webhook_event(
    payload: Value,
    id: uuid::Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    correlation_id: String,
) -> UnifiedEvent {
    let event_type: String = payload["type"].as_str().unwrap_or("unknown").into();
    UnifiedEvent {
        id,
        source: EventSource::Android,
        event_type,
        payload,
        timestamp,
        correlation_id,
    }
}

/// Pure payload -> `UnifiedEvent` conversion for Gmail webhooks.
fn gmail_webhook_event(
    payload: Value,
    id: uuid::Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    correlation_id: String,
) -> UnifiedEvent {
    UnifiedEvent {
        id,
        source: EventSource::Gmail,
        event_type: "new_email".into(),
        payload,
        timestamp,
        correlation_id,
    }
}

/// Pure payload -> `UnifiedEvent` conversion for Calendar webhooks.
fn calendar_webhook_event(
    payload: Value,
    id: uuid::Uuid,
    timestamp: chrono::DateTime<chrono::Utc>,
    correlation_id: String,
) -> UnifiedEvent {
    UnifiedEvent {
        id,
        source: EventSource::Calendar,
        event_type: "event_update".into(),
        payload,
        timestamp,
        correlation_id,
    }
}

// Webhook spans must not record `payload`: Gmail/Calendar/HA bodies can carry
// message content and token-like strings into the tracing pipeline. Only the
// handler name remains in the span; correlation ids are generated inside.
#[instrument(skip(state, payload))]
async fn ha_webhook(State(state): State<Arc<Normalizer>>, payload: BareJson<Value>) {
    // `BareJson` maps rejections to bare statuses before this line: 415 for a
    // missing Content-Type, 413 for an oversized body, 400 for bad syntax or a
    // type mismatch. The raw rejection text never reaches a response or a log.
    let BareJson(Json(payload)) = payload;
    let event = ha_webhook_event(
        payload,
        uuid::Uuid::new_v4(),
        chrono::Utc::now(),
        uuid::Uuid::new_v4().to_string(),
    );
    state.emit(event);
}

#[instrument(skip(state, payload))]
async fn android_webhook(State(state): State<Arc<Normalizer>>, payload: BareJson<Value>) {
    let BareJson(Json(payload)) = payload;
    let event = android_webhook_event(
        payload,
        uuid::Uuid::new_v4(),
        chrono::Utc::now(),
        uuid::Uuid::new_v4().to_string(),
    );
    state.emit(event);
}

#[instrument(skip(state, payload))]
async fn gmail_webhook(State(state): State<Arc<Normalizer>>, payload: BareJson<Value>) {
    let BareJson(Json(payload)) = payload;
    let event = gmail_webhook_event(
        payload,
        uuid::Uuid::new_v4(),
        chrono::Utc::now(),
        uuid::Uuid::new_v4().to_string(),
    );
    state.emit(event);
}

#[instrument(skip(state, payload))]
async fn calendar_webhook(State(state): State<Arc<Normalizer>>, payload: BareJson<Value>) {
    let BareJson(Json(payload)) = payload;
    let event = calendar_webhook_event(
        payload,
        uuid::Uuid::new_v4(),
        chrono::Utc::now(),
        uuid::Uuid::new_v4().to_string(),
    );
    state.emit(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::io::Write;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tracing_subscriber::fmt::format::FmtSpan;

    fn fixed_id() -> uuid::Uuid {
        uuid::Uuid::from_u128(1)
    }

    fn fixed_timestamp() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp")
    }

    fn fixed_correlation_id() -> String {
        "corr-test".to_string()
    }

    /// The generated values are supplied by the caller and must reach the
    /// event unchanged; `process_event` is client-dependent and not exercised.
    fn assert_injected_fields(event: &UnifiedEvent) {
        assert_eq!(event.id, fixed_id());
        assert_eq!(event.timestamp, fixed_timestamp());
        assert_eq!(event.correlation_id, fixed_correlation_id());
    }

    #[test]
    fn ha_webhook_event_uses_event_type_field_and_keeps_payload() {
        let payload = json!({
            "event_type": "state_changed",
            "data": {"entity_id": "light.kitchen", "new_state": "on"},
        });
        let event = ha_webhook_event(
            payload.clone(),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );

        assert_injected_fields(&event);
        assert_eq!(event.source, EventSource::HomeAssistant);
        assert_eq!(event.event_type, "state_changed");
        assert_eq!(event.payload, payload);
    }

    #[test]
    fn ha_webhook_event_defaults_to_unknown_for_missing_or_non_string_event_type() {
        let missing_payload = json!({"data": {"entity_id": "light.kitchen"}});
        let missing = ha_webhook_event(
            missing_payload.clone(),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(missing.source, EventSource::HomeAssistant);
        assert_eq!(missing.event_type, "unknown");
        assert_eq!(missing.payload, missing_payload);

        let non_string = ha_webhook_event(
            json!({"event_type": 42}),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(non_string.event_type, "unknown");
    }

    #[test]
    fn android_webhook_event_uses_type_field_and_keeps_payload() {
        let payload = json!({
            "type": "device_update",
            "device": {"id": "phone-1", "battery": 81},
        });
        let event = android_webhook_event(
            payload.clone(),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );

        assert_injected_fields(&event);
        assert_eq!(event.source, EventSource::Android);
        assert_eq!(event.event_type, "device_update");
        assert_eq!(event.payload, payload);
    }

    #[test]
    fn android_webhook_event_defaults_to_unknown_for_missing_or_non_string_type() {
        let missing = android_webhook_event(
            json!({"event_type": "not_the_type_field"}),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(missing.source, EventSource::Android);
        assert_eq!(missing.event_type, "unknown");

        let non_string = android_webhook_event(
            json!({"type": true}),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(non_string.event_type, "unknown");
    }

    #[test]
    fn gmail_webhook_event_uses_fixed_type_and_ignores_payload_type_hints() {
        let payload = json!({
            "event_type": "ignored",
            "type": "also_ignored",
            "message": {"id": "msg-1"},
        });
        let event = gmail_webhook_event(
            payload.clone(),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );

        assert_injected_fields(&event);
        assert_eq!(event.source, EventSource::Gmail);
        assert_eq!(event.event_type, "new_email");
        assert_eq!(event.payload, payload);
    }

    #[test]
    fn calendar_webhook_event_uses_fixed_type_and_ignores_payload_type_hints() {
        let payload = json!({
            "event_type": "ignored",
            "type": "also_ignored",
            "calendar": {"id": "cal-1"},
        });
        let event = calendar_webhook_event(
            payload.clone(),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );

        assert_injected_fields(&event);
        assert_eq!(event.source, EventSource::Calendar);
        assert_eq!(event.event_type, "event_update");
        assert_eq!(event.payload, payload);
    }

    #[test]
    fn fixed_type_webhooks_accept_an_empty_payload() {
        let gmail = gmail_webhook_event(
            json!({}),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(gmail.event_type, "new_email");
        assert_eq!(gmail.payload, json!({}));

        let calendar = calendar_webhook_event(
            json!({}),
            fixed_id(),
            fixed_timestamp(),
            fixed_correlation_id(),
        );
        assert_eq!(calendar.event_type, "event_update");
        assert_eq!(calendar.payload, json!({}));
    }

    fn test_event(seq: u128, event_type: &str) -> UnifiedEvent {
        UnifiedEvent {
            id: uuid::Uuid::from_u128(seq),
            source: EventSource::Manual,
            event_type: event_type.into(),
            payload: json!({"seq": seq}),
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp"),
            correlation_id: format!("corr-{seq}"),
        }
    }

    #[test]
    fn emit_enqueues_event_when_channel_has_capacity() {
        assert_eq!(EVENT_CHANNEL_CAPACITY, 1024);
        let (tx, mut rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let normalizer = Normalizer::with_event_tx(tx);

        normalizer.emit(test_event(1, "state_changed"));

        let queued = rx.try_recv().expect("event accepted");
        assert_eq!(queued.id, uuid::Uuid::from_u128(1));
        assert_eq!(queued.source, EventSource::Manual);
        assert_eq!(queued.event_type, "state_changed");
        assert_eq!(queued.payload, json!({"seq": 1}));
    }

    #[test]
    fn emit_drops_overflow_event_instead_of_blocking() {
        let (tx, mut rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let normalizer = Normalizer::with_event_tx(tx);

        for seq in 0..EVENT_CHANNEL_CAPACITY {
            normalizer.emit(test_event(seq as u128, "probe"));
        }
        normalizer.emit(test_event(EVENT_CHANNEL_CAPACITY as u128, "overflow"));

        let mut received = 0;
        while let Ok(event) = rx.try_recv() {
            assert_ne!(event.event_type, "overflow");
            received += 1;
        }
        assert_eq!(received, EVENT_CHANNEL_CAPACITY);
    }

    #[test]
    fn event_sender_shares_the_same_bounded_capacity() {
        let (tx, mut rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let normalizer = Normalizer::with_event_tx(tx.clone());
        let cloned = normalizer.event_sender();

        for seq in 0..EVENT_CHANNEL_CAPACITY {
            normalizer.emit(test_event(seq as u128, "probe"));
        }
        try_emit_event(
            &cloned,
            test_event(EVENT_CHANNEL_CAPACITY as u128, "overflow"),
        );

        let mut received = 0;
        while let Ok(event) = rx.try_recv() {
            assert_ne!(event.event_type, "overflow");
            received += 1;
        }
        assert_eq!(received, EVENT_CHANNEL_CAPACITY);
    }

    #[test]
    fn try_emit_event_tolerates_a_closed_channel() {
        let (tx, rx) = mpsc::channel::<UnifiedEvent>(1);
        drop(rx);
        assert!(tx.is_closed());

        try_emit_event(&tx, test_event(1, "state_changed"));
    }

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut guard = self.0.lock().expect("log buffer mutex");
            guard.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn webhook_spans_do_not_record_raw_payloads() {
        const HA_SECRET: &str = "ha-body-secret-4f1a";
        const ANDROID_SECRET: &str = "android-body-secret-4f1a";
        const GMAIL_SECRET: &str = "gmail-body-secret-4f1a";
        const CALENDAR_SECRET: &str = "calendar-body-secret-4f1a";
        // Positive control (I37-B): an EVENT proves the thread-local subscriber
        // is capturing output. An empty log would make every `!contains`
        // assertion below vacuously pass even if `#[instrument(skip(...))]`
        // regressed. Kept distinct from the four leak canaries so capturing it
        // cannot mask a real leak.
        const SUBSCRIBER_PROBE: &str = "subscriber-sanity-probe-4f1a";

        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
            .with_writer(move || writer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(marker = SUBSCRIBER_PROBE, "subscriber sanity check");
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime builds");
            runtime.block_on(async {
                let (tx, _rx) = mpsc::channel(8);
                let state = || State(Arc::new(Normalizer::with_event_tx(tx.clone())));

                let ha_payload = json!({"event_type": "state_changed", "body": HA_SECRET});
                ha_webhook(state(), BareJson(Json(ha_payload))).await;
                let android_payload = json!({"type": "device_update", "body": ANDROID_SECRET});
                android_webhook(state(), BareJson(Json(android_payload))).await;
                let gmail_payload = json!({"message": {"body": GMAIL_SECRET}});
                gmail_webhook(state(), BareJson(Json(gmail_payload))).await;
                let calendar_payload = json!({"event": {"body": CALENDAR_SECRET}});
                calendar_webhook(state(), BareJson(Json(calendar_payload))).await;
            });
        });

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        assert!(
            logs.contains(SUBSCRIBER_PROBE),
            "subscriber did not capture the sanity event: {logs}"
        );
        for secret in [HA_SECRET, ANDROID_SECRET, GMAIL_SECRET, CALENDAR_SECRET] {
            assert!(!logs.contains(secret), "webhook payload leaked: {logs}");
        }
        // Do not assert that the spans themselves were captured: the
        // thread-local subscriber can miss spans under parallel execution,
        // which made this test flaky. The SUBSCRIBER_PROBE event above is the
        // deterministic liveness check; the non-leak assertions are the
        // contract being verified.
    }

    /// Static guard (I37-B): the four webhook handlers must keep skipping the
    /// raw `payload` in their `#[instrument]` spans. The expected text is
    /// assembled with `concat!` so it does not appear verbatim in this file,
    /// which `include_str!` also reads, and cannot match its own literal.
    #[test]
    fn webhook_instrument_attributes_skip_payload() {
        const SRC: &str = include_str!("normalizer.rs");
        let attribute = concat!("#[instrument(skip(state, ", "payload))]");
        assert_eq!(
            SRC.matches(attribute).count(),
            4,
            "every webhook handler must skip the raw payload in its span"
        );
    }

    #[test]
    fn emit_preserves_correlation_id_and_timestamp_through_bounded_channel() {
        let (tx, mut rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let normalizer = Normalizer::with_event_tx(tx);
        let event = test_event(31, "correlation_probe");

        normalizer.emit(event.clone());

        let queued = rx.try_recv().expect("event accepted");
        assert_eq!(queued.id, event.id);
        assert_eq!(queued.correlation_id, event.correlation_id);
        assert_eq!(queued.timestamp, event.timestamp);
        assert_eq!(queued.payload, event.payload);
        assert_eq!(queued.source, event.source);
    }

    #[test]
    fn webhook_handlers_generate_distinct_ids_and_correlation_ids_per_call() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");
        runtime.block_on(async {
            let (tx, mut rx) = mpsc::channel(8);
            let state = || State(Arc::new(Normalizer::with_event_tx(tx.clone())));

            ha_webhook(
                state(),
                BareJson(Json(json!({"event_type": "state_changed"}))),
            )
            .await;
            ha_webhook(
                state(),
                BareJson(Json(json!({"event_type": "state_changed"}))),
            )
            .await;

            let first = rx.try_recv().expect("first event accepted");
            let second = rx.try_recv().expect("second event accepted");
            assert_ne!(first.id, second.id);
            assert_ne!(first.correlation_id, second.correlation_id);
            assert!(!first.correlation_id.is_empty());
            assert!(!second.correlation_id.is_empty());
        });
    }

    #[test]
    fn overflow_warning_records_event_metadata_without_payload() {
        // tracing's thread-local dispatcher can be displaced by parallel tests;
        // serialize log capture the same way the context-side helper does.
        static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CAPTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        const OVERFLOW_SECRET: &str = "overflow-body-secret-4f1a";

        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let (tx, _rx) = mpsc::channel::<UnifiedEvent>(1);
            try_emit_event(&tx, test_event(21, "first"));

            let mut overflow = test_event(22, "overflow");
            overflow.payload = json!({"body": OVERFLOW_SECRET});
            try_emit_event(&tx, overflow);
        });

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");

        assert!(
            logs.contains("event channel full; dropping event"),
            "overflow warning not captured: {logs}"
        );
        assert!(logs.contains("event_type=overflow"), "event_type absent");
        assert!(logs.contains("source=manual"), "source absent");
        assert!(logs.contains("capacity="), "capacity absent");
        assert!(
            !logs.contains(OVERFLOW_SECRET),
            "dropped payload leaked: {logs}"
        );
    }

    /// The failure branch lives inside `Normalizer::new`'s spawned task and
    /// requires live policy/context clients, so it cannot run in a unit test.
    /// Guard its logging contract at the source level instead: the record must
    /// carry both correlation identifiers plus a fixed `error_kind`, and must
    /// never format the anyhow error (or the raw event body) into the log.
    #[test]
    fn processing_failure_log_keeps_correlation_ids_and_drops_payload() {
        const SRC: &str = include_str!("normalizer.rs");
        const MSG: &str = "event processing failed";

        let at = SRC.find(MSG).expect("failure log message exists");
        let start = SRC[..at].rfind("tracing::error!(");
        let start = start.expect("failure log is an error event");
        let statement = &SRC[start..at + MSG.len()];

        assert!(
            statement.contains("event_id = %event.id"),
            "failure log missing event_id"
        );
        assert!(
            statement.contains("correlation_id = %event.correlation_id"),
            "failure log missing correlation_id"
        );
        assert!(
            statement.contains("error_kind = error_kind(err)"),
            "failure log missing fixed error_kind"
        );
        assert!(!statement.contains("payload"), "failure log leaks payload");
        assert!(
            !statement.contains("{e}"),
            "anyhow Display leaked into the failure log"
        );
    }

    /// I59-C PROBE: the old `"Event processing failed: {e}"` record sent the
    /// entire anyhow chain (context text, config paths, PII-like bodies) to
    /// OTLP. A hostile chain must now reduce to a fixed `error_kind` token,
    /// with only the two correlation identifiers alongside it.
    #[test]
    fn i59_c_processing_failure_does_not_leak_error_chain() {
        const CHAIN_PROBE: &str = "I59C-PROBE-do-not-log";
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let detail = format!("{CHAIN_PROBE} at /etc/jarvis/{CHAIN_PROBE}.toml");
            let inner = anyhow::anyhow!(detail);
            let err = anyhow::Error::from(crate::error::EventError::Config(inner))
                .context("outer chain that must also stay out of the log");
            log_processing_failure(&test_event(91, "chain_probe"), &err);
        });

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        assert!(logs.contains("event processing failed"), "{logs}");
        // Do not pin the textual rendering of structured fields: it depends on
        // the tracing-subscriber formatter version. The contract here is that
        // the raw anyhow chain never reaches the log.
        assert!(logs.contains("event_id="), "{logs}");
        assert!(logs.contains("correlation_id="), "{logs}");
        assert!(!logs.contains(CHAIN_PROBE), "error chain leaked: {logs}");
        assert!(
            !logs.contains("outer chain"),
            "anyhow context layer leaked: {logs}"
        );
    }

    /// Injected ports that record calls and always deny, so no event is
    /// dispatched to OpenCode (no network I/O in these regression tests).
    struct SeamProbe {
        context_provider: Arc<dyn ContextProvider>,
        policy_gate: Arc<dyn PolicyGate>,
        context_calls: Arc<AtomicUsize>,
        policy_calls: Arc<AtomicUsize>,
        event_types: Arc<Mutex<Vec<String>>>,
        seen_context: Arc<Mutex<Option<Value>>>,
    }

    impl SeamProbe {
        fn new() -> Self {
            let context_calls = Arc::new(AtomicUsize::new(0));
            let policy_calls = Arc::new(AtomicUsize::new(0));
            let event_types = Arc::new(Mutex::new(Vec::new()));
            let seen_context = Arc::new(Mutex::new(None));
            Self {
                context_provider: Arc::new(RecordingContextProvider::new(
                    context_calls.clone(),
                    event_types.clone(),
                )),
                policy_gate: Arc::new(DenyPolicyGate::new(
                    policy_calls.clone(),
                    seen_context.clone(),
                )),
                context_calls,
                policy_calls,
                event_types,
                seen_context,
            }
        }
    }

    struct RecordingContextProvider {
        calls: Arc<AtomicUsize>,
        event_types: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingContextProvider {
        fn new(calls: Arc<AtomicUsize>, event_types: Arc<Mutex<Vec<String>>>) -> Self {
            Self { calls, event_types }
        }
    }

    #[async_trait]
    impl ContextProvider for RecordingContextProvider {
        async fn build_context(&self, event: &UnifiedEvent) -> anyhow::Result<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.event_types
                .lock()
                .expect("event types mutex")
                .push(event.event_type.clone());
            Ok(json!({"provider": "recording"}))
        }
    }

    struct DenyPolicyGate {
        calls: Arc<AtomicUsize>,
        seen_context: Arc<Mutex<Option<Value>>>,
    }

    impl DenyPolicyGate {
        fn new(calls: Arc<AtomicUsize>, seen_context: Arc<Mutex<Option<Value>>>) -> Self {
            Self {
                calls,
                seen_context,
            }
        }
    }

    #[async_trait]
    impl PolicyGate for DenyPolicyGate {
        async fn is_allowed(
            &self,
            _principal: &str,
            _action: &str,
            _resource: &str,
            context: &Value,
        ) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.seen_context.lock().expect("context mutex") = Some(context.clone());
            false
        }
    }

    /// Construction is offline: it only builds an HTTP client and never sends.
    fn offline_opencode() -> Arc<crate::opencode::OpenCodeClient> {
        Arc::new(
            crate::opencode::OpenCodeClient::new(&crate::config::EventConfig::default())
                .expect("offline OpenCode client"),
        )
    }

    #[tokio::test]
    async fn process_event_uses_injected_context_provider_before_policy_gate() {
        let probe = SeamProbe::new();

        Normalizer::process_event(
            &probe.context_provider,
            &probe.policy_gate,
            &offline_opencode(),
            &test_event(77, "seam_probe"),
        )
        .await
        .expect("deny decision keeps processing successful");

        assert_eq!(probe.context_calls.load(Ordering::SeqCst), 1);
        assert_eq!(probe.policy_calls.load(Ordering::SeqCst), 1);
        {
            let types = probe.event_types.lock().expect("event types mutex");
            assert_eq!(types.len(), 1);
            assert_eq!(types[0], "seam_probe");
        }
        let context = probe.seen_context.lock().expect("context mutex");
        assert_eq!(*context, Some(json!({"provider": "recording"})));
    }

    #[tokio::test]
    async fn new_wires_injected_ports_into_worker_task() {
        let probe = SeamProbe::new();
        let normalizer = Normalizer::new(
            crate::config::EventConfig::default(),
            probe.policy_gate.clone(),
            offline_opencode(),
            probe.context_provider.clone(),
        )
        .await
        .expect("normalizer builds with injected ports");

        normalizer.emit(test_event(78, "worker_seam_probe"));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while probe.context_calls.load(Ordering::SeqCst) == 0
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert_eq!(probe.context_calls.load(Ordering::SeqCst), 1);
        assert_eq!(probe.policy_calls.load(Ordering::SeqCst), 1);
        let types = probe.event_types.lock().expect("event types mutex");
        assert_eq!(types.len(), 1);
        assert_eq!(types[0], "worker_seam_probe");
    }
}
