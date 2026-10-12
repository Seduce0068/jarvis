//! Kill Switch - emergency stop for all automated actions

use anyhow::Result;
use hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

pub struct KillSwitch {
    activated: AtomicBool,
    activated_at: AtomicU64,
    reason: std::sync::RwLock<Option<String>>,
    activated_by: std::sync::RwLock<Option<String>>,
    admin_token_hash: String,
}

impl KillSwitch {
    pub fn new(admin_token: &str) -> Self {
        Self {
            activated: AtomicBool::new(false),
            activated_at: AtomicU64::new(0),
            reason: std::sync::RwLock::new(None),
            activated_by: std::sync::RwLock::new(None),
            admin_token_hash: Self::hash_token(admin_token),
        }
    }

    pub fn activate(&self, reason: String) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.activated.store(true, Ordering::SeqCst);
        self.activated_at.store(now, Ordering::SeqCst);
        *self.reason.write().unwrap() = Some(reason.clone());
        *self.activated_by.write().unwrap() = Some("system".into());
        warn!("KILL SWITCH ACTIVATED: {}", reason);
    }

    pub fn deactivate(&self, admin_token: &str) -> Result<()> {
        if Self::hash_token(admin_token) != self.admin_token_hash {
            return Err(anyhow::anyhow!("Invalid admin token"));
        }
        self.activated.store(false, Ordering::SeqCst);
        self.activated_at.store(0, Ordering::SeqCst);
        *self.reason.write().unwrap() = None;
        *self.activated_by.write().unwrap() = None;
        warn!("Kill switch deactivated by admin");
        Ok(())
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
