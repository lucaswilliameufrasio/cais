#[derive(Debug, Clone)]
pub struct KdfConfig {
    pub salt: Vec<u8>,
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

#[derive(Debug, Clone)]
pub struct EncryptedValue {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct InstanceSecret {
    pub encrypted: EncryptedValue,
}

#[derive(Debug, Clone)]
pub struct ProvisionedDatabaseRecord {
    pub id: i64,
    pub instance_name: String,
    pub database_name: String,
    pub application_name: String,
    pub role_name: String,
    pub encrypted: EncryptedValue,
    pub database_created: bool,
    pub role_created: bool,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    pub database_name: String,
    pub application_name: String,
}

#[derive(Debug, Clone)]
pub struct ProvisionOutcome {
    pub database_name: String,
    pub application_name: String,
    pub role_name: String,
    pub connection_string: String,
    pub database_created: bool,
    pub role_created: bool,
}

#[derive(Debug, Clone)]
pub struct ExtraUserProvisionRequest {
    pub database_name: String,
    pub username: String,
    pub application_name: String,
}

#[derive(Debug, Clone)]
pub struct ExtraUserProvisionOutcome {
    pub database_name: String,
    pub username: String,
    pub application_name: String,
    pub connection_string: String,
    pub role_created: bool,
    pub grants_applied: bool,
}

#[derive(Debug, Clone)]
pub struct ProvisionFullRequest {
    pub database_name: String,
    pub application_name: String,
    pub extra_username: Option<String>,
    pub extra_application_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProvisionFullOutcome {
    pub database_name: String,
    pub application_name: String,
    pub role_name: String,
    pub database_connection_string: String,
    pub database_created: bool,
    pub role_created: bool,
    pub extra_username: Option<String>,
    pub extra_connection_string: Option<String>,
    pub extra_role_created: Option<bool>,
    pub extra_grants_applied: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ProvisionedExtraUserRecord {
    pub id: i64,
    pub instance_name: String,
    pub database_name: String,
    pub username: String,
    pub application_name: String,
    pub encrypted: EncryptedValue,
    pub role_created: bool,
    pub grants_applied: bool,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub enum SavedConnectionRecord {
    Database(ProvisionedDatabaseRecord),
    ExtraUser(ProvisionedExtraUserRecord),
    Instance {
        name: String,
        encrypted: EncryptedValue,
    },
}

#[derive(Debug, Clone)]
pub struct MigrateRequest {
    pub dest_database_name: String,
    pub application_name: String,
    pub include_extra_users: bool,
}

#[derive(Debug, Clone)]
pub enum PgToolBackend {
    Native {
        dump_ver: String,
        restore_ver: String,
    },
    Docker {
        image: String,
    },
    NotFound,
}

#[derive(Debug, Clone)]
pub struct BackupOutcome {
    pub file_path: String,
    pub database_name: String,
    pub database_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DiscoveredDatabase {
    pub name: String,
    pub owner: String,
    pub encoding: String,
    pub tablespace: String,
    pub connection_limit: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TablespaceMode {
    #[default]
    Flatten,
    Preserve,
}

#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub include_globals: bool,
    pub include_role_passwords: bool,
    pub tablespace_mode: TablespaceMode,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            include_globals: true,
            include_role_passwords: false,
            tablespace_mode: TablespaceMode::Flatten,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedDatabaseUrl {
    pub username: String,
    pub host: String,
    pub port: u16,
    pub database: String,
}

#[derive(Debug, Clone)]
pub struct ActiveQuery {
    pub pid: i32,
    pub user: String,
    pub database: String,
    pub client_addr: String,
    pub duration_secs: i64,
    pub state: String,
    pub query: String,
}

#[derive(Debug, Clone)]
pub struct BackupFile {
    pub path: std::path::PathBuf,
    pub filename: String,
    pub size: u64,
    pub modified: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseEngine {
    #[default]
    PostgreSQL,
}

impl DatabaseEngine {
    pub fn as_str(&self) -> &'static str {
        match self {
            DatabaseEngine::PostgreSQL => "postgresql",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackupMetadata {
    pub machine_id: String,
    pub hostname: String,
    pub instance_name: String,
    pub database_name: String,
    pub application_name: String,
    pub engine: String,
    pub timestamp: String,
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_engine_default_is_postgresql() {
        assert_eq!(DatabaseEngine::default(), DatabaseEngine::PostgreSQL);
    }

    #[test]
    fn database_engine_as_str() {
        assert_eq!(DatabaseEngine::PostgreSQL.as_str(), "postgresql");
    }

    #[test]
    fn database_engine_serde_round_trip() {
        let json = serde_json::to_string(&DatabaseEngine::PostgreSQL).unwrap();
        assert_eq!(json, "\"postgresql\"");
        let deserialized: DatabaseEngine = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, DatabaseEngine::PostgreSQL);
    }

    #[test]
    fn backup_metadata_serde_round_trip() {
        let meta = BackupMetadata {
            machine_id: "abc-123".into(),
            hostname: "server-1".into(),
            instance_name: "prod".into(),
            database_name: "orders".into(),
            application_name: "orders-api".into(),
            engine: "postgresql".into(),
            timestamp: "2026-07-08T12:00:00Z".into(),
            version: "0.1.0".into(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: BackupMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.machine_id, meta.machine_id);
        assert_eq!(deserialized.hostname, meta.hostname);
        assert_eq!(deserialized.instance_name, meta.instance_name);
        assert_eq!(deserialized.database_name, meta.database_name);
        assert_eq!(deserialized.application_name, meta.application_name);
        assert_eq!(deserialized.engine, meta.engine);
        assert_eq!(deserialized.timestamp, meta.timestamp);
        assert_eq!(deserialized.version, meta.version);
    }

    #[test]
    fn backup_metadata_contains_all_fields_in_json() {
        let meta = BackupMetadata {
            machine_id: "m1".into(),
            hostname: "h1".into(),
            instance_name: "i1".into(),
            database_name: "d1".into(),
            application_name: "a1".into(),
            engine: "postgresql".into(),
            timestamp: "t1".into(),
            version: "v1".into(),
        };
        let json = serde_json::to_value(&meta).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("machine_id"));
        assert!(obj.contains_key("hostname"));
        assert!(obj.contains_key("instance_name"));
        assert!(obj.contains_key("database_name"));
        assert!(obj.contains_key("application_name"));
        assert!(obj.contains_key("engine"));
        assert!(obj.contains_key("timestamp"));
        assert!(obj.contains_key("version"));
    }
}
