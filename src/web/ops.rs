use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use uuid::Uuid;

const MAX_LOGS: usize = 300;

/// The final result of a background operation, in a web-friendly shape.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationResult {
    Provision {
        database_name: String,
        application_name: String,
        role_name: String,
        connection_string: String,
        extra_username: Option<String>,
        extra_connection_string: Option<String>,
    },
    Migrate {
        database_name: String,
        instance_name: String,
        connection_string: String,
    },
    Backup {
        file_path: String,
        database_name: String,
        database_names: Vec<String>,
    },
    Restore {
        restored: Vec<String>,
        skipped: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationStatus {
    Running,
    Done,
}

#[derive(Debug, Clone)]
pub struct OperationState {
    pub logs: Vec<String>,
    pub status: OperationStatus,
    pub result: Option<Result<OperationResult, String>>,
}

pub type OperationHandle = Arc<Mutex<OperationState>>;

#[derive(Debug, Default)]
pub struct OperationRegistry {
    map: Mutex<HashMap<Uuid, OperationHandle>>,
}

impl OperationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawns a background operation. `work` receives a log emitter and returns
    /// either the final result or an error string. Logs are capped to avoid
    /// unbounded memory growth.
    pub fn spawn<F>(&self, start_log: String, work: F) -> Uuid
    where
        F: FnOnce(&dyn Fn(String)) -> Result<OperationResult, String> + Send + 'static,
    {
        let id = Uuid::now_v7();
        let state = Arc::new(Mutex::new(OperationState {
            logs: vec![start_log],
            status: OperationStatus::Running,
            result: None,
        }));
        self.map.lock().unwrap().insert(id, state.clone());

        std::thread::spawn(move || {
            let log = |msg: String| {
                let mut guard = state.lock().unwrap();
                guard.logs.push(msg);
                if guard.logs.len() > MAX_LOGS {
                    let overflow = guard.logs.len() - MAX_LOGS;
                    guard.logs.drain(0..overflow);
                }
            };
            let result = work(&log);
            let mut guard = state.lock().unwrap();
            guard.status = OperationStatus::Done;
            guard.result = Some(result);
        });

        id
    }

    pub fn get(&self, id: Uuid) -> Option<OperationHandle> {
        self.map.lock().unwrap().get(&id).cloned()
    }

    pub fn remove(&self, id: Uuid) {
        self.map.lock().unwrap().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_until_done(handle: &OperationHandle) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if handle.lock().unwrap().status == OperationStatus::Done {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("operation did not finish in time");
    }

    #[test]
    fn runner_collects_logs_and_result() {
        let registry = OperationRegistry::new();
        let id = registry.spawn("start".to_owned(), |log| {
            log("step one".to_owned());
            log("step two".to_owned());
            Ok(OperationResult::Backup {
                file_path: "/tmp/x.pgdump.enc".into(),
                database_name: "orders".into(),
                database_names: vec!["orders".into()],
            })
        });
        let handle = registry.get(id).expect("handle");
        wait_until_done(&handle);
        let state = handle.lock().unwrap();
        assert_eq!(state.status, OperationStatus::Done);
        assert_eq!(state.logs, ["start", "step one", "step two"]);
        let result = state.result.as_ref().unwrap().as_ref().unwrap();
        match result {
            OperationResult::Backup { file_path, .. } => {
                assert_eq!(file_path, "/tmp/x.pgdump.enc");
            }
            _ => panic!("expected backup result"),
        }
    }

    #[test]
    fn runner_captures_errors() {
        let registry = OperationRegistry::new();
        let id = registry.spawn("start".to_owned(), |_| Err("boom".to_owned()));
        let handle = registry.get(id).expect("handle");
        wait_until_done(&handle);
        let state = handle.lock().unwrap();
        let error = state.result.as_ref().unwrap().as_ref().unwrap_err();
        assert_eq!(error, "boom");
    }

    #[test]
    fn registry_removes_and_gets_none() {
        let registry = OperationRegistry::new();
        let id = registry.spawn("start".to_owned(), |_| {
            Ok(OperationResult::Provision {
                database_name: "d".into(),
                application_name: "a".into(),
                role_name: "r".into(),
                connection_string: "c".into(),
                extra_username: None,
                extra_connection_string: None,
            })
        });
        registry.remove(id);
        assert!(registry.get(id).is_none());
    }
}
