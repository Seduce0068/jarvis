//! gRPC-backed [`ContextProvider`] for the opt-in `ContextMode::Grpc` route.
//!
//! The default `in_process` mode keeps using `DefaultContextProvider`. This
//! adapter calls the context sidecar's `BuildContext` RPC and carries the
//! event's `source` / `timestamp` in the additive proto fields (I31-B), so the
//! correlation metadata is not collapsed to `Manual` / `Utc::now()`.

use async_trait::async_trait;
use jarvis_context::grpc::proto::BuildContextRequest;
use jarvis_context::grpc::proto::context_service_client::ContextServiceClient;
use jarvis_core::UnifiedEvent;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};

use crate::ports::ContextProvider;

/// Same secret header the context service's gRPC ingress gate reads.
const CONTEXT_TOKEN_HEADER: &str = "x-jarvis-context-token";

/// Encode an event for the context sidecar, including the additive correlation
/// fields (I31-B).
///
/// Pure and network-free: `source` uses the `EventSource` display form (the
/// snake_case string its `FromStr` accepts) and `timestamp` uses Unix seconds,
/// exactly the wire format the context handler decodes. An unserializable
/// payload falls back to an empty string, which stays inside the handler's
/// legacy `unwrap_or_default()` behavior.
fn to_build_context_request(event: &UnifiedEvent) -> BuildContextRequest {
    BuildContextRequest {
        event_id: event.id.to_string(),
        event_type: event.event_type.clone(),
        payload: serde_json::to_string(&event.payload).unwrap_or_default(),
        correlation_id: event.correlation_id.clone(),
        source: event.source.to_string(),
        timestamp: event.timestamp.timestamp(),
    }
}

/// Context provider that delegates to the context gRPC sidecar.
///
/// The channel is built with `connect_lazy()`, so constructing the provider
/// performs no I/O and needs no running sidecar. A connection is only attempted
/// when [`ContextProvider::build_context`] is actually called.
pub struct GrpcContextProvider {
    client: ContextServiceClient<Channel>,
    token: Option<String>,
}

impl GrpcContextProvider {
    /// Build a provider for `addr` (for example `http://127.0.0.1:50054`).
    ///
    /// No network connection is made here; a malformed endpoint URI is the only
    /// failure mode. The token is never logged.
    pub fn new(addr: &str, token: Option<&str>) -> anyhow::Result<Self> {
        let channel = Endpoint::from_shared(addr.to_owned())?.connect_lazy();
        Ok(Self {
            client: ContextServiceClient::new(channel),
            token: token.map(str::to_owned),
        })
    }
}

#[async_trait]
impl ContextProvider for GrpcContextProvider {
    async fn build_context(&self, event: &UnifiedEvent) -> anyhow::Result<serde_json::Value> {
        let mut request = tonic::Request::new(to_build_context_request(event));
        if let Some(token) = self.token.as_deref() {
            let value: MetadataValue<Ascii> = token.parse().map_err(|_| {
                anyhow::anyhow!("context gRPC token is not valid metadata; value never logged")
            })?;
            request.metadata_mut().insert(CONTEXT_TOKEN_HEADER, value);
        }

        let mut client = self.client.clone();
        let response = client
            .build_context(request)
            .await
            .map_err(|status| anyhow::anyhow!("context gRPC BuildContext failed: {status}"))?;
        let context = response.into_inner().context;
        serde_json::from_str(&context)
            .map_err(|error| anyhow::anyhow!("context gRPC returned non-JSON context: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_core::EventSource;

    fn sample_event(source: EventSource, timestamp: i64) -> UnifiedEvent {
        UnifiedEvent {
            id: uuid::Uuid::from_u128(31),
            source,
            event_type: "state_changed".into(),
            payload: serde_json::json!({"entity_id": "light.kitchen"}),
            timestamp: chrono::DateTime::from_timestamp(timestamp, 0).expect("valid timestamp"),
            correlation_id: "corr-31".into(),
        }
    }

    /// I31-B contract test: encode here, decode with the real context handler.
    /// No channel is opened and no server is started, so this needs no network.
    #[test]
    fn source_and_timestamp_survive_grpc_round_trip() {
        let event = sample_event(EventSource::HomeAssistant, 1_700_000_000);
        let wire = to_build_context_request(&event);

        let (source, timestamp) = jarvis_context::grpc::event_metadata_from_request(&wire);

        assert_eq!(source, event.source);
        assert_eq!(timestamp, event.timestamp);
        // The pre-existing fields must still be carried unchanged.
        assert_eq!(wire.event_id, event.id.to_string());
        assert_eq!(wire.event_type, event.event_type);
        assert_eq!(wire.correlation_id, event.correlation_id);
        let expected_payload = serde_json::to_string(&event.payload).expect("payload serializes");
        assert_eq!(wire.payload, expected_payload);
    }

    /// Wire-level check: an actual protobuf encode/decode (still no transport)
    /// keeps both additive fields, so the proto tag additions are effective.
    #[test]
    fn additive_fields_survive_protobuf_encoding() {
        use prost::Message;

        let event = sample_event(EventSource::Calendar, 1_700_000_000);
        let wire = to_build_context_request(&event);
        let bytes = wire.encode_to_vec();
        let decoded = BuildContextRequest::decode(bytes.as_slice()).expect("proto decodes");

        assert_eq!(decoded, wire);
        let (source, timestamp) = jarvis_context::grpc::event_metadata_from_request(&decoded);
        assert_eq!(source, EventSource::Calendar);
        assert_eq!(timestamp, event.timestamp);
    }

    /// Every known source must round-trip (Display -> FromStr) through the
    /// additive `source` field.
    #[test]
    fn every_event_source_survives_the_additive_field() {
        for source in [
            EventSource::HomeAssistant,
            EventSource::Android,
            EventSource::Gmail,
            EventSource::Calendar,
            EventSource::Web,
            EventSource::Finance,
            EventSource::Manual,
        ] {
            let event = sample_event(source, 1);
            let wire = to_build_context_request(&event);
            let (decoded, _) = jarvis_context::grpc::event_metadata_from_request(&wire);
            assert_eq!(decoded, source);
        }
    }

    /// Zero is the handler's "unset" sentinel; every other value, including a
    /// pre-epoch one, is preserved verbatim.
    #[test]
    fn timestamp_encoding_uses_the_unix_seconds_contract() {
        let wire = to_build_context_request(&sample_event(EventSource::Web, -1));
        assert_eq!(wire.timestamp, -1);
        let (_, decoded) = jarvis_context::grpc::event_metadata_from_request(&wire);
        let expected = chrono::DateTime::from_timestamp(-1, 0).expect("valid timestamp");
        assert_eq!(decoded, expected);
    }

    /// Constructing the provider must not connect: an unused port is fine
    /// because the channel is lazy.
    // `connect_lazy` still builds the connector, which needs a Tokio reactor.
    #[tokio::test]
    async fn provider_construction_does_not_open_a_connection() {
        let provider = GrpcContextProvider::new("http://127.0.0.1:1", Some("ctx-token"))
            .expect("lazy endpoint is valid");
        assert_eq!(provider.token.as_deref(), Some("ctx-token"));
    }

    #[test]
    fn invalid_endpoint_uri_is_rejected_without_network() {
        assert!(GrpcContextProvider::new("not a uri", None).is_err());
    }

    #[test]
    fn token_header_literal_matches_the_context_service_gate() {
        assert_eq!(CONTEXT_TOKEN_HEADER, "x-jarvis-context-token");
    }
}
