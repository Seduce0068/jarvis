//! Kill Switch - emergency stop for all automated actions

use anyhow::Result;
use hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

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
    reason: std::sync::RwLock<Option<String>>,
    activated_by: std::sync::RwLock<Option<String>>,
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
            reason: std::sync::RwLock::new(None),
            activated_by: std::sync::RwLock::new(None),
            admin_token_hash: Self::hash_token(admin_token),
            state_path,
        };
        switch.restore();
        switch
    }

    pub fn activate(&self, reason: String) {
        self.arm(reason.clone());
        self.persist();
        warn!("KILL SWITCH ACTIVATED: {}", reason);
    }

    /// Set the in-memory state without persisting. Used by [`Self::restore`]
    /// so a corrupt state file is not overwritten during recovery.
    fn arm(&self, reason: String) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.activated.store(true, Ordering::SeqCst);
        self.activated_at.store(now, Ordering::SeqCst);
        *self.reason.write().unwrap() = Some(reason);
        *self.activated_by.write().unwrap() = Some("system".into());
    }

    pub fn deactivate(&self, admin_token: &str) -> Result<()> {
        if Self::hash_token(admin_token) != self.admin_token_hash {
            return Err(anyhow::anyhow!("Invalid admin token"));
        }
        self.activated.store(false, Ordering::SeqCst);
        self.activated_at.store(0, Ordering::SeqCst);
        *self.reason.write().unwrap() = None;
        *self.activated_by.write().unwrap() = None;
        self.persist();
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
                *self.reason.write().unwrap() = state.reason;
                *self.activated_by.write().unwrap() = state.activated_by;
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

    fn persist(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        let state = PersistedState {
            activated: self.activated.load(Ordering::SeqCst),
            activated_at: self.activated_at.load(Ordering::SeqCst),
            reason: self.reason.read().unwrap().clone(),
            activated_by: self.activated_by.read().unwrap().clone(),
        };
        let bytes = match serde_json::to_vec_pretty(&state) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!("Failed to serialize kill switch state: {error}");
                return;
            }
        };
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            warn!("Failed to create {}: {error}", parent.display());
            return;
        }
        if let Err(error) = std::fs::write(path, bytes) {
            warn!(
                "Failed to persist kill switch state to {}: {error}",
                path.display()
            );
        }
    }

    pub fn is_activated(&self) -> bool {
        self.activated.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> KillSwitchStatus {
        KillSwitchStatus {
            activated: self.activated.load(Ordering::SeqCst),
            activated_at: if self.activated.load(Ordering::SeqCst) {
                Some(self.activated_at.load(Ordering::SeqCst))
            } else {
                None
            },
            reason: self.reason.read().unwrap().clone(),
            activated_by: self.activated_by.read().unwrap().clone(),
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
}
