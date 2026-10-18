//! Shared fixtures for unit and integration tests.
//!
//! Compiled only for tests; keeps the MCP protocol mock and policy wiring
//! in one place instead of per-module copies.

use jarvis_policy::PolicyEngine;
use jarvis_policy::config::PolicyConfig;
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Minimal MCP server speaking newline-delimited JSON-RPC over the given
/// stream. Advertises a single `tool_name` tool. `fail_calls` makes
/// `tools/call` answer with an MCP error result.
pub(crate) async fn run_mock_mcp_server(
    stream: tokio::io::DuplexStream,
    tool_name: &str,
    fail_calls: bool,
) {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let message: Value = serde_json::from_str(&line).expect("valid JSON-RPC line");
        let method = message["method"].as_str().unwrap_or_default();
        let id = message.get("id").cloned();

        let response = match method {
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "serverInfo": { "name": "mock-server", "version": "0.1.0" }
                }
            })),
            "notifications/initialized" => None,
            "tools/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": tool_name,
                        "description": "Echo back the arguments",
                        "inputSchema": { "type": "object" }
                    }]
                }
            })),
            "tools/call" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": "pong" }],
                    "isError": fail_calls
                }
            })),
            _ => None,
        };

        if let Some(response) = response {
            let mut line = serde_json::to_string(&response).expect("serializable response");
            line.push('\n');
            write_half
                .write_all(line.as_bytes())
                .await
                .expect("write response");
            write_half.flush().await.expect("flush response");
        }
    }
}

/// Policy engine over the shipped policy files, with throwaway copies of
/// the Casbin files (the engine persists policy edits to its own copy).
pub(crate) async fn shipped_policy_engine(label: &str) -> PolicyEngine {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/config/policy");
    let dir = std::env::temp_dir().join(format!("jarvis-mcp-gw-policy-{label}"));
    tokio::fs::create_dir_all(&dir)
        .await
        .expect("create temp policy dir");
    let model = dir.join("casbin_model.conf");
    let policy = dir.join("casbin_policy.csv");
    tokio::fs::copy(source.join("casbin_model.conf"), &model)
        .await
        .expect("copy casbin model");
    tokio::fs::copy(source.join("casbin_policy.csv"), &policy)
        .await
        .expect("copy casbin policy");

    let config = PolicyConfig {
        casbin_model_path: model.to_string_lossy().into_owned(),
        casbin_policy_path: policy.to_string_lossy().into_owned(),
        cedar_policy_dir: dir.join("cedar").to_string_lossy().into_owned(),
        kill_switch_admin_token: "test-admin-token".into(),
        ..Default::default()
    };
    PolicyEngine::new(config)
        .await
        .expect("policy engine starts")
}
