use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde::Serialize;
use zeroize::Zeroizing;

use crate::crypto;
use crate::models::PgToolBackend;
use crate::storage::{self, Storage};

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
    #[serde(default, skip_serializing_if = "is_false")]
    pub timed_out: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// A workspace entry as shown on the unlock screen. Names are not secret;
/// `initialized` tells whether the vault already has a master password.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub initialized: bool,
}

/// Session-bound authentication record: the decryption key plus which
/// workspace it belongs to.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub key: Arc<Zeroizing<Vec<u8>>>,
    pub workspace: String,
}

/// Shared state for the HTTP server. Held behind `Arc` so every handler can
/// reach the same workspace storages, session map, operation registry and
/// health cache.
pub struct WebState {
    pub workspaces_dir: PathBuf,
    pub workspace_storages: Mutex<HashMap<String, Arc<Mutex<Storage>>>>,
    pub sessions: Mutex<HashMap<String, SessionRecord>>,
    pub ops: OperationRegistry,
    pub health: Arc<Mutex<HashMap<String, HealthInfo>>>,
    pub pg_tool_backend: PgToolBackend,
}

impl WebState {
    pub fn new() -> Result<Self> {
        let data_dir = storage::app_data_dir()?;
        // The web server may be the first entry point after an upgrade, so it
        // must run the legacy vault migration just like the TUI does.
        storage::migrate_legacy_vault_at(&data_dir)?;
        Self::open_at(&data_dir)
    }

    /// Builds state rooted at `data_dir/workspaces`. Used by tests to point at
    /// a temporary directory.
    pub fn open_at(data_dir: &std::path::Path) -> Result<Self> {
        let workspaces_dir = data_dir.join("workspaces");
        std::fs::create_dir_all(&workspaces_dir)
            .with_context(|| format!("failed to create {}", workspaces_dir.display()))?;
        Ok(Self {
            workspaces_dir,
            workspace_storages: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            ops: OperationRegistry::new(),
            health: Arc::new(Mutex::new(HashMap::new())),
            pg_tool_backend: crate::postgres::check_pg_tools(),
        })
    }

    pub fn workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let mut out = Vec::new();
        for name in storage::list_workspaces_at(&self.workspaces_dir)? {
            let vault = Storage::open_at(storage::workspace_file(&self.workspaces_dir, &name)?)?;
            let initialized = vault.is_initialized()?;
            out.push(WorkspaceInfo { name, initialized });
        }
        Ok(out)
    }

    /// Opens a workspace vault, creating the file if it does not exist yet,
    /// and caches it. Only called with names that are already validated (or
    /// validated right after, in `initialize`).
    pub fn storage_for(&self, workspace: &str) -> Result<Arc<Mutex<Storage>>> {
        let mut map = self.workspace_storages.lock().unwrap();
        if let Some(existing) = map.get(workspace) {
            return Ok(existing.clone());
        }
        let vault = Storage::open_at(storage::workspace_file(&self.workspaces_dir, workspace)?)?;
        let shared = Arc::new(Mutex::new(vault));
        map.insert(workspace.to_owned(), shared.clone());
        Ok(shared)
    }

    /// Deletes a workspace vault. Refuses while any session is bound to it.
    /// Returns whether the vault file existed.
    pub fn delete_workspace(&self, workspace: &str) -> Result<bool> {
        storage::validate_workspace_name(workspace)?;
        {
            let sessions = self.sessions.lock().unwrap();
            if sessions.values().any(|s| s.workspace == workspace) {
                anyhow::bail!(
                    "workspace '{workspace}' has active sessions; lock it before removing"
                );
            }
        }
        self.workspace_storages.lock().unwrap().remove(workspace);
        storage::delete_workspace_at(&self.workspaces_dir, workspace)
    }

    /// Derives a key from the master password and opens a session bound to
    /// `workspace`, returning a bearer token. Fails when the password is
    /// wrong.
    pub fn create_session(&self, workspace: &str, password: &str) -> Result<String> {
        let vault = self.storage_for(workspace)?;
        let config = vault.lock().unwrap().load_kdf_config()?;
        let key = crypto::derive_key(password, &config)?;
        vault
            .lock()
            .unwrap()
            .verify_master_password(key.as_slice())?;
        let token = new_session_token()?;
        self.sessions.lock().unwrap().insert(
            token.clone(),
            SessionRecord {
                key: Arc::new(key),
                workspace: workspace.to_owned(),
            },
        );
        Ok(token)
    }

    /// Initializes the master password of `workspace` (creating the vault
    /// file when needed) and opens a session.
    pub fn initialize(&self, workspace: &str, password: &str) -> Result<String> {
        storage::validate_workspace_name(workspace)?;
        let vault = self.storage_for(workspace)?;
        {
            let guard = vault.lock().unwrap();
            if guard.is_initialized()? {
                anyhow::bail!("workspace '{workspace}' already has a master password");
            }
            guard.initialize_master_password(password)?;
        }
        self.create_session(workspace, password)
    }

    pub fn session_record(&self, token: &str) -> Option<SessionRecord> {
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
        let state = WebState::open_at(dir.path()).unwrap();
        (state, dir)
    }

    #[test]
    fn initialize_and_unlock_round_trip() {
        let (state, _dir) = test_state();
        let token = state.initialize("default", "hunter2").unwrap();
        assert!(state.session_record(&token).is_some());
        assert!(state.create_session("default", "hunter2").is_ok());
    }

    #[test]
    fn wrong_password_is_rejected() {
        let (state, _dir) = test_state();
        state.initialize("default", "correct horse").unwrap();
        assert!(state.create_session("default", "wrong").is_err());
    }

    #[test]
    fn sessions_are_unique_and_lockable() {
        let (state, _dir) = test_state();
        state.initialize("default", "pw").unwrap();
        let a = state.create_session("default", "pw").unwrap();
        let b = state.create_session("default", "pw").unwrap();
        assert_ne!(a, b);
        assert_eq!(
            state.session_record(&a).unwrap().workspace,
            "default",
            "session must be bound to its workspace"
        );
        state.lock(&a);
        assert!(state.session_record(&a).is_none());
        assert!(state.session_record(&b).is_some());
    }

    #[test]
    fn wipe_sessions_clears_all_keys() {
        let (state, _dir) = test_state();
        state.initialize("default", "pw").unwrap();
        let token = state.create_session("default", "pw").unwrap();
        state.wipe_sessions();
        assert!(state.session_record(&token).is_none());
    }

    #[test]
    fn workspaces_are_independent_vaults() {
        let (state, _dir) = test_state();
        state.initialize("old", "old-pass").unwrap();
        state.initialize("new", "new-pass").unwrap();

        assert!(state.create_session("old", "new-pass").is_err());
        assert!(state.create_session("new", "new-pass").is_ok());

        let workspaces = state.workspaces().unwrap();
        let names: Vec<_> = workspaces.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["new", "old"]);
        assert!(workspaces.iter().all(|w| w.initialized));
    }

    #[test]
    fn delete_workspace_refuses_active_sessions() {
        let (state, dir) = test_state();
        let _keeper = state.initialize("keeper", "pw").unwrap();
        let doomed_init = state.initialize("doomed", "pw").unwrap();
        let doomed_session = state.create_session("doomed", "pw").unwrap();

        assert!(state.delete_workspace("doomed").is_err());
        state.lock(&doomed_init);
        state.lock(&doomed_session);
        assert!(state.delete_workspace("doomed").unwrap());

        let workspaces = state.workspaces().unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "keeper");
        drop(dir);
    }

    #[test]
    fn uninitialized_workspace_is_reported_as_such() {
        let (state, _dir) = test_state();
        state.storage_for("fresh").unwrap();
        let workspaces = state.workspaces().unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].name, "fresh");
        assert!(!workspaces[0].initialized);
    }
}
