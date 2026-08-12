use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Serialize;
use zeroize::Zeroizing;

use crate::crypto;
use crate::models::PgToolBackend;
use crate::storage::Storage;

use super::ops::OperationRegistry;

/// Serialized health-check result used by the web dashboard.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HealthInfo {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Shared state for the HTTP server. Held behind `Arc` so every handler can
/// reach the same storage, session map, operation registry and health cache.
pub struct WebState {
    pub storage: Arc<Mutex<Storage>>,
    pub sessions: Mutex<HashMap<String, Arc<Zeroizing<Vec<u8>>>>>,
    pub ops: OperationRegistry,
    pub health: Arc<Mutex<HashMap<String, HealthInfo>>>,
    pub machine_id: String,
    pub hostname: String,
    pub pg_tool_backend: PgToolBackend,
}

impl WebState {
    pub fn new() -> Result<Self> {
        Self::from_storage(Storage::open()?)
    }

    /// Builds state from an already-open storage. Kept public so tests and the
    /// web server can point at a specific SQLite file.
    pub fn from_storage(storage: Storage) -> Result<Self> {
        let (machine_id, hostname) = storage.ensure_machine_identity()?;
        Ok(Self {
            storage: Arc::new(Mutex::new(storage)),
            sessions: Mutex::new(HashMap::new()),
            ops: OperationRegistry::new(),
            health: Arc::new(Mutex::new(HashMap::new())),
            machine_id,
            hostname,
            pg_tool_backend: crate::postgres::check_pg_tools(),
        })
    }

    pub fn is_initialized(&self) -> Result<bool> {
        self.storage.lock().unwrap().is_initialized()
    }

    /// Derives a key from the master password and opens a session, returning a
    /// bearer token. Fails when the password is wrong.
    pub fn create_session(&self, password: &str) -> Result<String> {
        let storage = self.storage.lock().unwrap();
        let config = storage.load_kdf_config()?;
        let key = crypto::derive_key(password, &config)?;
        storage.verify_master_password(key.as_slice())?;
        drop(storage);

        let token = new_session_token()?;
        self.sessions
            .lock()
            .unwrap()
            .insert(token.clone(), Arc::new(key));
        Ok(token)
    }

    /// Initializes the master password on first run and opens a session.
    pub fn initialize(&self, password: &str) -> Result<String> {
        let storage = self.storage.lock().unwrap();
        let config = storage
            .initialize_master_password(password)
            .context("failed to initialize master password")?;
        let key = crypto::derive_key(password, &config)?;
        drop(storage);

        let token = new_session_token()?;
        self.sessions
            .lock()
            .unwrap()
            .insert(token.clone(), Arc::new(key));
        Ok(token)
    }

    pub fn session_key(&self, token: &str) -> Option<Arc<Zeroizing<Vec<u8>>>> {
        self.sessions.lock().unwrap().get(token).cloned()
    }

    pub fn lock(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }

    /// Zeros every live session key. Called on graceful shutdown.
    pub fn wipe_sessions(&self) {
        self.sessions.lock().unwrap().clear();
    }
}

fn new_session_token() -> Result<String> {
    let bytes = crypto::random_bytes(32)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    fn test_state() -> (WebState, TempDir) {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let state = WebState::from_storage(storage).unwrap();
        (state, dir)
    }

    #[test]
    fn initialize_and_unlock_round_trip() {
        let (state, _dir) = test_state();
        let token = state.initialize("hunter2").unwrap();
        assert!(state.session_key(&token).is_some());
        assert!(state.create_session("hunter2").is_ok());
    }

    #[test]
    fn wrong_password_is_rejected() {
        let (state, _dir) = test_state();
        state.initialize("correct horse").unwrap();
        assert!(state.create_session("wrong").is_err());
    }

    #[test]
    fn sessions_are_unique_and_lockable() {
        let (state, _dir) = test_state();
        state.initialize("pw").unwrap();
        let a = state.create_session("pw").unwrap();
        let b = state.create_session("pw").unwrap();
        assert_ne!(a, b);
        assert!(state.session_key(&a).is_some());
        state.lock(&a);
        assert!(state.session_key(&a).is_none());
        assert!(state.session_key(&b).is_some());
    }

    #[test]
    fn wipe_sessions_clears_all_keys() {
        let (state, _dir) = test_state();
        state.initialize("pw").unwrap();
        let token = state.create_session("pw").unwrap();
        assert!(state.session_key(&token).is_some());
        state.wipe_sessions();
        assert!(state.session_key(&token).is_none());
    }
}
