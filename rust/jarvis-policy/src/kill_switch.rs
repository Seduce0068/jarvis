//! Kill Switch - emergency stop for all automated actions

use anyhow::Result;
use hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// Reason recorded when either metadata lock is poisoned. A panic while the
/// switch was being updated leaves its state untrustworthy, so we arm it.
const POISONED_LOCK_REASON: &str = "kill switch state lock poisoned; failing closed";
/// Reason recorded when the state file cannot be written.
const PERSIST_FAILURE_REASON: &str = "failed to persist kill switch state; failing closed";
/// Reason recorded when the system clock predates UNIX_EPOCH. Reading the
/// clock is fallible (e.g. on time regression); a safety device must fail
/// closed and log the degradation rather than panic (fatal under `panic =
/// "abort"`).
const CLOCK_FAILURE_REASON: &str = "system clock is before UNIX_EPOCH; failing closed";
/// Context attached to errors from `persist()`. Even a caller
/// that only logs `Display` must see an internal storage failure here, never
/// a bad admin token.
const PERSIST_ERROR_CONTEXT: &str =
    "kill switch persistence failed (internal storage error, NOT an authentication failure)";

/// Metadata key used by every transport to carry the dedicated kill-switch
/// admin token on the read-only status surface. Keeping the name in this crate
/// module lets HTTP and gRPC agree without changing the protobuf schema.
pub(crate) const KILL_SWITCH_TOKEN_HEADER: &str = "x-jarvis-kill-switch-token";

/// Closed vocabulary returned in place of the free-form `reason` / `activated_by`
/// strings by the status surfaces. Internal callers still see the raw values
/// (audit log, persistence); only the wire projections use these literals.
pub(crate) mod status_vocabulary {
    pub const REASON_LOCK_POISONED: &str = "lock_poisoned";
    pub const REASON_PERSISTENCE_FAILURE: &str = "persistence_failure";
    pub const REASON_CLOCK_FAILURE: &str = "clock_failure";
    pub const REASON_STATE_UNREADABLE: &str = "state_unreadable";
    pub const REASON_OPERATOR_DEFINED: &str = "operator_defined";
    pub const ACTOR_SYSTEM: &str = "system";
    pub const ACTOR_OPERATOR: &str = "operator";
}

/// Compare two tokens without an early exit on the first mismatch.
///
/// This is the crate-wide helper for every token gate: the HTTP approval token
/// middleware, kill-switch activation/deactivation, and both read-only status
/// surfaces all call this function. The loop always covers the longer input and
/// pads the shorter one, so neither a content mismatch nor a length mismatch
/// short-circuits. No `unsafe` code and no extra dependency are involved.
///
/// Kept `pub(crate)` here (rather than in a new top-level module) because only
/// the four policy transport/state files are in scope for this change; the
/// module placement does not affect visibility across the crate.
pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for (x, y) in a
        .iter()
        .map(Some)
        .chain(std::iter::repeat(None))
        .zip(b.iter().map(Some).chain(std::iter::repeat(None)))
        .take(max)
    {
        diff |= (x.copied().unwrap_or(0) ^ y.copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

/// Error categories for the token-gated kill switch operations.
///
/// [`KillSwitchError::InvalidAdminToken`] is the only authentication failure.
/// [`KillSwitchError::Persistence`] means the token was accepted but the
/// durable state file could not be written: the switch is armed in memory
/// (fail-closed) while the change is not durable. Callers must map that
/// variant to an internal/storage status (gRPC `Internal`, HTTP 5xx), never
/// to `Unauthenticated`/401. Encoding the distinction in the type instead of
/// message text lets the out-of-scope callers fix their mapping without
/// string matching.
#[derive(Debug, thiserror::Error)]
pub enum KillSwitchError {
    /// Wrong admin token: the sole variant that is an auth failure.
    #[error("Invalid admin token")]
    InvalidAdminToken,

    /// Internal persistence/serialization failure. NOT an authentication
    /// failure: the token was valid. The switch stays activated (fail-closed)
    /// but the state file was not written, so a deactivation must not be
    /// reported as successful.
    #[error(
        "internal error (NOT an authentication failure): kill switch state could not be \
         persisted; switch remains activated: {detail}"
    )]
    Persistence { detail: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitchStatus {
    pub activated: bool,
    pub activated_at: Option<u64>,
    pub reason: Option<String>,
    pub activated_by: Option<String>,
}

impl KillSwitchStatus {
    /// Transport-safe projection of the status.
    ///
    /// `reason` and `activated_by` are free-form strings: an operator may put
    /// arbitrary text in a reason, and a persisted state file can contain
    /// attacker-controlled data. Even after the status surface is admin-gated,
    /// returning those strings verbatim would let an authenticated reader (or
    /// a log/forwarder downstream) turn the field into an exfiltration channel.
    /// Collapse them to a fixed vocabulary; the raw values remain available to
    /// internal audit code via [`KillSwitch::status`].
    pub fn redacted(&self) -> Self {
        let reason = self.reason.as_deref().map(|reason| {
            if reason == POISONED_LOCK_REASON {
                status_vocabulary::REASON_LOCK_POISONED
            } else if reason == PERSIST_FAILURE_REASON {
                status_vocabulary::REASON_PERSISTENCE_FAILURE
            } else if reason.starts_with(CLOCK_FAILURE_REASON) {
                status_vocabulary::REASON_CLOCK_FAILURE
            } else if reason.starts_with("unreadable kill switch state") {
                status_vocabulary::REASON_STATE_UNREADABLE
            } else {
                status_vocabulary::REASON_OPERATOR_DEFINED
            }
        });
        let activated_by = self.activated_by.as_deref().map(|actor| {
            if actor == status_vocabulary::ACTOR_SYSTEM {
                status_vocabulary::ACTOR_SYSTEM
            } else {
                status_vocabulary::ACTOR_OPERATOR
            }
        });
        Self {
            activated: self.activated,
            activated_at: self.activated_at,
            reason: reason.map(str::to_owned),
            activated_by: activated_by.map(str::to_owned),
        }
    }
}

/// JSON shape written to the state file.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedState {
    activated: bool,
    activated_at: u64,
    reason: Option<String>,
    activated_by: Option<String>,
}

pub struct KillSwitch {
    activated: AtomicBool,
    activated_at: AtomicU64,
    reason: RwLock<Option<String>>,
    activated_by: RwLock<Option<String>>,
    admin_token_hash: String,
    /// Optional path used to survive process restarts. Without it the switch
    /// remains in-memory (tests and ephemeral runs).
    state_path: Option<PathBuf>,
}

impl KillSwitch {
    pub fn new(admin_token: &str, state_path: Option<PathBuf>) -> Self {
        let switch = Self {
            activated: AtomicBool::new(false),
            activated_at: AtomicU64::new(0),
            reason: RwLock::new(None),
            activated_by: RwLock::new(None),
            admin_token_hash: Self::hash_token(admin_token),
            state_path,
        };
        switch.restore();
        switch
    }

    /// Hash the presented token and compare it with the stored digest using
    /// the crate-wide constant-time helper. A plain `!=` here leaked the match
    /// position through timing and diverged from the other token gates.
    fn hash_matches(&self, token: &str) -> bool {
        constant_time_eq(&Self::hash_token(token), &self.admin_token_hash)
    }

    /// Verify the dedicated admin token for the read-only status surfaces.
    ///
    /// Both transports call this instead of reading the raw configured token,
    /// so the status gate and the activate/deactivate gates share one
    /// comparison implementation.
    pub fn verify_admin_token(&self, token: &str) -> bool {
        self.hash_matches(token)
    }

    /// Activate with the historical `"system"` actor. Kept as a compatibility
    /// wrapper; callers that know the requesting principal should use
    /// [`Self::activate_with_actor`].
    pub fn activate(&self, reason: String) {
        self.activate_with_actor(reason, "system".into());
    }

    /// Activate the switch and record `actor` in the audit slot.
    ///
    /// `lib.rs` / `http.rs` are outside this file's ownership, so this is
    /// added now as the future connection point: once those callers are
    /// updated they can pass the authenticated principal here. Until then
    /// every existing path goes through [`Self::activate`] and still records
    /// `"system"`.
    pub fn activate_with_actor(&self, reason: String, actor: String) {
        self.arm_with_actor(reason.clone(), actor);
        if let Err(error) = self.persist() {
            // `persist()` re-arms the switch fail-closed, so the in-memory
            // state stays activated. The state file was NOT written though:
            // never log a durable "KILL SWITCH ACTIVATED" that contradicts
            // the post-restart reality (the switch would be disarmed again).
            warn!(
                "KILL SWITCH ACTIVATED IN MEMORY ONLY (NOT PERSISTED): requested reason '{}'; \
                 state will not survive restart: {error}",
                reason
            );
        } else {
            warn!("KILL SWITCH ACTIVATED: {}", reason);
        }
    }

    /// Activate after verifying the admin token.
    ///
    /// The unauthenticated [`Self::activate`] remains for internal callers
    /// (restore, tests); every external surface must use this method so a
    /// local process cannot stop the system unilaterally.
    pub fn activate_with_token(&self, reason: String, admin_token: &str) -> Result<()> {
        if !self.hash_matches(admin_token) {
            return Err(KillSwitchError::InvalidAdminToken.into());
        }
        self.activate(reason);
        Ok(())
    }

    /// Write a metadata slot even when a previous writer panicked while
    /// holding the lock. The guarded value is untrustworthy, but for a safety
    /// device completing the write is safer than panicking and leaving stale
    /// metadata behind.
    fn write_slot(slot: &RwLock<Option<String>>, value: Option<String>) {
        let mut guard = match slot.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("kill switch metadata lock is poisoned; forcing write");
                poisoned.into_inner()
            }
        };
        *guard = value;
    }

    /// Read a metadata slot, recovering the guarded value when the lock is
    /// poisoned. The flag tells callers that the state must be fail-closed.
    fn read_slot(slot: &RwLock<Option<String>>) -> (Option<String>, bool) {
        let mut poisoned = false;
        let value = slot
            .read()
            .unwrap_or_else(|error| {
                poisoned = true;
                error.into_inner()
            })
            .clone();
        (value, poisoned)
    }

    /// Read both metadata slots, arming the switch if either lock was
    /// poisoned so callers never observe a potentially disarmed state.
    fn read_state_slots(&self) -> (Option<String>, Option<String>) {
        let (_, reason_poisoned) = Self::read_slot(&self.reason);
        let (_, actor_poisoned) = Self::read_slot(&self.activated_by);
        if reason_poisoned || actor_poisoned {
            self.arm(POISONED_LOCK_REASON.into());
        }
        (
            Self::read_slot(&self.reason).0,
            Self::read_slot(&self.activated_by).0,
        )
    }

    /// Set the in-memory state without persisting, attributing the activation
    /// to the system. Used by [`Self::restore`] so a corrupt state file is not
    /// overwritten during recovery, and by the fail-closed paths in
    /// [`Self::persist`] / poisoned-lock recovery.
    fn arm(&self, reason: String) {
        self.arm_with_actor(reason, "system".into());
    }

    /// Set the in-memory state without persisting, recording `actor` in the
    /// audit slot. `arm()` delegates here with `"system"` so the existing
    /// fail-closed behavior stays unchanged; `arm_with_actor` never calls
    /// `persist()`.
    fn arm_with_actor(&self, reason: String, actor: String) {
        // `duration_since` returns Err when the clock is before UNIX_EPOCH
        // (e.g. on time regression). Never unwrap here: this is the emergency
        // stop path and a panic is fatal under `panic = "abort"`. Fail closed
        // instead: keep the switch armed, use a sentinel timestamp and record
        // the degradation in the reason slot alongside the original reason.
        let (now, reason) = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => (duration.as_secs(), reason),
            Err(error) => {
                warn!("kill switch clock is before UNIX_EPOCH: {error}; failing closed");
                (0, format!("{CLOCK_FAILURE_REASON}: {reason}"))
            }
        };
        self.activated.store(true, Ordering::SeqCst);
        self.activated_at.store(now, Ordering::SeqCst);
        Self::write_slot(&self.reason, Some(reason));
        Self::write_slot(&self.activated_by, Some(actor));
    }

    pub fn deactivate(&self, admin_token: &str) -> Result<()> {
        if !self.hash_matches(admin_token) {
            return Err(KillSwitchError::InvalidAdminToken.into());
        }
        self.activated.store(false, Ordering::SeqCst);
        self.activated_at.store(0, Ordering::SeqCst);
        Self::write_slot(&self.reason, None);
        Self::write_slot(&self.activated_by, None);
        let persist_result = self.persist();
        // A persist failure fails closed by re-arming the switch. Never tell
        // the operator the deactivation succeeded when it did not stick, and
        // never let this be mistaken for a bad token: the token was valid,
        // the durable write was not.
        if persist_result.is_err() || self.is_activated() {
            let detail = match persist_result {
                Err(error) => format!("{error:#}"),
                Ok(()) => "switch re-armed by fail-closed recovery".into(),
            };
            return Err(KillSwitchError::Persistence { detail }.into());
        }
        warn!("Kill switch deactivated by admin");
        Ok(())
    }

    /// Restore state from disk. A missing file is a normal first start. An
    /// unreadable file is treated as activated: a corrupt safety device must
    /// not silently disarm itself.
    fn restore(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            // A missing file is the normal first start.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.arm(format!("unreadable kill switch state: {error}"));
                warn!(
                    "Kill switch state at {} cannot be read; treating as activated: {error}",
                    path.display()
                );
                return;
            }
        };
        match serde_json::from_str::<PersistedState>(&contents) {
            Ok(state) if state.activated => {
                self.activated.store(true, Ordering::SeqCst);
                self.activated_at
                    .store(state.activated_at, Ordering::SeqCst);
                Self::write_slot(&self.reason, state.reason);
                Self::write_slot(&self.activated_by, state.activated_by);
                warn!("Kill switch restored as ACTIVE from {}", path.display());
            }
            Ok(_) => {}
            Err(error) => {
                self.arm(format!("unreadable kill switch state: {error}"));
                warn!(
                    "Kill switch state at {} is unreadable; treating as activated: {error}",
                    path.display()
                );
            }
        }
    }

    /// Persist the current state when a path is configured.
    ///
    /// Returns `Ok(())` when the write succeeded, or when no path is
    /// configured (in-memory operation only). Returns `Err` when a configured
    /// path could not be written; every failure path re-arms the switch
    /// fail-closed via [`Self::arm`] with [`PERSIST_FAILURE_REASON`]. Callers
    /// must therefore treat `Err` as "activated but degraded (not durable)".
    fn persist(&self) -> Result<()> {
        let Some(path) = &self.state_path else {
            return Ok(());
        };
        let (reason, activated_by) = self.read_state_slots();
        let state = PersistedState {
            activated: self.activated.load(Ordering::SeqCst),
            activated_at: self.activated_at.load(Ordering::SeqCst),
            reason,
            activated_by,
        };
        let bytes = match serde_json::to_vec_pretty(&state) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!("Failed to serialize kill switch state: {error}");
                self.arm(PERSIST_FAILURE_REASON.into());
                let error = anyhow::anyhow!("failed to serialize kill switch state: {error}");
                return Err(error.context(PERSIST_ERROR_CONTEXT));
            }
        };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            warn!("Failed to create {}: {error}", parent.display());
            self.arm(PERSIST_FAILURE_REASON.into());
            let error = anyhow::anyhow!("failed to create {}: {error}", parent.display());
            return Err(error.context(PERSIST_ERROR_CONTEXT));
        }
        if let Err(error) = std::fs::write(path, bytes) {
            warn!(
                "Failed to persist kill switch state to {}: {error}",
                path.display()
            );
            // `arm` never calls `persist`, so this cannot recurse.
            self.arm(PERSIST_FAILURE_REASON.into());
            let error = anyhow::anyhow!(
                "failed to persist kill switch state to {}: {error}",
                path.display()
            );
            return Err(error.context(PERSIST_ERROR_CONTEXT));
        }
        Ok(())
    }

    pub fn is_activated(&self) -> bool {
        self.activated.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> KillSwitchStatus {
        let (reason, activated_by) = self.read_state_slots();
        let activated = self.activated.load(Ordering::SeqCst);
        KillSwitchStatus {
            activated,
            activated_at: if activated {
                Some(self.activated_at.load(Ordering::SeqCst))
            } else {
                None
            },
            reason,
            activated_by,
        }
    }

    fn hash_token(token: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        hex::encode(hasher.finalize())
    }
}

impl crate::PolicyEngine {
    /// Gate the read-only kill-switch status surfaces with the same dedicated
    /// admin token that guards activation/deactivation.
    ///
    /// Defined here (instead of `lib.rs`) because only this file and the two
    /// transport files are in scope for this change; the method is additive and
    /// keeps the raw configured token out of both transport modules.
    pub(crate) fn verify_kill_switch_admin_token(&self, token: &str) -> bool {
        self.kill_switch.verify_admin_token(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-kill-switch-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("kill-switch.json")
    }

    #[test]
    fn persists_activation_across_restarts() {
        let path = temp_path("persist");
        let switch = KillSwitch::new("admin-token", Some(path.clone()));
        assert!(!switch.is_activated());

        switch.activate("maintenance".into());

        let restored = KillSwitch::new("admin-token", Some(path.clone()));
        assert!(restored.is_activated());
        assert_eq!(restored.status().reason.as_deref(), Some("maintenance"));

        restored
            .deactivate("admin-token")
            .expect("admin token deactivates");

        let after = KillSwitch::new("admin-token", Some(path.clone()));
        assert!(!after.is_activated());

        std::fs::remove_dir_all(path.parent().unwrap()).expect("clean temp dir");
    }

    #[test]
    fn corrupt_state_file_fails_closed() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{not json").expect("write corrupt state");

        let switch = KillSwitch::new("admin-token", Some(path.clone()));
        assert!(switch.is_activated());
        assert!(
            switch
                .status()
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("unreadable"))
        );

        std::fs::remove_dir_all(path.parent().unwrap()).expect("clean temp dir");
    }

    #[test]
    fn without_a_state_path_state_stays_in_memory() {
        let switch = KillSwitch::new("admin-token", None);
        switch.activate("ephemeral".into());
        assert!(switch.is_activated());
    }

    /// A panic while holding either metadata lock must not turn into another
    /// panic (which could take down the whole policy service); the switch
    /// must instead recover the value and arm itself.
    #[test]
    fn poisoned_state_locks_do_not_panic() {
        let switch = KillSwitch::new("admin-token", None);

        std::thread::scope(|scope| {
            for slot in [&switch.reason, &switch.activated_by] {
                let handle = scope.spawn(move || {
                    let _guard = slot.write().expect("hold the write lock");
                    panic!("deliberately poison the lock");
                });
                assert!(handle.join().is_err(), "poisoning thread panicked");
            }
        });

        let status = switch.status();
        assert!(status.activated);
        assert_eq!(status.reason.as_deref(), Some(POISONED_LOCK_REASON));
        assert_eq!(status.activated_by.as_deref(), Some("system"));

        // Writes on poisoned locks use into_inner() instead of panicking.
        switch.activate("after poison".into());
        assert!(switch.is_activated());
        assert!(switch.deactivate("admin-token").is_ok());
        assert!(!switch.is_activated());
    }

    /// If the state file cannot be written, the switch must fail closed
    /// instead of only warning and leaving itself disarmed.
    #[test]
    fn persist_failure_fails_closed() {
        let path = temp_path("persist-fail");
        let switch = KillSwitch::new("admin-token", Some(path.clone()));
        assert!(!switch.is_activated());

        // Replace the parent directory with a regular file: create_dir_all
        // now fails, so persist() cannot write the state file.
        let parent = path.parent().expect("state path has a parent");
        std::fs::remove_dir_all(parent).expect("remove temp dir");
        std::fs::write(parent, b"blocker").expect("create blocker file");

        switch.activate("maintenance".into());

        assert!(switch.is_activated());
        let status = switch.status();
        assert_eq!(status.reason.as_deref(), Some(PERSIST_FAILURE_REASON));

        // Deactivation cannot be reported as successful when persistence
        // fails and the switch re-arms itself.
        let error = switch
            .deactivate("admin-token")
            .expect_err("failed persist must surface as an error");
        assert!(error.to_string().contains("remains activated"));
        assert!(switch.is_activated());
        assert_eq!(status.activated_by.as_deref(), Some("system"));

        std::fs::remove_file(parent).expect("clean blocker file");
    }

    /// Regression for the `activate()` path: when persistence fails, the
    /// switch must stay armed and the degradation must be recorded in state
    /// rather than being reported as a durable activation. `activate()` also
    /// emits a distinct Warn in this case; capturing it would require a
    /// tracing subscriber, so this test asserts the observable in-memory
    /// state that the log must agree with.
    #[test]
    fn activate_on_persist_failure_stays_activated_and_records_degradation() {
        let path = temp_path("activate-persist-fail");
        // Build the switch while the state file is still absent (normal first
        // start), then make the configured parent unwritable so persist()
        // fails.
        let switch = KillSwitch::new("admin-token", Some(path.clone()));
        assert!(!switch.is_activated());

        let parent = path.parent().expect("state path has a parent");
        std::fs::remove_dir_all(parent).expect("remove temp dir");
        std::fs::write(parent, b"blocker").expect("create blocker file");

        switch.activate("operator-requested-reason".into());

        let status = switch.status();
        assert!(
            status.activated,
            "failed persist must leave the switch armed (fail-closed)"
        );
        assert_eq!(
            status.reason.as_deref(),
            Some(PERSIST_FAILURE_REASON),
            "degradation must be recorded instead of the caller's reason"
        );

        std::fs::remove_file(parent).expect("clean blocker file");
    }

    /// `activate_with_actor` must make the public `activated_by` audit field
    /// reflect the real caller, both in memory and after a restart.
    #[test]
    fn activate_with_actor_records_actor_in_status_and_state_file() {
        let path = temp_path("actor");
        let switch = KillSwitch::new("admin-token", Some(path.clone()));

        switch.activate_with_actor("maintenance".into(), "admin:alice".into());

        let status = switch.status();
        assert!(status.activated);
        assert_eq!(status.reason.as_deref(), Some("maintenance"));
        assert_eq!(status.activated_by.as_deref(), Some("admin:alice"));

        let restored = KillSwitch::new("admin-token", Some(path.clone()));
        assert_eq!(
            restored.status().activated_by.as_deref(),
            Some("admin:alice")
        );

        restored
            .deactivate("admin-token")
            .expect("admin token deactivates");
        assert_eq!(restored.status().activated_by, None);

        std::fs::remove_dir_all(path.parent().unwrap()).expect("clean temp dir");
    }

    /// The legacy entry point and the token-gated path must keep their
    /// historical `"system"` attribution until their callers pass an actor.
    #[test]
    fn legacy_activation_paths_keep_system_actor() {
        let switch = KillSwitch::new("admin-token", None);
        switch.activate("legacy".into());
        assert_eq!(switch.status().activated_by.as_deref(), Some("system"));

        let token_switch = KillSwitch::new("admin-token", None);
        token_switch
            .activate_with_token("token-request".into(), "admin-token")
            .expect("valid admin token");
        assert_eq!(
            token_switch.status().activated_by.as_deref(),
            Some("system")
        );
    }

    /// The new actor-aware entry point must not weaken the fail-closed
    /// behavior on a persist failure: the switch stays armed and records
    /// `PERSIST_FAILURE_REASON` just like `activate()` does.
    #[test]
    fn activate_with_actor_on_persist_failure_stays_armed_and_records_degradation() {
        let path = temp_path("actor-persist-fail");
        let switch = KillSwitch::new("admin-token", Some(path.clone()));
        assert!(!switch.is_activated());

        let parent = path.parent().expect("state path has a parent");
        std::fs::remove_dir_all(parent).expect("remove temp dir");
        std::fs::write(parent, b"blocker").expect("create blocker file");

        switch.activate_with_actor("operator-requested-reason".into(), "admin:alice".into());

        let status = switch.status();
        assert!(
            status.activated,
            "failed persist must leave the switch armed"
        );
        assert_eq!(status.reason.as_deref(), Some(PERSIST_FAILURE_REASON));

        std::fs::remove_file(parent).expect("clean blocker file");
    }

    /// A persistence failure must be a typed internal error, never the same
    /// authentication error produced by a wrong token. Callers currently map
    /// every `Err` to `Unauthenticated`/401, so the error *type* is the signal
    /// they need to correct that mapping without parsing message text.
    #[test]
    fn persist_failure_is_typed_as_internal_not_authentication() {
        let path = temp_path("persist-error-kind");
        let switch = KillSwitch::new("admin-token", Some(path.clone()));
        assert!(!switch.is_activated());

        let parent = path.parent().expect("state path has a parent");
        std::fs::remove_dir_all(parent).expect("remove temp dir");
        std::fs::write(parent, b"blocker").expect("create blocker file");

        let bad_token = switch
            .deactivate("wrong-token")
            .expect_err("wrong token is rejected");
        assert!(
            bad_token
                .downcast_ref::<KillSwitchError>()
                .is_some_and(|error| matches!(error, KillSwitchError::InvalidAdminToken)),
            "a bad token must be the only authentication error"
        );

        let persist_error = switch
            .deactivate("admin-token")
            .expect_err("failed persist must surface as an error");
        let kind = persist_error
            .downcast_ref::<KillSwitchError>()
            .expect("persist failure must carry a typed error");
        assert!(
            matches!(kind, KillSwitchError::Persistence { .. }),
            "persist failure must be typed as internal, not authentication"
        );
        let message = persist_error.to_string();
        assert!(
            message.contains("NOT an authentication failure"),
            "message must rule out an auth failure: {message}"
        );
        assert!(
            message.contains("remains activated"),
            "message must keep the fail-closed signal: {message}"
        );
        assert!(switch.is_activated(), "fail-closed must be preserved");

        std::fs::remove_file(parent).expect("clean blocker file");
    }

    /// The shared helper must reject both a content mismatch and a length
    /// mismatch, and must accept an empty token only when both sides are empty.
    #[test]
    fn shared_constant_time_eq_helper_covers_content_and_length() {
        assert!(constant_time_eq("admin-token", "admin-token"));
        assert!(!constant_time_eq("admin-token", "admin-tokel"));
        assert!(!constant_time_eq("admin-token", "admin-token-extra"));
        assert!(!constant_time_eq("", "non-empty"));
        assert!(constant_time_eq("", ""));
    }

    /// Every token gate must go through the shared helper. The observable part
    /// is that both the valid and the invalid token are decided by the same
    /// hash comparison used by `activate_with_token` / `deactivate`.
    #[test]
    fn status_token_verification_matches_the_gates_comparison() {
        let switch = KillSwitch::new("admin-token", None);
        assert!(switch.verify_admin_token("admin-token"));
        assert!(!switch.verify_admin_token("admin-tokel"));
        assert!(!switch.verify_admin_token(""));
        assert!(!switch.verify_admin_token("admin-token-extra"));
    }

    /// The wire projection must never carry a caller-supplied reason or actor.
    /// Keep the raw status unchanged for internal audit consumers.
    #[test]
    fn redacted_status_uses_a_fixed_vocabulary() {
        let switch = KillSwitch::new("admin-token", None);
        switch.activate_with_actor(
            "SENTINEL-REASON-DO-NOT-LEAK".into(),
            "SENTINEL-ACTOR-DO-NOT-LEAK".into(),
        );
        let raw = switch.status();
        assert_eq!(raw.reason.as_deref(), Some("SENTINEL-REASON-DO-NOT-LEAK"));
        assert_eq!(
            raw.activated_by.as_deref(),
            Some("SENTINEL-ACTOR-DO-NOT-LEAK")
        );

        let view = raw.redacted();
        assert!(view.activated);
        assert_eq!(view.activated_at, raw.activated_at);
        assert_eq!(
            view.reason.as_deref(),
            Some(status_vocabulary::REASON_OPERATOR_DEFINED)
        );
        assert_eq!(
            view.activated_by.as_deref(),
            Some(status_vocabulary::ACTOR_OPERATOR)
        );
        for field in [view.reason.as_deref(), view.activated_by.as_deref()] {
            let field = field.expect("activated status carries metadata");
            assert!(!field.contains("SENTINEL"));
        }
    }

    /// Internal failure categories stay distinguishable through the fixed
    /// vocabulary; only free-form text is collapsed.
    #[test]
    fn redacted_status_classifies_internal_failure_reasons() {
        let switch = KillSwitch::new("admin-token", None);
        switch.arm(POISONED_LOCK_REASON.into());
        assert_eq!(
            switch.status().redacted().reason.as_deref(),
            Some(status_vocabulary::REASON_LOCK_POISONED)
        );

        switch.arm(PERSIST_FAILURE_REASON.into());
        assert_eq!(
            switch.status().redacted().reason.as_deref(),
            Some(status_vocabulary::REASON_PERSISTENCE_FAILURE)
        );

        switch.arm(CLOCK_FAILURE_REASON.into());
        assert_eq!(
            switch.status().redacted().reason.as_deref(),
            Some(status_vocabulary::REASON_CLOCK_FAILURE)
        );

        switch.arm("unreadable kill switch state: detail".into());
        let view = switch.status().redacted();
        assert_eq!(
            view.reason.as_deref(),
            Some(status_vocabulary::REASON_STATE_UNREADABLE)
        );
        assert_eq!(
            view.activated_by.as_deref(),
            Some(status_vocabulary::ACTOR_SYSTEM)
        );
    }
}
