//! External source clients

use anyhow::Result;
use reqwest::Client;
use serde_json::Value;
use secrecy::ExposeSecret;
use std::sync::Arc;

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
            token: config.ha_token.expose_secret().into(),
        })
    }

    pub async fn get_state(&self, entity_id: &str) -> Result<Value> {
        let resp = self.client
            .get(format!("{}/api/states/{}", self.base_url, entity_id))
            .bearer_auth(&self.token)
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    pub async fn call_service(&self, domain: &str, service: &str, data: Value) -> Result<Value> {
        let resp = self.client
            .post(format!("{}/api/services/{}/{}", self.base_url, domain, service))
            .bearer_auth(&self.token)
            .json(&data)
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    pub async fn subscribe_events(&self) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>> {
        let url = format!("{}/api/websocket", self.base_url.replace("http", "ws"));
        let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
        // Auth flow omitted for brevity
        Ok(ws)
    }
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
        let resp = self.client
            .post(format!("{}/mcp/call", self.base_url))
            .json(&serde_json::json!({ "tool": tool, "args": args }))
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }

    pub async fn get_device_info(&self) -> Result<Value> {
        let resp = self.client
            .get(format!("{}/device/info", self.base_url))
            .send()
            .await?
            .json()
            .await?;
        Ok(resp)
    }
}