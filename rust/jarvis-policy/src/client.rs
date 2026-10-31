//! gRPC client for the policy engine.
//!
//! Sidecars authorize through this client instead of building their own
//! [`crate::PolicyEngine`], so kill-switch state and policy decisions have a
//! single source of truth. Transport and protocol errors are fail-closed:
//! callers treat an unreachable policy service as a denial.

use std::time::Duration;

use anyhow::{Context, Result};
use tonic::transport::Channel;

use crate::grpc::proto::CheckRequest;
use crate::grpc::proto::policy_service_client::PolicyServiceClient;

/// Deadline for a policy check.
///
/// Local gRPC calls complete in microseconds; the bound only matters when the
/// policy service is wedged. A timed-out check is denied.
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Client for the `jarvis.policy.v1.PolicyService` endpoint.
#[derive(Clone)]
pub struct PolicyClient {
    endpoint: String,
    channel: Channel,
}

impl PolicyClient {
    /// Build a lazily connected client.
    ///
    /// Construction never blocks and never fails on an unreachable endpoint:
    /// the channel connects on the first RPC. This lets sidecars start before
    /// `jarvis-policy` is ready.
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let channel = Channel::from_shared(endpoint.clone())
            .with_context(|| format!("invalid policy endpoint: {endpoint}"))?
            .connect_lazy();
        Ok(Self { endpoint, channel })
    }

    /// Authorization check that fails closed.
    ///
    /// Returns `true` only when the policy engine explicitly allows the
    /// request. An unreachable or failing policy service yields `false` and an
    /// error log.
    pub async fn is_allowed(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &serde_json::Value,
    ) -> bool {
        match self.check(principal, action, resource, context).await {
            Ok(allowed) => allowed,
            Err(error) => {
                tracing::error!(
                    endpoint = %self.endpoint,
                    principal,
                    action,
                    resource,
                    %error,
                    "policy check failed; denying (fail-closed)"
                );
                false
            }
        }
    }

    /// Raw check that surfaces transport and protocol errors to the caller.
    pub async fn check(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &serde_json::Value,
    ) -> Result<bool> {
        let mut client = PolicyServiceClient::new(self.channel.clone());
        let mut request = tonic::Request::new(CheckRequest {
            principal: principal.to_owned(),
            action: action.to_owned(),
            resource: resource.to_owned(),
            context: serde_json::to_string(context)?,
        });
        request.set_timeout(CHECK_TIMEOUT);
        let response = client
            .check(request)
            .await
            .with_context(|| format!("policy check RPC to {} failed", self.endpoint))?
            .into_inner();
        Ok(response.allowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PolicyEngine;
    use crate::config::PolicyConfig;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-policy-client-{label}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Start a real policy server on an ephemeral port and return a client
    /// plus the engine handle so tests can drive state changes.
    async fn start_server(label: &str) -> (PolicyClient, Arc<PolicyEngine>, PathBuf) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/config/policy");
        let dir = temp_dir(label);
        let model = dir.join("casbin_model.conf");
        let policy = dir.join("casbin_policy.csv");
        std::fs::copy(source.join("casbin_model.conf"), &model).expect("copy casbin model");
        std::fs::copy(source.join("casbin_policy.csv"), &policy).expect("copy casbin policy");

        let config = PolicyConfig {
            cedar_policy_dir: dir.join("cedar").to_string_lossy().into_owned(),
            casbin_model_path: model.to_string_lossy().into_owned(),
            casbin_policy_path: policy.to_string_lossy().into_owned(),
            kill_switch_admin_token: "test-admin-token".into(),
            ..Default::default()
        };
        let engine = Arc::new(
            PolicyEngine::new(config)
                .await
                .expect("policy engine starts"),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind policy server");
        let addr = listener.local_addr().expect("policy server address");
        let serving = engine.clone();
        tokio::spawn(async move {
            crate::grpc::serve_with_listener(serving, listener)
                .await
                .expect("policy server runs");
        });

        (
            PolicyClient::new(format!("http://{addr}")).expect("policy client"),
            engine,
            dir,
        )
    }

    #[tokio::test]
    async fn allowed_and_denied_checks_go_through_grpc() {
        let (client, _engine, dir) = start_server("checks").await;
        let context = serde_json::json!({});

        assert!(
            client
                .check("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("check runs"),
            "the shipped policy grants filesystem.read to jarvis"
        );
        assert!(
            !client
                .check("jarvis", "execute", "tool:git.commit", &context)
                .await
                .expect("check runs"),
            "the shipped policy does not grant git.commit to jarvis"
        );

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    /// The kill switch must reach sidecars through the client, not through
    /// process-local state.
    #[tokio::test]
    async fn kill_switch_activation_is_visible_to_remote_clients() {
        let (client, engine, dir) = start_server("killswitch").await;
        let context = serde_json::json!({});

        assert!(
            client
                .check("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("baseline check runs")
        );

        engine
            .activate_kill_switch("test activation".into(), "test-admin-token")
            .expect("admin token activates the switch");

        assert!(
            !client
                .check("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("check while activated runs")
        );

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    #[tokio::test]
    async fn unreachable_policy_fails_closed() {
        // Port 1 is not served; the lazy channel fails on the first RPC.
        let client = PolicyClient::new("http://127.0.0.1:1").expect("policy client");
        assert!(
            !client
                .is_allowed(
                    "jarvis",
                    "execute",
                    "tool:filesystem.read",
                    &serde_json::json!({})
                )
                .await,
            "an unreachable policy service must deny"
        );
    }
}
