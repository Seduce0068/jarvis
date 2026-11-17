//! External source clients

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use jarvis_core::{EventSource, UnifiedEvent};
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(60);

type HomeAssistantSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct HomeAssistantClient {
    client: Client,
    base_url: String,
    token: String,
}

impl HomeAssistantClient {
    pub async fn new(config: &crate::config::EventConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;

        Ok(Self {
            client,
            base_url: config.ha_url.trim_end_matches('/').into(),
            token: config.ha_token().into(),
        })
    }

    pub async fn get_state(&self, entity_id: &str) -> Result<Value> {
        let resp = self
            .client
            .get(format!("{}/api/states/{}", self.base_url, entity_id))
            .bearer_auth(&self.token)
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    pub async fn call_service(&self, domain: &str, service: &str, data: Value) -> Result<Value> {
        let resp = self
            .client
            .post(format!(
                "{}/api/services/{}/{}",
                self.base_url, domain, service
            ))
            .bearer_auth(&self.token)
            .json(&data)
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    /// Listen to `state_changed` events over the Home Assistant WebSocket API.
    ///
    /// Connects, authenticates with the configured token, subscribes, and
    /// forwards each received event through `event_tx`. Submission is
    /// non-blocking: a full queue logs a warning and drops the event (see
    /// [`crate::normalizer::try_emit_event`]). Transient failures are retried
    /// with exponential backoff; an invalid token stops the listener.
    pub async fn listen_events(&self, event_tx: mpsc::Sender<UnifiedEvent>) -> Result<()> {
        let url = websocket_url(&self.base_url)?;
        let mut backoff = INITIAL_RECONNECT_BACKOFF;
        loop {
            match self.listen_once(&url, &event_tx).await {
                Ok(()) => {
                    tracing::info!("Home Assistant WebSocket closed; reconnecting");
                    backoff = INITIAL_RECONNECT_BACKOFF;
                }
                Err(ListenFailure::Fatal(error)) => return Err(error),
                Err(ListenFailure::Transient { error, subscribed }) => {
                    if subscribed {
                        // The connection had been healthy; retry promptly.
                        backoff = INITIAL_RECONNECT_BACKOFF;
                    }
                    tracing::warn!("Home Assistant WebSocket disconnected: {error:#}");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
        }
    }

    async fn listen_once(
        &self,
        url: &str,
        event_tx: &mpsc::Sender<UnifiedEvent>,
    ) -> Result<(), ListenFailure> {
        let (mut socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|error| ListenFailure::transient(error, false))?;

        // 1. Auth handshake: auth_required -> auth -> auth_ok
        let hello = read_json(&mut socket)
            .await
            .map_err(|error| ListenFailure::transient(error, false))?;
        if hello["type"] != "auth_required" {
            return Err(ListenFailure::transient(
                anyhow::anyhow!("unexpected Home Assistant banner: {hello}"),
                false,
            ));
        }
        send_json(
            &mut socket,
            &json!({ "type": "auth", "access_token": self.token }),
        )
        .await
        .map_err(|error| ListenFailure::transient(error, false))?;

        let auth = read_json(&mut socket)
            .await
            .map_err(|error| ListenFailure::transient(error, false))?;
        match auth["type"].as_str() {
            Some("auth_ok") => {}
            Some("auth_invalid") => {
                return Err(ListenFailure::Fatal(anyhow::anyhow!(
                    "Home Assistant rejected the access token: {}",
                    auth["message"].as_str().unwrap_or("no details")
                )));
            }
            other => {
                return Err(ListenFailure::transient(
                    anyhow::anyhow!("unexpected auth response: {other:?}"),
                    false,
                ));
            }
        }

        // 2. Subscribe to state_changed events.
        send_json(
            &mut socket,
            &json!({ "id": 1, "type": "subscribe_events", "event_type": "state_changed" }),
        )
        .await
        .map_err(|error| ListenFailure::transient(error, false))?;
        let subscription = read_json(&mut socket)
            .await
            .map_err(|error| ListenFailure::transient(error, false))?;
        if subscription["type"] != "result" || subscription["success"] != true {
            return Err(ListenFailure::transient(
                anyhow::anyhow!("Home Assistant rejected the subscription: {subscription}"),
                false,
            ));
        }
        tracing::info!("Subscribed to Home Assistant state_changed events");

        // 3. Forward events until the connection drops.
        loop {
            let message =
                read_json(&mut socket)
                    .await
                    .map_err(|error| ListenFailure::Transient {
                        error,
                        subscribed: true,
                    })?;
            if let Some(event) = parse_event_message(&message) {
                crate::normalizer::try_emit_event(event_tx, event);
            }
        }
    }
}

enum ListenFailure {
    /// Unrecoverable failure (e.g. invalid access token); stop listening.
    Fatal(anyhow::Error),
    /// Transient failure; `subscribed` reports whether the connection had
    /// completed the subscription before failing.
    Transient {
        error: anyhow::Error,
        subscribed: bool,
    },
}

impl ListenFailure {
    fn transient(error: impl Into<anyhow::Error>, subscribed: bool) -> Self {
        Self::Transient {
            error: error.into(),
            subscribed,
        }
    }
}

/// Derive the Home Assistant WebSocket API URL from the configured base URL.
pub fn websocket_url(base_url: &str) -> Result<String> {
    let base = base_url.trim_end_matches('/');
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base.starts_with("ws://") || base.starts_with("wss://") {
        base.to_string()
    } else {
        bail!("unsupported Home Assistant URL scheme: {base_url}");
    };
    Ok(format!("{ws_base}/api/websocket"))
}

/// Convert a Home Assistant WebSocket `event` message into a [`UnifiedEvent`].
///
/// Returns `None` for messages that are not events or carry no `event_type`.
pub fn parse_event_message(message: &Value) -> Option<UnifiedEvent> {
    if message["type"] != "event" {
        return None;
    }
    let ha_event = &message["event"];
    let event_type = ha_event["event_type"].as_str()?;
    let timestamp = ha_event["time_fired"]
        .as_str()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);
    let correlation_id = ha_event["context"]["id"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    Some(UnifiedEvent {
        id: uuid::Uuid::new_v4(),
        source: EventSource::HomeAssistant,
        event_type: event_type.to_string(),
        payload: ha_event.clone(),
        timestamp,
        correlation_id,
    })
}

/// Read the next text message as JSON, skipping non-text frames.
///
/// Pong replies to ping frames are queued automatically by tungstenite and
/// must not be sent manually.
async fn read_json(socket: &mut HomeAssistantSocket) -> Result<Value> {
    loop {
        let Some(message) = socket.next().await else {
            bail!("Home Assistant closed the WebSocket");
        };
        match message? {
            Message::Text(text) => {
                return serde_json::from_str(&text)
                    .with_context(|| format!("decoding Home Assistant message: {text}"));
            }
            _ => continue,
        }
    }
}

async fn send_json(socket: &mut HomeAssistantSocket, value: &Value) -> Result<()> {
    socket.send(Message::Text(value.to_string())).await?;
    Ok(())
}

pub struct AndroidClient {
    client: Client,
    base_url: String,
}

impl AndroidClient {
    pub async fn new(config: &crate::config::EventConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            base_url: config.android_mcp_url.trim_end_matches('/').into(),
        })
    }

    pub async fn execute_tool(&self, tool: &str, args: Value) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}/mcp/call", self.base_url))
            .json(&serde_json::json!({ "tool": tool, "args": args }))
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    pub async fn get_device_info(&self) -> Result<Value> {
        let resp = self
            .client
            .get(format!("{}/device/info", self.base_url))
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_websocket_url_from_http_base() {
        assert_eq!(
            websocket_url("http://homeassistant.local:8123").unwrap(),
            "ws://homeassistant.local:8123/api/websocket"
        );
    }

    #[test]
    fn derives_wss_url_from_https_base_and_trims_trailing_slash() {
        assert_eq!(
            websocket_url("https://ha.example.com/").unwrap(),
            "wss://ha.example.com/api/websocket"
        );
    }

    #[test]
    fn keeps_explicit_websocket_scheme() {
        assert_eq!(
            websocket_url("ws://127.0.0.1:8123").unwrap(),
            "ws://127.0.0.1:8123/api/websocket"
        );
        assert_eq!(
            websocket_url("wss://ha.example.com").unwrap(),
            "wss://ha.example.com/api/websocket"
        );
    }

    #[test]
    fn rejects_unknown_scheme() {
        assert!(websocket_url("ftp://homeassistant.local").is_err());
    }

    fn state_changed_message() -> Value {
        json!({
            "id": 1,
            "type": "event",
            "event": {
                "event_type": "state_changed",
                "data": {
                    "entity_id": "light.kitchen",
                    "old_state": {"state": "off"},
                    "new_state": {"state": "on"}
                },
                "origin": "LOCAL",
                "time_fired": "2026-10-15T12:00:00.000000+00:00",
                "context": {"id": "01JTESTCONTEXT", "parent_id": null, "user_id": null}
            }
        })
    }

    #[test]
    fn parses_state_changed_event() {
        let event = parse_event_message(&state_changed_message()).expect("event parsed");
        assert_eq!(event.source, EventSource::HomeAssistant);
        assert_eq!(event.event_type, "state_changed");
        assert_eq!(event.payload["data"]["entity_id"], "light.kitchen");
        assert_eq!(event.correlation_id, "01JTESTCONTEXT");
        assert_eq!(
            event.timestamp,
            chrono::DateTime::parse_from_rfc3339("2026-10-15T12:00:00+00:00")
                .unwrap()
                .with_timezone(&chrono::Utc)
        );
    }

    #[test]
    fn ignores_non_event_messages() {
        assert!(
            parse_event_message(&json!({"id": 1, "type": "result", "success": true})).is_none()
        );
        assert!(parse_event_message(&json!({"type": "event", "event": {}})).is_none());
    }

    #[test]
    fn missing_context_gets_fresh_correlation_id() {
        let mut message = state_changed_message();
        message["event"]["context"] = json!({});
        let event = parse_event_message(&message).expect("event parsed");
        assert!(!event.correlation_id.is_empty());
        assert_ne!(event.correlation_id, "01JTESTCONTEXT");
    }
}
