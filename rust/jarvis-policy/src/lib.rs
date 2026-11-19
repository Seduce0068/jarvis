//! JARVIS Policy Engine
//! Cedar/Casbin-based authorization with approval gates and kill switch.

pub mod approval;
pub mod client;
pub mod config;
pub mod error;
pub mod grpc;
pub mod http;
pub mod kill_switch;
pub mod persistence;
pub mod policy;

pub use client::PolicyClient;
pub use persistence::PgApprovalPersistence;

use anyhow::Result;
use std::sync::Arc;

#[derive(Clone)]
#[allow(dead_code)]
pub struct PolicyEngine {
    config: config::PolicyConfig,
    cedar_engine: Arc<policy::CedarEngine>,
    casbin_enforcer: Arc<policy::CasbinEnforcer>,
    kill_switch: Arc<kill_switch::KillSwitch>,
    approvals: approval::ApprovalStore,
}

impl PolicyEngine {
    pub async fn new(config: config::PolicyConfig) -> Result<Self> {
        // P0-10 staged rollout: the kill-switch admin token must not be the
        // placeholder in production. Default stays warn-only to avoid an
        // unplanned outage; operators can opt into a hard startup failure by
        // setting `require_kill_switch_admin_token = true` once the deployed
        // unit injects a real token via Secret Manager.
        if config.kill_switch_admin_token() == config::PLACEHOLDER_KILL_SWITCH_TOKEN {
            if config.require_kill_switch_admin_token {
                anyhow::bail!(
                    "kill switch admin token is still '{}' but \
                     require_kill_switch_admin_token = true; set \
                     JARVIS_POLICY_KILL_SWITCH_ADMIN_TOKEN",
                    config::PLACEHOLDER_KILL_SWITCH_TOKEN
                );
            }
            tracing::warn!(
                "Kill switch admin token is still the default '{}'; \
                 set JARVIS_POLICY_KILL_SWITCH_ADMIN_TOKEN",
                config::PLACEHOLDER_KILL_SWITCH_TOKEN
            );
        }
        let cedar_engine = Arc::new(policy::CedarEngine::new(&config).await?);
        let casbin_enforcer = Arc::new(
            policy::CasbinEnforcer::new(
                config.casbin_model_path.clone(),
                config.casbin_policy_path.clone(),
            )
            .await?,
        );
        let kill_switch = Arc::new(kill_switch::KillSwitch::new(
            &config.kill_switch_admin_token,
            config
                .kill_switch_state_path
                .as_ref()
                .map(std::path::PathBuf::from),
        ));
        // Fail closed for programmatically built configs too when durable
        // approval persistence is required but no URL was supplied. The
        // file-based startup path already calls this in `PolicyConfig::load`.
        config.validate_approval_persistence()?;

        // Policy evaluation is always deny-by-default (Cedar/Casbin or the
        // kill switch). Reject an explicit opt-out so operators cannot be
        // misled into believing `default_deny = false` weakens the check.
        if !config.default_deny {
            tracing::warn!(
                "default_deny = false is ignored: authorization is always deny-by-default"
            );
        }

        // I30-A: inject the PostgreSQL backend only when a non-blank `pg_url`
        // is configured. With no URL the store keeps its Noop/in-memory default,
        // so existing (and all test) deployments are behaviorally unchanged.
        // The URL itself is never logged: it may carry credentials.
        let approval_timeout = std::time::Duration::from_secs(config.approval_timeout_secs);
        // R5-D: the commit side re-reads the authoritative kill-switch flag at
        // both decision commit boundaries. The gate captures an Arc to the
        // very same KillSwitch the transports arm/disarm, so no mirror can
        // drift and no activation can slip between the transport pre-check
        // and the durable commit unnoticed.
        let kill_switch_for_gate = Arc::clone(&kill_switch);
        let commit_gate: Arc<dyn Fn() -> bool + Send + Sync> =
            Arc::new(move || kill_switch_for_gate.is_activated());
        let approvals = match config
            .pg_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
        {
            Some(pg_url) => {
                tracing::info!("approval persistence enabled: PostgreSQL");
                let persistence = Arc::new(PgApprovalPersistence::connect(pg_url).await?);
                approval::ApprovalStore::with_commit_gate(
                    approval_timeout,
                    config.approval_max_pending,
                    persistence,
                    Some(Arc::clone(&commit_gate)),
                )
            }
            None => approval::ApprovalStore::with_commit_gate(
                approval_timeout,
                config.approval_max_pending,
                Arc::new(approval::NoopPersistence),
                Some(commit_gate),
            ),
        };

        // I31-A: restore durable pending holds before the engine is published.
        // Every undecided row is loaded; live ones become "no waiter" entries
        // visible to `pending_approvals()` / `/metrics`, while already-expired
        // ones are settled through `mark_expired` (Expired + audit) so they do
        // not survive a restart as stale UI holds. Without `pg_url` the store
        // uses `NoopPersistence`, making this a genuine no-op.
        approvals.reconcile_pending().await.map_err(|error| {
            anyhow::anyhow!("failed to reconcile persisted approvals at startup: {error}")
        })?;

        Ok(Self {
            config,
            cedar_engine,
            casbin_enforcer,
            kill_switch,
            approvals,
        })
    }

    /// Optional token required by the HTTP approval endpoints. `None` keeps
    /// the legacy unauthenticated (loopback-only) behavior.
    pub fn approval_api_token(&self) -> Option<&str> {
        self.config.approval_api_token.as_deref()
    }

    /// Opt-in P0-1 guard: when no approval token is configured, the HTTP
    /// approval endpoints fail closed with 503 instead of staying open for
    /// loopback-only deployments.
    pub fn require_approval_api_token(&self) -> bool {
        self.config.require_approval_api_token
    }

    /// Check if action is allowed (async, with Casbin fast path)
    pub async fn is_allowed(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &serde_json::Value,
    ) -> Result<bool> {
        if self.kill_switch.is_activated() {
            tracing::warn!("Kill switch activated - denying all actions");
            return Ok(false);
        }

        // Fast path: Casbin for simple RBAC
        if self
            .casbin_enforcer
            .enforce(principal, action, resource)
            .await?
        {
            return Ok(true);
        }

        // Deep path: Cedar for ABAC with context
        self.cedar_engine
            .is_authorized(principal, action, resource, context)
    }

    /// Request approval for sensitive action (async, may wait for user)
    pub async fn request_approval(
        &self,
        request: policy::ApprovalRequest,
    ) -> Result<policy::ApprovalDecision> {
        if self.kill_switch.is_activated() {
            return Ok(policy::ApprovalDecision::Denied {
                reason: "Kill switch activated".into(),
            });
        }
        Ok(self.approvals.request(request).await)
    }

    /// Approve a pending request (human-in-the-loop decision).
    pub async fn approve(
        &self,
        id: uuid::Uuid,
        approved_by: &str,
    ) -> std::result::Result<policy::ApprovalDecision, approval::ApprovalError> {
        // P0-4: the emergency stop must also block human decisions, not just
        // tool execution. Otherwise an operator could approve work that is
        // immediately denied, while the UI reports a successful decision.
        // R5-D: this is only the fast-path check; `ApprovalStore::decide`
        // re-reads the same authoritative flag at its commit boundaries so an
        // activation landing after this check cannot commit the decision.
        if self.kill_switch.is_activated() {
            return Err(approval::ApprovalError::KillSwitchActivated);
        }
        let decision = policy::ApprovalDecision::Approved {
            approved_by: approved_by.into(),
            approved_at: chrono::Utc::now(),
        };
        self.approvals.decide(id, decision.clone()).await?;
        Ok(decision)
    }

    /// Deny a pending request.
    pub async fn deny(
        &self,
        id: uuid::Uuid,
        reason: &str,
    ) -> std::result::Result<policy::ApprovalDecision, approval::ApprovalError> {
        // R5-D: fast-path check; the authoritative re-check lives in
        // `ApprovalStore::decide` so a stop landing here still blocks the commit.
        if self.kill_switch.is_activated() {
            return Err(approval::ApprovalError::KillSwitchActivated);
        }
        let decision = policy::ApprovalDecision::Denied {
            reason: reason.into(),
        };
        self.approvals.decide(id, decision.clone()).await?;
        Ok(decision)
    }

    /// Current status of an approval request (`Pending` while undecided).
    pub async fn approval_status(
        &self,
        id: uuid::Uuid,
    ) -> std::result::Result<policy::ApprovalDecision, approval::ApprovalError> {
        self.approvals.status(id).await
    }

    /// All requests still awaiting a decision.
    pub async fn pending_approvals(&self) -> Vec<policy::ApprovalRequest> {
        self.approvals.list_pending().await
    }

    /// Activate the kill switch - denies all subsequent actions.
    ///
    /// Requires the admin token; an unauthenticated caller cannot stop the
    /// system.
    pub fn activate_kill_switch(&self, reason: String, admin_token: &str) -> Result<()> {
        self.kill_switch.activate_with_token(reason, admin_token)
    }

    /// Deactivate kill switch (requires admin)
    pub fn deactivate_kill_switch(&self, admin_token: &str) -> Result<()> {
        self.kill_switch.deactivate(admin_token)
    }

    pub fn kill_switch_status(&self) -> kill_switch::KillSwitchStatus {
        self.kill_switch.status()
    }

    pub async fn serve_grpc(&self, addr: std::net::SocketAddr) -> Result<()> {
        grpc::serve(Arc::new(self.clone()), addr).await
    }

    pub async fn serve_http(&self, addr: std::net::SocketAddr) -> Result<()> {
        http::serve(Arc::new(self.clone()), addr).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-policy-lib-{label}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn shipped_casbin(label: &str) -> (String, String) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/config/policy");
        let dir = temp_dir(label);
        let model = dir.join("casbin_model.conf");
        let policy = dir.join("casbin_policy.csv");
        std::fs::copy(source.join("casbin_model.conf"), &model).expect("copy casbin model");
        std::fs::copy(source.join("casbin_policy.csv"), &policy).expect("copy casbin policy");
        (
            model.to_string_lossy().into_owned(),
            policy.to_string_lossy().into_owned(),
        )
    }

    async fn test_engine(label: &str) -> PolicyEngine {
        let (casbin_model_path, casbin_policy_path) = shipped_casbin(label);
        let config = config::PolicyConfig {
            cedar_policy_dir: temp_dir(label).join("cedar").to_string_lossy().into_owned(),
            casbin_model_path,
            casbin_policy_path,
            kill_switch_admin_token: "test-admin-token".into(),
            ..Default::default()
        };
        PolicyEngine::new(config)
            .await
            .expect("policy engine starts")
    }

    fn sample_request() -> policy::ApprovalRequest {
        let now = chrono::Utc::now();
        policy::ApprovalRequest {
            id: uuid::Uuid::new_v4(),
            principal: "jarvis".into(),
            action: "execute".into(),
            resource: "tool:filesystem.write".into(),
            context: serde_json::json!({}),
            requested_at: now,
            expires_at: now + chrono::Duration::seconds(60),
        }
    }

    /// Activating the kill switch must deny every action, including
    /// actions the shipped policy otherwise grants, and must deny
    /// approval requests without queueing them up. Deactivation requires
    /// the admin token.
    #[tokio::test]
    async fn kill_switch_denies_all_actions_until_deactivated() {
        let engine = test_engine("killswitch").await;
        let context = serde_json::json!({});

        // Baseline: the shipped policy grants this tool to the jarvis agent.
        assert!(
            engine
                .is_allowed("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("baseline check runs")
        );

        // Unauthenticated activation must be rejected.
        assert!(
            engine
                .activate_kill_switch("test activation".into(), "wrong-token")
                .is_err()
        );
        assert!(!engine.kill_switch_status().activated);

        engine
            .activate_kill_switch("test activation".into(), "test-admin-token")
            .expect("admin token activates the switch");
        assert!(engine.kill_switch_status().activated);

        assert!(
            !engine
                .is_allowed("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("check while activated runs")
        );

        let decision = engine
            .request_approval(sample_request())
            .await
            .expect("approval request runs");
        assert!(
            matches!(decision, policy::ApprovalDecision::Denied { .. }),
            "approvals are denied while the kill switch is active"
        );

        // Deactivation requires the configured admin token.
        assert!(engine.deactivate_kill_switch("wrong-token").is_err());
        assert!(engine.kill_switch_status().activated);
        engine
            .deactivate_kill_switch("test-admin-token")
            .expect("admin token deactivates the switch");
        assert!(
            engine
                .is_allowed("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("check after deactivation runs")
        );

        std::fs::remove_dir_all(temp_dir("killswitch")).expect("clean temp dir");
    }

    #[tokio::test]
    async fn kill_switch_blocks_approval_decisions() {
        let engine = test_engine("killswitch-approval").await;
        let request = sample_request();
        let id = request.id;
        let engine_for_waiter = engine.clone();
        let waiter = tokio::spawn(async move { engine_for_waiter.request_approval(request).await });
        // Wait until the request is registered as pending.
        loop {
            if matches!(
                engine.approval_status(id).await,
                Ok(policy::ApprovalDecision::Pending)
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        engine
            .activate_kill_switch("test".into(), "test-admin-token")
            .expect("activate");

        assert!(matches!(
            engine.approve(id, "koppe").await,
            Err(crate::approval::ApprovalError::KillSwitchActivated)
        ));
        assert!(matches!(
            engine.deny(id, "no").await,
            Err(crate::approval::ApprovalError::KillSwitchActivated)
        ));

        // The request is still pending and can be decided after deactivation.
        engine
            .deactivate_kill_switch("test-admin-token")
            .expect("deactivate");
        assert!(engine.approve(id, "koppe").await.is_ok());
        let _ = waiter.await.expect("waiter task");
    }

    /// Opt-in strict guard: a placeholder admin token must abort startup
    /// before any policy engine is initialized.
    #[tokio::test]
    async fn strict_token_guard_rejects_placeholder() {
        let label = "strict-token-reject";
        let (casbin_model_path, casbin_policy_path) = shipped_casbin(label);
        let config = config::PolicyConfig {
            cedar_policy_dir: temp_dir(label).join("cedar").to_string_lossy().into_owned(),
            casbin_model_path,
            casbin_policy_path,
            kill_switch_admin_token: config::PLACEHOLDER_KILL_SWITCH_TOKEN.into(),
            require_kill_switch_admin_token: true,
            ..Default::default()
        };

        let err = PolicyEngine::new(config)
            .await
            .err()
            .expect("strict guard rejects the placeholder token");
        let message = err.to_string();
        assert!(
            message.contains(config::PLACEHOLDER_KILL_SWITCH_TOKEN),
            "error names the placeholder"
        );
        assert!(
            message.contains("require_kill_switch_admin_token"),
            "error names the opt-in flag"
        );

        std::fs::remove_dir_all(temp_dir(label)).expect("clean temp dir");
    }

    /// With the guard off (the default), the placeholder token still starts
    /// and only logs a warning, so existing deployments are unaffected.
    #[tokio::test]
    async fn strict_token_guard_off_keeps_warn_only_startup() {
        let label = "strict-token-off";
        let (casbin_model_path, casbin_policy_path) = shipped_casbin(label);
        let config = config::PolicyConfig {
            cedar_policy_dir: temp_dir(label).join("cedar").to_string_lossy().into_owned(),
            casbin_model_path,
            casbin_policy_path,
            kill_switch_admin_token: config::PLACEHOLDER_KILL_SWITCH_TOKEN.into(),
            require_kill_switch_admin_token: false,
            ..Default::default()
        };

        PolicyEngine::new(config)
            .await
            .expect("placeholder token is allowed when the strict guard is off");

        std::fs::remove_dir_all(temp_dir(label)).expect("clean temp dir");
    }

    // ---- R5-D TOCTOU: activation between the pre-check and the commit -------

    /// No-I/O persistence double: records durable commits and can fire a hook
    /// from inside `record_decision` to control the kill-switch race. The
    /// optional third argument of `with_commit_gate` is what the tests use to
    /// flip the authoritative flag at an exact point in the ordering.
    #[derive(Default)]
    struct CommitRecorder {
        decisions: tokio::sync::Mutex<Vec<uuid::Uuid>>,
        on_commit: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    #[async_trait::async_trait]
    impl approval::ApprovalPersistence for CommitRecorder {
        async fn record_request(
            &self,
            _request: &policy::ApprovalRequest,
            _deadline: chrono::DateTime<chrono::Utc>,
        ) -> std::result::Result<(), approval::ApprovalError> {
            Ok(())
        }

        async fn record_decision(
            &self,
            id: uuid::Uuid,
            _decision: &policy::ApprovalDecision,
            _decided_at: chrono::DateTime<chrono::Utc>,
        ) -> std::result::Result<(), approval::ApprovalError> {
            if let Some(hook) = &self.on_commit {
                hook();
            }
            self.decisions.lock().await.push(id);
            Ok(())
        }

        async fn load_pending(
            &self,
        ) -> std::result::Result<Vec<policy::ApprovalRequest>, approval::ApprovalError> {
            Ok(Vec::new())
        }

        async fn mark_expired(
            &self,
            _id: uuid::Uuid,
            _expired_at: chrono::DateTime<chrono::Utc>,
        ) -> std::result::Result<(), approval::ApprovalError> {
            Ok(())
        }
    }

    /// Engine whose commit gate arms the real kill switch on its first read.
    /// That read happens inside `ApprovalStore::decide`, after
    /// `PolicyEngine::approve/deny` already passed their pre-check: precisely
    /// the "check -> activation -> commit" ordering of the confirmed finding.
    async fn engine_with_toctou_gate(label: &str) -> (PolicyEngine, Arc<CommitRecorder>) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let base = test_engine(label).await;
        let switch = Arc::clone(&base.kill_switch);
        let hits = Arc::new(AtomicUsize::new(0));
        let gate: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
            if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                switch.activate("regression: stop after pre-check".into());
            }
            switch.is_activated()
        });
        let recorder = Arc::new(CommitRecorder::default());
        let approvals = approval::ApprovalStore::with_commit_gate(
            std::time::Duration::from_secs(30),
            approval::DEFAULT_MAX_PENDING,
            recorder.clone(),
            Some(gate),
        );
        (PolicyEngine { approvals, ..base }, recorder)
    }

    async fn register_pending(
        engine: &PolicyEngine,
    ) -> (
        uuid::Uuid,
        tokio::task::JoinHandle<policy::ApprovalDecision>,
    ) {
        let request = sample_request();
        let id = request.id;
        let engine_for_waiter = engine.clone();
        let waiter = tokio::spawn(async move {
            engine_for_waiter
                .request_approval(request)
                .await
                .expect("approval request accepted")
        });
        for _ in 0..200 {
            if matches!(
                engine.approval_status(id).await,
                Ok(policy::ApprovalDecision::Pending)
            ) {
                return (id, waiter);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("request never became pending");
    }

    /// Regression: a stop landing after the pre-check but before the commit
    /// must reject `approve` with the fixed-vocabulary error, never call the
    /// persistence backend, and leave the request Pending for a later retry.
    #[tokio::test]
    async fn kill_switch_between_check_and_commit_blocks_approve() {
        let label = "toctou-approve";
        let (engine, recorder) = engine_with_toctou_gate(label).await;
        let (id, waiter) = register_pending(&engine).await;

        assert!(
            !engine.kill_switch_status().activated,
            "precondition: the stop has not landed yet"
        );

        let error = engine
            .approve(id, "koppe")
            .await
            .expect_err("the commit boundary must block the decision");
        assert!(
            matches!(error, approval::ApprovalError::KillSwitchActivated),
            "fixed-vocabulary error expected: {error:?}"
        );
        assert!(
            recorder.decisions.lock().await.is_empty(),
            "record_decision must never be reached after the stop"
        );
        assert!(matches!(
            engine.approval_status(id).await,
            Ok(policy::ApprovalDecision::Pending)
        ));
        assert!(
            !waiter.is_finished(),
            "the waiter must not observe a blocked decision"
        );

        engine
            .deactivate_kill_switch("test-admin-token")
            .expect("deactivate");
        engine
            .approve(id, "koppe")
            .await
            .expect("decision succeeds once the stop is lifted");
        assert!(matches!(
            waiter.await.expect("waiter task"),
            policy::ApprovalDecision::Approved { .. }
        ));

        std::fs::remove_dir_all(temp_dir(label)).expect("clean temp dir");
    }

    /// Same ordering for `deny`: a human denial is also a decision and must be
    /// blocked by the emergency stop at the commit boundary.
    #[tokio::test]
    async fn kill_switch_between_check_and_commit_blocks_deny() {
        let label = "toctou-deny";
        let (engine, recorder) = engine_with_toctou_gate(label).await;
        let (id, waiter) = register_pending(&engine).await;

        let error = engine
            .deny(id, "no")
            .await
            .expect_err("the commit boundary must block the denial");
        assert!(
            matches!(error, approval::ApprovalError::KillSwitchActivated),
            "fixed-vocabulary error expected: {error:?}"
        );
        assert!(
            recorder.decisions.lock().await.is_empty(),
            "record_decision must never be reached after the stop"
        );
        assert!(matches!(
            engine.approval_status(id).await,
            Ok(policy::ApprovalDecision::Pending)
        ));

        engine
            .deactivate_kill_switch("test-admin-token")
            .expect("deactivate");
        engine
            .deny(id, "no")
            .await
            .expect("deny after deactivation");
        assert!(matches!(
            waiter.await.expect("waiter task"),
            policy::ApprovalDecision::Denied { .. }
        ));

        std::fs::remove_dir_all(temp_dir(label)).expect("clean temp dir");
    }

    /// Defense in depth: when the stop lands *while* the durable commit is in
    /// flight, the authority is re-read once more before finalization so the
    /// decision is never published to the waiter or the UI.
    #[tokio::test]
    async fn kill_switch_during_commit_is_not_published() {
        let label = "toctou-during-commit";
        let base = test_engine(label).await;
        let switch = Arc::clone(&base.kill_switch);
        let recorder = Arc::new(CommitRecorder {
            on_commit: Some(Arc::new(move || {
                switch.activate("regression: stop during commit".into());
            })),
            ..Default::default()
        });
        let commit_gate: Arc<dyn Fn() -> bool + Send + Sync> = {
            let switch = Arc::clone(&base.kill_switch);
            Arc::new(move || switch.is_activated())
        };
        let approvals = approval::ApprovalStore::with_commit_gate(
            std::time::Duration::from_secs(30),
            approval::DEFAULT_MAX_PENDING,
            recorder.clone(),
            Some(commit_gate),
        );
        let engine = PolicyEngine { approvals, ..base };
        let (id, waiter) = register_pending(&engine).await;

        let error = engine
            .approve(id, "koppe")
            .await
            .expect_err("publication must be blocked after the stop lands");
        assert!(
            matches!(error, approval::ApprovalError::KillSwitchActivated),
            "fixed-vocabulary error expected: {error:?}"
        );
        assert_eq!(
            recorder.decisions.lock().await.len(),
            1,
            "the backend commit was already in flight when the stop landed"
        );
        assert!(matches!(
            engine.approval_status(id).await,
            Ok(policy::ApprovalDecision::Pending)
        ));
        assert!(
            !waiter.is_finished(),
            "a blocked decision is never published"
        );

        std::fs::remove_dir_all(temp_dir(label)).expect("clean temp dir");
    }
}
