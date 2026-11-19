//! Ports (trait seams) for the external services used by event processing.
//!
//! Slice 1 of the `jarvis-event` sidecar refactor. The default adapters below
//! delegate to the existing `jarvis_context` / `jarvis_policy` clients, so
//! runtime behaviour stays unchanged; a later slice can replace an adapter
//! with a sidecar client without touching the normalizer.

use std::sync::Arc;

use async_trait::async_trait;
use jarvis_core::UnifiedEvent;

/// Supplies the PII-filtered context attached to an event.
///
/// `Send + Sync` is required because the normalizer's worker task owns the
/// provider across `.await` points on a multi-threaded runtime.
#[async_trait]
pub trait ContextProvider: Send + Sync {
    /// Build the context passed to the policy gate and the agent runtime.
    async fn build_context(&self, event: &UnifiedEvent) -> anyhow::Result<serde_json::Value>;
}

/// Authorizes an automated response for a normalized event.
///
/// Implementations must fail closed: transport or backend errors deny.
#[async_trait]
pub trait PolicyGate: Send + Sync {
    /// Returns `true` only when the action is explicitly allowed.
    async fn is_allowed(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &serde_json::Value,
    ) -> bool;
}

/// Default [`ContextProvider`]: delegates to the existing context builder.
pub struct DefaultContextProvider {
    inner: Arc<jarvis_context::ContextBuilder>,
}

impl DefaultContextProvider {
    /// Wrap a context builder created by the existing startup path.
    pub fn new(inner: Arc<jarvis_context::ContextBuilder>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ContextProvider for DefaultContextProvider {
    async fn build_context(&self, event: &UnifiedEvent) -> anyhow::Result<serde_json::Value> {
        self.inner.build_context(event).await
    }
}

/// Default [`PolicyGate`]: delegates to the existing fail-closed policy client.
pub struct DefaultPolicyGate {
    inner: Arc<jarvis_policy::PolicyClient>,
}

impl DefaultPolicyGate {
    /// Wrap a policy client created by the existing startup path.
    pub fn new(inner: Arc<jarvis_policy::PolicyClient>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl PolicyGate for DefaultPolicyGate {
    async fn is_allowed(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &serde_json::Value,
    ) -> bool {
        self.inner
            .is_allowed(principal, action, resource, context)
            .await
    }
}
