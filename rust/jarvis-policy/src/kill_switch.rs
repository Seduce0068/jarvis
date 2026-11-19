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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitchStatus {
    pub activated: bool,
    pub activated_at: Option<u64>,
    pub reason: Option<String>,
    pub activated_by: Option<String>,
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
        if Self::hash_token(admin_token) != self.admin_token_hash {
            return Err(anyhow::anyhow!("Invalid admin token"));
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
        if Self::hash_token(admin_token) != self.admin_token_hash {
            return Err(anyhow::anyhow!("Invalid admin token"));
        }
        self.activated.store(false, Ordering::SeqCst);
        self.activated_at.store(0, Ordering::SeqCst);
        Self::write_slot(&self.reason, None);
        Self::write_slot(&self.activated_by, None);
        let persist_result = self.persist();
        // A persist failure fails closed by re-arming the switch. Never tell
        // the operator the deactivation succeeded when it did not stick.
        if persist_result.is_err() || self.is_activated() {
            return Err(anyhow::anyhow!(
                "kill switch state could not be persisted; switch remains activated"
            ));
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
                return Err(anyhow::anyhow!(
                    "failed to serialize kill switch state: {error}"
                ));
            }
        };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            warn!("Failed to create {}: {error}", parent.display());
            self.arm(PERSIST_FAILURE_REASON.into());
            return Err(anyhow::anyhow!(
                "failed to create {}: {error}",
                parent.display()
            ));
        }
        if let Err(error) = std::fs::write(path, bytes) {
            warn!(
                "Failed to persist kill switch state to {}: {error}",
                path.display()
            );
            // `arm` never calls `persist`, so this cannot recurse.
            self.arm(PERSIST_FAILURE_REASON.into());
            return Err(anyhow::anyhow!(
                "failed to persist kill switch state to {}: {error}",
                path.display()
            ));
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
}
