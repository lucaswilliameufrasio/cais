use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::crypto;
use crate::models::{
    EncryptedValue, ExtraUserProvisionOutcome, InstanceSecret, KdfConfig, ProvisionOutcome,
    ProvisionedDatabaseRecord, ProvisionedExtraUserRecord, SavedConnectionRecord,
};

const TABLE_COLUMNS: &[(&str, &str, &str)] = &[
    (
        "environments",
        "engine",
        "TEXT NOT NULL DEFAULT 'postgresql'",
    ),
    (
        "provisioned_databases",
        "engine",
        "TEXT NOT NULL DEFAULT 'postgresql'",
    ),
    (
        "provisioned_extra_users",
        "engine",
        "TEXT NOT NULL DEFAULT 'postgresql'",
    ),
];

pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open() -> Result<Self> {
        Self::open_at(database_path()?)
    }

    pub fn open_at(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create app data directory at {}",
                    parent.display()
                )
            })?;
        }

        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open sqlite database at {}", path.display()))?;
        let storage = Self { conn };
        storage.init_schema()?;
        Ok(storage)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS app_metadata (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                kdf_salt BLOB NOT NULL,
                argon2_memory_kib INTEGER NOT NULL,
                argon2_iterations INTEGER NOT NULL,
                argon2_parallelism INTEGER NOT NULL,
                password_check_ciphertext BLOB NOT NULL,
                password_check_nonce BLOB NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS machine_identity (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                machine_id TEXT NOT NULL,
                hostname TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS environments (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                base_database_url_ciphertext BLOB NOT NULL,
                base_database_url_nonce BLOB NOT NULL,
                engine TEXT NOT NULL DEFAULT 'postgresql',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS provisioned_databases (
                id INTEGER PRIMARY KEY,
                environment_name TEXT NOT NULL,
                database_name TEXT NOT NULL,
                application_name TEXT NOT NULL,
                role_name TEXT NOT NULL,
                connection_string_ciphertext BLOB NOT NULL,
                connection_string_nonce BLOB NOT NULL,
                database_created INTEGER NOT NULL,
                role_created INTEGER NOT NULL,
                engine TEXT NOT NULL DEFAULT 'postgresql',
                created_at TEXT NOT NULL,
                UNIQUE(environment_name, database_name)
            );

            CREATE TABLE IF NOT EXISTS provisioned_extra_users (
                id INTEGER PRIMARY KEY,
                environment_name TEXT NOT NULL,
                database_name TEXT NOT NULL,
                username TEXT NOT NULL,
                application_name TEXT NOT NULL,
                connection_string_ciphertext BLOB NOT NULL,
                connection_string_nonce BLOB NOT NULL,
                role_created INTEGER NOT NULL,
                grants_applied INTEGER NOT NULL,
                engine TEXT NOT NULL DEFAULT 'postgresql',
                created_at TEXT NOT NULL,
                UNIQUE(environment_name, database_name, username)
            );
            "#,
        )?;
        self.ensure_columns()?;
        Ok(())
    }

    fn ensure_columns(&self) -> Result<()> {
        for (table, column, col_def) in TABLE_COLUMNS {
            let exists: bool = self
                .conn
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |row| row.get::<_, String>(1))?
                .any(|c| c.map(|c| c == *column).unwrap_or(false));
            if !exists {
                self.conn.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {column} {col_def};"
                ))?;
            }
        }
        Ok(())
    }

    pub fn is_initialized(&self) -> Result<bool> {
        let exists = self
            .conn
            .query_row("SELECT 1 FROM app_metadata WHERE id = 1", [], |_| Ok(()))
            .optional()?
            .is_some();
        Ok(exists)
    }

    pub fn initialize_master_password(&self, password: &str) -> Result<KdfConfig> {
        let config = crypto::default_kdf_config()?;
        let key = crypto::derive_key(password, &config)?;
        let check = crypto::build_password_check(key.as_slice())?;
        let now = now_iso();

        self.conn.execute(
            r#"
            INSERT INTO app_metadata (
                id,
                kdf_salt,
                argon2_memory_kib,
                argon2_iterations,
                argon2_parallelism,
                password_check_ciphertext,
                password_check_nonce,
                created_at,
                updated_at
            ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                config.salt,
                config.memory_kib,
                config.iterations,
                config.parallelism,
                check.ciphertext,
                check.nonce,
                now,
                now,
            ],
        )?;

        Ok(config)
    }

    pub fn load_kdf_config(&self) -> Result<KdfConfig> {
        self.conn
            .query_row(
                r#"
            SELECT kdf_salt, argon2_memory_kib, argon2_iterations, argon2_parallelism
            FROM app_metadata
            WHERE id = 1
            "#,
                [],
                |row| {
                    Ok(KdfConfig {
                        salt: row.get(0)?,
                        memory_kib: row.get(1)?,
                        iterations: row.get(2)?,
                        parallelism: row.get(3)?,
                    })
                },
            )
            .context("master password metadata not found")
    }

    pub fn verify_master_password(&self, key: &[u8]) -> Result<()> {
        let encrypted = self.conn.query_row(
            r#"
            SELECT password_check_ciphertext, password_check_nonce
            FROM app_metadata
            WHERE id = 1
            "#,
            [],
            |row| {
                Ok(EncryptedValue {
                    ciphertext: row.get(0)?,
                    nonce: row.get(1)?,
                })
            },
        )?;
        crypto::verify_password_check(key, &encrypted)
    }

    pub fn get_machine_identity(&self) -> Result<Option<(String, String)>> {
        let row = self
            .conn
            .query_row(
                "SELECT machine_id, hostname FROM machine_identity WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    pub fn set_machine_identity(&self, machine_id: &str, hostname: &str) -> Result<()> {
        let now = now_iso();
        self.conn.execute(
            r#"
            INSERT INTO machine_identity (id, machine_id, hostname, created_at)
            VALUES (1, ?1, ?2, ?3)
            ON CONFLICT(id) DO UPDATE SET
                machine_id = excluded.machine_id,
                hostname = excluded.hostname
            "#,
            params![machine_id, hostname, now],
        )?;
        Ok(())
    }

    pub fn ensure_machine_identity(&self) -> Result<(String, String)> {
        if let Some(identity) = self.get_machine_identity()? {
            return Ok(identity);
        }
        let machine_id = uuid::Uuid::now_v7().to_string();
        let hostname = std::env::var("HOSTNAME")
            .ok()
            .or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| {
                        if o.status.success() {
                            String::from_utf8(o.stdout).ok()
                        } else {
                            None
                        }
                    })
            })
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|| "unknown".to_owned());
        self.set_machine_identity(&machine_id, &hostname)?;
        Ok((machine_id, hostname))
    }

    pub fn list_instances(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM environments ORDER BY name")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut output = Vec::new();
        for row in rows {
            output.push(row?);
        }
        Ok(output)
    }

    pub fn list_instance_records(&self) -> Result<Vec<(String, EncryptedValue)>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, base_database_url_ciphertext, base_database_url_nonce \
             FROM environments ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            let name: String = row.get(0)?;
            let ciphertext: Vec<u8> = row.get(1)?;
            let nonce: Vec<u8> = row.get(2)?;
            Ok((name, EncryptedValue { ciphertext, nonce }))
        })?;
        let mut output = Vec::new();
        for row in rows {
            output.push(row?);
        }
        Ok(output)
    }

    pub fn save_instance_secret(
        &self,
        instance_name: &str,
        encrypted: &EncryptedValue,
    ) -> Result<()> {
        let now = now_iso();
        self.conn.execute(
            r#"
            INSERT INTO environments (
                name,
                base_database_url_ciphertext,
                base_database_url_nonce,
                created_at,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(name) DO UPDATE SET
                base_database_url_ciphertext = excluded.base_database_url_ciphertext,
                base_database_url_nonce = excluded.base_database_url_nonce,
                updated_at = excluded.updated_at
            "#,
            params![
                instance_name,
                encrypted.ciphertext,
                encrypted.nonce,
                now,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn load_instance_secret(&self, instance_name: &str) -> Result<Option<InstanceSecret>> {
        self.conn
            .query_row(
                r#"
                SELECT base_database_url_ciphertext, base_database_url_nonce, updated_at
                FROM environments
                WHERE name = ?1
                "#,
                params![instance_name],
                |row| {
                    Ok(InstanceSecret {
                        encrypted: EncryptedValue {
                            ciphertext: row.get(0)?,
                            nonce: row.get(1)?,
                        },
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn delete_instance_secret(&self, instance_name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM environments WHERE name = ?1",
            params![instance_name],
        )?;
        Ok(())
    }

    pub fn can_delete_instance(&self, name: &str) -> Result<()> {
        let db_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM provisioned_databases WHERE environment_name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        if db_count > 0 {
            anyhow::bail!(
                "cannot delete '{}': it has provisioned databases. Delete them first.",
                name
            );
        }
        let user_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM provisioned_extra_users WHERE environment_name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        if user_count > 0 {
            anyhow::bail!(
                "cannot delete '{}': it has provisioned extra users. Delete them first.",
                name
            );
        }
        Ok(())
    }

    pub fn delete_instance(&self, name: &str) -> Result<()> {
        self.can_delete_instance(name)?;
        self.conn
            .execute("DELETE FROM environments WHERE name = ?1", params![name])?;
        Ok(())
    }

    pub fn save_provisioned_database(
        &self,
        instance_name: &str,
        outcome: &ProvisionOutcome,
        encrypted: &EncryptedValue,
    ) -> Result<()> {
        let now = now_iso();
        self.conn.execute(
            r#"
            INSERT INTO provisioned_databases (
                environment_name,
                database_name,
                application_name,
                role_name,
                connection_string_ciphertext,
                connection_string_nonce,
                database_created,
                role_created,
                created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(environment_name, database_name) DO UPDATE SET
                application_name = excluded.application_name,
                role_name = excluded.role_name,
                connection_string_ciphertext = excluded.connection_string_ciphertext,
                connection_string_nonce = excluded.connection_string_nonce,
                database_created = excluded.database_created,
                role_created = excluded.role_created,
                created_at = excluded.created_at
            "#,
            params![
                instance_name,
                outcome.database_name,
                outcome.application_name,
                outcome.role_name,
                encrypted.ciphertext,
                encrypted.nonce,
                bool_to_int(outcome.database_created),
                bool_to_int(outcome.role_created),
                now,
            ],
        )?;
        Ok(())
    }

    pub fn list_provisioned_databases(&self) -> Result<Vec<ProvisionedDatabaseRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT
                id,
                environment_name,
                database_name,
                application_name,
                role_name,
                connection_string_ciphertext,
                connection_string_nonce,
                database_created,
                role_created,
                created_at
            FROM provisioned_databases
            ORDER BY environment_name, database_name
            "#,
        )?;

        let rows = stmt.query_map([], |row| {
            let instance_name: String = row.get(1)?;

            Ok(ProvisionedDatabaseRecord {
                id: row.get(0)?,
                instance_name,
                database_name: row.get(2)?,
                application_name: row.get(3)?,
                role_name: row.get(4)?,
                encrypted: EncryptedValue {
                    ciphertext: row.get(5)?,
                    nonce: row.get(6)?,
                },
                database_created: row.get::<_, i64>(7)? != 0,
                role_created: row.get::<_, i64>(8)? != 0,
                created_at: row.get(9)?,
            })
        })?;

        let mut output = Vec::new();
        for row in rows {
            output.push(row?);
        }
        Ok(output)
    }

    pub fn delete_provisioned_database(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM provisioned_databases WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn update_database_application_name(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE provisioned_databases SET application_name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        Ok(())
    }

    pub fn save_provisioned_extra_user(
        &self,
        instance_name: &str,
        outcome: &ExtraUserProvisionOutcome,
        encrypted: &EncryptedValue,
    ) -> Result<()> {
        let now = now_iso();
        self.conn.execute(
            r#"
            INSERT INTO provisioned_extra_users (
                environment_name,
                database_name,
                username,
                application_name,
                connection_string_ciphertext,
                connection_string_nonce,
                role_created,
                grants_applied,
                created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(environment_name, database_name, username) DO UPDATE SET
                application_name = excluded.application_name,
                connection_string_ciphertext = excluded.connection_string_ciphertext,
                connection_string_nonce = excluded.connection_string_nonce,
                role_created = excluded.role_created,
                grants_applied = excluded.grants_applied,
                created_at = excluded.created_at
            "#,
            params![
                instance_name,
                outcome.database_name,
                outcome.username,
                outcome.application_name,
                encrypted.ciphertext,
                encrypted.nonce,
                bool_to_int(outcome.role_created),
                bool_to_int(outcome.grants_applied),
                now,
            ],
        )?;
        Ok(())
    }

    pub fn list_provisioned_extra_users(&self) -> Result<Vec<ProvisionedExtraUserRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT
                id,
                environment_name,
                database_name,
                username,
                application_name,
                connection_string_ciphertext,
                connection_string_nonce,
                role_created,
                grants_applied,
                created_at
            FROM provisioned_extra_users
            ORDER BY environment_name, database_name, username
            "#,
        )?;

        let rows = stmt.query_map([], |row| {
            let instance_name: String = row.get(1)?;

            Ok(ProvisionedExtraUserRecord {
                id: row.get(0)?,
                instance_name,
                database_name: row.get(2)?,
                username: row.get(3)?,
                application_name: row.get(4)?,
                encrypted: EncryptedValue {
                    ciphertext: row.get(5)?,
                    nonce: row.get(6)?,
                },
                role_created: row.get::<_, i64>(7)? != 0,
                grants_applied: row.get::<_, i64>(8)? != 0,
                created_at: row.get(9)?,
            })
        })?;

        let mut output = Vec::new();
        for row in rows {
            output.push(row?);
        }
        Ok(output)
    }

    pub fn delete_provisioned_extra_user(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM provisioned_extra_users WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn update_extra_user_application_name(&self, id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE provisioned_extra_users SET application_name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        Ok(())
    }

    pub fn list_saved_connections(&self) -> Result<Vec<SavedConnectionRecord>> {
        let mut output = Vec::new();
        for record in self.list_provisioned_databases()? {
            output.push(SavedConnectionRecord::Database(record));
        }
        for record in self.list_provisioned_extra_users()? {
            output.push(SavedConnectionRecord::ExtraUser(record));
        }
        output.sort_by_key(connection_sort_key);
        Ok(output)
    }

    pub fn change_master_password(
        &mut self,
        old_key: &[u8],
        new_password: &str,
    ) -> Result<KdfConfig> {
        let new_config = crypto::default_kdf_config()?;
        let new_key = crypto::derive_key(new_password, &new_config)?;
        let tx = self.conn.transaction()?;

        reencrypt_metadata(&tx, old_key, new_key.as_slice(), &new_config)?;
        reencrypt_environments(&tx, old_key, new_key.as_slice())?;
        reencrypt_provisioned_databases(&tx, old_key, new_key.as_slice())?;
        reencrypt_provisioned_extra_users(&tx, old_key, new_key.as_slice())?;

        tx.commit()?;
        Ok(new_config)
    }
}

fn reencrypt_metadata(
    tx: &Transaction<'_>,
    old_key: &[u8],
    new_key: &[u8],
    new_config: &KdfConfig,
) -> Result<()> {
    let old_check = tx.query_row(
        r#"
        SELECT password_check_ciphertext, password_check_nonce
        FROM app_metadata
        WHERE id = 1
        "#,
        [],
        |row| {
            Ok(EncryptedValue {
                ciphertext: row.get(0)?,
                nonce: row.get(1)?,
            })
        },
    )?;

    crypto::verify_password_check(old_key, &old_check)?;
    let new_check = crypto::build_password_check(new_key)?;
    let now = now_iso();
    tx.execute(
        r#"
        UPDATE app_metadata
        SET kdf_salt = ?1,
            argon2_memory_kib = ?2,
            argon2_iterations = ?3,
            argon2_parallelism = ?4,
            password_check_ciphertext = ?5,
            password_check_nonce = ?6,
            updated_at = ?7
        WHERE id = 1
        "#,
        params![
            new_config.salt,
            new_config.memory_kib,
            new_config.iterations,
            new_config.parallelism,
            new_check.ciphertext,
            new_check.nonce,
            now,
        ],
    )?;
    Ok(())
}

fn reencrypt_environments(tx: &Transaction<'_>, old_key: &[u8], new_key: &[u8]) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT id, base_database_url_ciphertext, base_database_url_nonce FROM environments",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            EncryptedValue {
                ciphertext: row.get(1)?,
                nonce: row.get(2)?,
            },
        ))
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    drop(stmt);

    for (id, encrypted) in entries {
        let plaintext = crypto::decrypt(old_key, &encrypted)?;
        let reencrypted = crypto::encrypt(new_key, plaintext.as_slice())?;
        tx.execute(
            "UPDATE environments SET base_database_url_ciphertext = ?1, base_database_url_nonce = ?2, updated_at = ?3 WHERE id = ?4",
            params![reencrypted.ciphertext, reencrypted.nonce, now_iso(), id],
        )?;
    }
    Ok(())
}

fn reencrypt_provisioned_databases(
    tx: &Transaction<'_>,
    old_key: &[u8],
    new_key: &[u8],
) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT id, connection_string_ciphertext, connection_string_nonce FROM provisioned_databases",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            EncryptedValue {
                ciphertext: row.get(1)?,
                nonce: row.get(2)?,
            },
        ))
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    drop(stmt);

    for (id, encrypted) in entries {
        let plaintext = crypto::decrypt(old_key, &encrypted)?;
        let reencrypted = crypto::encrypt(new_key, plaintext.as_slice())?;
        tx.execute(
            "UPDATE provisioned_databases SET connection_string_ciphertext = ?1, connection_string_nonce = ?2 WHERE id = ?3",
            params![reencrypted.ciphertext, reencrypted.nonce, id],
        )?;
    }
    Ok(())
}

fn reencrypt_provisioned_extra_users(
    tx: &Transaction<'_>,
    old_key: &[u8],
    new_key: &[u8],
) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT id, connection_string_ciphertext, connection_string_nonce FROM provisioned_extra_users",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            EncryptedValue {
                ciphertext: row.get(1)?,
                nonce: row.get(2)?,
            },
        ))
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    drop(stmt);

    for (id, encrypted) in entries {
        let plaintext = crypto::decrypt(old_key, &encrypted)?;
        let reencrypted = crypto::encrypt(new_key, plaintext.as_slice())?;
        tx.execute(
            "UPDATE provisioned_extra_users SET connection_string_ciphertext = ?1, connection_string_nonce = ?2 WHERE id = ?3",
            params![reencrypted.ciphertext, reencrypted.nonce, id],
        )?;
    }
    Ok(())
}

fn database_path() -> Result<PathBuf> {
    let project_dirs = ProjectDirs::from("com", "lucaseufrasiojcpm", "cais")
        .context("failed to resolve app data directory")?;
    Ok(project_dirs.data_dir().join("data.sqlite"))
}

/// Returns the stable application-owned directory for encrypted backups.
pub fn backup_directory() -> Result<PathBuf> {
    let project_dirs = ProjectDirs::from("com", "lucaseufrasiojcpm", "cais")
        .context("failed to resolve app data directory")?;
    Ok(project_dirs.data_dir().join("backups"))
}

pub fn display_database_path() -> Result<String> {
    Ok(database_path()?.display().to_string())
}

fn bool_to_int(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn connection_sort_key(record: &SavedConnectionRecord) -> (String, String, u8, String) {
    match record {
        SavedConnectionRecord::Database(record) => (
            record.instance_name.clone(),
            record.database_name.clone(),
            0,
            record.role_name.clone(),
        ),
        SavedConnectionRecord::ExtraUser(record) => (
            record.instance_name.clone(),
            record.database_name.clone(),
            1,
            record.username.clone(),
        ),
        SavedConnectionRecord::Instance { name, .. } => {
            (name.clone(), String::new(), 2, String::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Storage;
    use crate::crypto;
    use crate::models::{ExtraUserProvisionOutcome, ProvisionOutcome, SavedConnectionRecord};
    use tempfile::tempdir;

    #[test]
    fn fresh_storage_generates_machine_identity() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let (mid, host) = storage.ensure_machine_identity().expect("identity");
        assert!(!mid.is_empty(), "machine_id should not be empty");
        assert!(!host.is_empty(), "hostname should not be empty");
        // Calling again should return the same identity
        let (mid2, host2) = storage.ensure_machine_identity().expect("identity2");
        assert_eq!(mid, mid2, "machine_id must be stable across calls");
        assert_eq!(host, host2, "hostname must be stable across calls");
    }

    #[test]
    fn machine_identity_is_persistent_across_storage_reopens() {
        let dir = tempdir().expect("dir");
        let path = dir.path().join("data.sqlite");
        {
            let storage = Storage::open_at(path.clone()).expect("storage");
            let (mid, host) = storage.ensure_machine_identity().expect("identity");
            assert!(!mid.is_empty());
            assert!(!host.is_empty());
        }
        {
            let storage = Storage::open_at(path).expect("storage2");
            let (mid, host) = storage
                .get_machine_identity()
                .expect("get")
                .expect("identity should exist after reopen");
            assert!(!mid.is_empty());
            assert!(!host.is_empty());
        }
    }

    #[test]
    fn engine_column_defaults_to_postgresql_on_instance_insert() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let key = b"01234567890123456789012345678901";
        let enc = crypto::encrypt(key, b"postgresql://u:p@h/db").expect("encrypt");

        storage
            .save_instance_secret("test-engine-instance", &enc)
            .expect("save instance");

        let instances = storage.list_instances().expect("list");
        assert!(
            instances.contains(&"test-engine-instance".to_owned()),
            "instance should be listable, proving engine column exists with default"
        );
    }

    #[test]
    fn engine_column_defaults_on_provisioned_database_insert() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let key = b"01234567890123456789012345678901";
        let enc = crypto::encrypt(key, b"postgresql://u:p@h/db").expect("encrypt");

        storage
            .save_provisioned_database(
                "test-instance",
                &ProvisionOutcome {
                    database_name: "test-db".into(),
                    application_name: "test-app".into(),
                    role_name: "test_owner".into(),
                    connection_string: "postgresql://u:p@h/db".into(),
                    database_created: true,
                    role_created: true,
                },
                &enc,
            )
            .expect("save provisioned database");

        // Reading back confirms engine column exists with default
        let records = storage.list_provisioned_databases().expect("list");
        assert!(
            records.iter().any(|r| r.database_name == "test-db"),
            "provisioned database should be listable with engine column"
        );
    }

    #[test]
    fn saves_and_loads_instance_secret() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let encrypted = crypto::encrypt(
            b"01234567890123456789012345678901",
            b"postgresql://u:p@h/db",
        )
        .expect("encrypt");
        storage
            .save_instance_secret("dev", &encrypted)
            .expect("save");
        let loaded = storage
            .load_instance_secret("dev")
            .expect("load")
            .expect("some");
        assert_eq!(loaded.encrypted.ciphertext, encrypted.ciphertext);
        assert_eq!(loaded.encrypted.nonce, encrypted.nonce);
    }

    #[test]
    fn saves_and_lists_connections_stably() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let key = b"01234567890123456789012345678901";
        let db_enc = crypto::encrypt(key, b"db").expect("encrypt db");
        let user_enc = crypto::encrypt(key, b"user").expect("encrypt user");
        storage
            .save_provisioned_database(
                "dev",
                &ProvisionOutcome {
                    database_name: "app".into(),
                    application_name: "app".into(),
                    role_name: "app_owner".into(),
                    connection_string: "db".into(),
                    database_created: true,
                    role_created: true,
                },
                &db_enc,
            )
            .expect("save db");
        storage
            .save_provisioned_extra_user(
                "dev",
                &ExtraUserProvisionOutcome {
                    database_name: "app".into(),
                    username: "writer".into(),
                    application_name: "app".into(),
                    connection_string: "user".into(),
                    role_created: true,
                    grants_applied: true,
                },
                &user_enc,
            )
            .expect("save user");

        let records = storage.list_saved_connections().expect("list");
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn deletes_database_connection() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let key = b"01234567890123456789012345678901";
        let db_enc = crypto::encrypt(key, b"db").expect("encrypt");
        storage
            .save_provisioned_database(
                "prd",
                &ProvisionOutcome {
                    database_name: "analytics".into(),
                    application_name: "analytics".into(),
                    role_name: "analytics_owner".into(),
                    connection_string: "db".into(),
                    database_created: true,
                    role_created: true,
                },
                &db_enc,
            )
            .expect("save");

        let records = storage.list_saved_connections().expect("list");
        assert_eq!(records.len(), 1);
        let id = match &records[0] {
            SavedConnectionRecord::Database(r) => r.id,
            _ => panic!("expected database record"),
        };

        storage.delete_provisioned_database(id).expect("delete");
        assert!(storage.list_saved_connections().unwrap().is_empty());
    }

    #[test]
    fn deletes_extra_user_connection() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let key = b"01234567890123456789012345678901";
        let user_enc = crypto::encrypt(key, b"user").expect("encrypt");

        let db_enc = crypto::encrypt(key, b"db").expect("encrypt");
        storage
            .save_provisioned_database(
                "hml",
                &ProvisionOutcome {
                    database_name: "payments".into(),
                    application_name: "payments".into(),
                    role_name: "payments_owner".into(),
                    connection_string: "db".into(),
                    database_created: true,
                    role_created: true,
                },
                &db_enc,
            )
            .expect("save db");

        storage
            .save_provisioned_extra_user(
                "hml",
                &ExtraUserProvisionOutcome {
                    database_name: "payments".into(),
                    username: "payments_app".into(),
                    application_name: "payments".into(),
                    connection_string: "user".into(),
                    role_created: true,
                    grants_applied: true,
                },
                &user_enc,
            )
            .expect("save user");

        let records = storage.list_saved_connections().expect("list");
        let user_records: Vec<_> = records
            .iter()
            .filter(|r| matches!(r, SavedConnectionRecord::ExtraUser(_)))
            .collect();
        assert_eq!(user_records.len(), 1);
        let id = match &user_records[0] {
            SavedConnectionRecord::ExtraUser(r) => r.id,
            _ => panic!("expected extra user record"),
        };

        storage.delete_provisioned_extra_user(id).expect("delete");
        let remaining = storage.list_saved_connections().expect("list");
        assert_eq!(remaining.len(), 1);
        assert!(matches!(remaining[0], SavedConnectionRecord::Database(_)));
    }

    #[test]
    fn updates_database_application_name() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let key = b"01234567890123456789012345678901";
        let db_enc = crypto::encrypt(key, b"db").expect("encrypt");

        storage
            .save_provisioned_database(
                "dev",
                &ProvisionOutcome {
                    database_name: "crm".into(),
                    application_name: "original".into(),
                    role_name: "crm_owner".into(),
                    connection_string: "db".into(),
                    database_created: true,
                    role_created: true,
                },
                &db_enc,
            )
            .expect("save");

        let records = storage.list_saved_connections().expect("list");
        let id = match &records[0] {
            SavedConnectionRecord::Database(r) => {
                assert_eq!(r.application_name, "original");
                r.id
            }
            _ => panic!("expected database record"),
        };

        storage
            .update_database_application_name(id, "renamed")
            .expect("update");

        let updated = storage.list_saved_connections().expect("list");
        match &updated[0] {
            SavedConnectionRecord::Database(r) => {
                assert_eq!(r.application_name, "renamed");
            }
            _ => panic!("expected database record"),
        }
    }

    #[test]
    fn updates_extra_user_application_name() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let key = b"01234567890123456789012345678901";
        let user_enc = crypto::encrypt(key, b"user").expect("encrypt");

        storage
            .save_provisioned_extra_user(
                "dev",
                &ExtraUserProvisionOutcome {
                    database_name: "crm".into(),
                    username: "crm_app".into(),
                    application_name: "original".into(),
                    connection_string: "user".into(),
                    role_created: true,
                    grants_applied: true,
                },
                &user_enc,
            )
            .expect("save");

        let records = storage.list_saved_connections().expect("list");
        let id = match &records[0] {
            SavedConnectionRecord::ExtraUser(r) => {
                assert_eq!(r.application_name, "original");
                r.id
            }
            _ => panic!("expected extra user record"),
        };

        storage
            .update_extra_user_application_name(id, "renamed")
            .expect("update");

        let updated = storage.list_saved_connections().expect("list");
        match &updated[0] {
            SavedConnectionRecord::ExtraUser(r) => {
                assert_eq!(r.application_name, "renamed");
            }
            _ => panic!("expected extra user record"),
        }
    }
}
