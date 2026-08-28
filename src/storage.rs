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
        let data_dir = app_data_dir()?;
        migrate_legacy_vault_at(&data_dir)?;
        let workspaces = data_dir.join("workspaces");
        Self::open_at(workspace_file(&workspaces, "default")?)
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

    /// Removes an instance and every catalog record that belongs to it in a
    /// single transaction. Only catalog data is removed — nothing is dropped
    /// from the actual PostgreSQL server.
    pub fn delete_instance_cascade(&mut self, name: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM provisioned_extra_users WHERE environment_name = ?1",
            params![name],
        )?;
        tx.execute(
            "DELETE FROM provisioned_databases WHERE environment_name = ?1",
            params![name],
        )?;
        tx.execute("DELETE FROM environments WHERE name = ?1", params![name])?;
        tx.commit()?;
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

    pub fn update_database_connection(&self, id: i64, encrypted: &EncryptedValue) -> Result<()> {
        self.conn.execute(
            "UPDATE provisioned_databases SET connection_string_ciphertext = ?1, connection_string_nonce = ?2 WHERE id = ?3",
            params![encrypted.ciphertext, encrypted.nonce, id],
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

    pub fn update_extra_user_connection(&self, id: i64, encrypted: &EncryptedValue) -> Result<()> {
        self.conn.execute(
            "UPDATE provisioned_extra_users SET connection_string_ciphertext = ?1, connection_string_nonce = ?2 WHERE id = ?3",
            params![encrypted.ciphertext, encrypted.nonce, id],
        )?;
        Ok(())
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
    Ok(app_data_dir()?.join("data.sqlite"))
}

/// The application-owned data directory.
pub fn app_data_dir() -> Result<PathBuf> {
    let project_dirs = ProjectDirs::from("com", "lucaseufrasiojcpm", "cais")
        .context("failed to resolve app data directory")?;
    Ok(project_dirs.data_dir().to_path_buf())
}

/// Validates a workspace name: 1-32 chars, ASCII letters/digits plus `-` and
/// `_`, starting with a letter or digit. The name becomes the vault filename.
pub fn validate_workspace_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 32 {
        anyhow::bail!("workspace name must be 1-32 characters long");
    }
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    let all_ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !first_ok || !all_ok {
        anyhow::bail!(
            "workspace name may only contain letters, digits, '-' and '_', starting with a letter or digit"
        );
    }
    Ok(())
}

/// Vault file path of `workspace` inside `workspaces_dir`.
pub fn workspace_file(workspaces_dir: &std::path::Path, workspace: &str) -> Result<PathBuf> {
    validate_workspace_name(workspace)?;
    Ok(workspaces_dir.join(format!("{workspace}.sqlite")))
}

/// Lists workspace names (file stems) in `workspaces_dir`, sorted.
pub fn list_workspaces_at(workspaces_dir: &std::path::Path) -> Result<Vec<String>> {
    if !workspaces_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(workspaces_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "sqlite")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            names.push(stem.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// Deletes a workspace vault file. Returns whether it existed.
pub fn delete_workspace_at(workspaces_dir: &std::path::Path, workspace: &str) -> Result<bool> {
    let file = workspace_file(workspaces_dir, workspace)?;
    if !file.exists() {
        return Ok(false);
    }
    fs::remove_file(&file)
        .with_context(|| format!("failed to delete workspace file {}", file.display()))?;
    Ok(true)
}

/// Moves the legacy single-vault `data.sqlite` (and SQLite side files) into
/// the multi-workspace layout as `workspaces/default.sqlite`. Skips silently
/// when there is no legacy file or when the target already exists.
pub fn migrate_legacy_vault_at(data_dir: &std::path::Path) -> Result<()> {
    let legacy = data_dir.join("data.sqlite");
    if !legacy.exists() {
        return Ok(());
    }
    let workspaces = data_dir.join("workspaces");
    fs::create_dir_all(&workspaces)
        .with_context(|| format!("failed to create {}", workspaces.display()))?;

    // Lands on default.sqlite, falling back to default-2, default-3, ... when
    // the name is taken — the legacy vault always comes over, never dropped.
    let mut target = workspaces.join("default.sqlite");
    if target.exists() {
        let mut index = 2;
        loop {
            target = workspaces.join(format!("default-{index}.sqlite"));
            if !target.exists() {
                break;
            }
            index += 1;
        }
    }

    fs::rename(&legacy, &target).with_context(|| {
        format!(
            "failed to move {} into the workspaces layout",
            legacy.display()
        )
    })?;
    for side in ["-wal", "-shm"] {
        let side_file = data_dir.join(format!("data.sqlite{side}"));
        if side_file.exists() {
            let stem = target
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("default");
            let _ = fs::rename(&side_file, workspaces.join(format!("{stem}.sqlite{side}")));
        }
    }
    Ok(())
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

/// Result of a vault reset: every `(from, to)` pair that was moved aside.
pub struct VaultReset {
    pub moved: Vec<(PathBuf, PathBuf)>,
}

/// Moves the vault database (and any SQLite side files) plus the encrypted
/// backups directory aside under a timestamped `.old-` suffix, so the next
/// launch starts a fresh first-run setup. Paths that do not exist are
/// skipped. The moved files cannot be opened again without the old master
/// password — that is the point.
pub fn reset_vault_at(data_dir: &std::path::Path) -> Result<VaultReset> {
    let suffix = format!(".old-{}", chrono::Local::now().format("%Y%m%dT%H%M%S"));
    let mut moved = Vec::new();

    let database = data_dir.join("data.sqlite");
    if database.exists() {
        let target = data_dir.join(format!("data.sqlite{suffix}"));
        fs::rename(&database, &target)
            .with_context(|| format!("failed to move {}", database.display()))?;
        moved.push((database, target));
    }
    for side in ["-wal", "-shm"] {
        let side_file = data_dir.join(format!("data.sqlite{side}"));
        if side_file.exists() {
            let target = data_dir.join(format!("data.sqlite{side}{suffix}"));
            fs::rename(&side_file, &target)
                .with_context(|| format!("failed to move {}", side_file.display()))?;
            moved.push((side_file, target));
        }
    }
    let backups = data_dir.join("backups");
    if backups.is_dir() {
        let target = data_dir.join(format!("backups{suffix}"));
        fs::rename(&backups, &target)
            .with_context(|| format!("failed to move {}", backups.display()))?;
        moved.push((backups, target));
    }
    Ok(VaultReset { moved })
}

/// Resets the real application data directory.
pub fn reset_vault() -> Result<VaultReset> {
    reset_vault_at(&app_data_dir()?)
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
    use super::{
        Storage, delete_workspace_at, list_workspaces_at, migrate_legacy_vault_at, reset_vault_at,
        validate_workspace_name, workspace_file,
    };
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

    #[test]
    fn updates_database_connection_string() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let key = b"01234567890123456789012345678901";
        let db_enc = crypto::encrypt(key, b"old-cs").expect("encrypt");

        storage
            .save_provisioned_database(
                "dev",
                &ProvisionOutcome {
                    database_name: "crm".into(),
                    application_name: "crm".into(),
                    role_name: "crm_owner".into(),
                    connection_string: "old-cs".into(),
                    database_created: true,
                    role_created: true,
                },
                &db_enc,
            )
            .expect("save");

        let records = storage.list_provisioned_databases().expect("list");
        let id = records[0].id;

        let new_enc = crypto::encrypt(key, b"new-cs").expect("encrypt");
        storage
            .update_database_connection(id, &new_enc)
            .expect("update");

        let updated = storage.list_provisioned_databases().expect("list");
        let plaintext = crypto::decrypt(key, &updated[0].encrypted).expect("decrypt");
        assert_eq!(plaintext.as_slice(), b"new-cs");
    }

    #[test]
    fn updates_extra_user_connection_string() {
        let dir = tempdir().expect("dir");
        let storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let key = b"01234567890123456789012345678901";
        let user_enc = crypto::encrypt(key, b"old-cs").expect("encrypt");

        storage
            .save_provisioned_extra_user(
                "dev",
                &ExtraUserProvisionOutcome {
                    database_name: "crm".into(),
                    username: "crm_app".into(),
                    application_name: "crm".into(),
                    connection_string: "old-cs".into(),
                    role_created: true,
                    grants_applied: true,
                },
                &user_enc,
            )
            .expect("save");

        let records = storage.list_provisioned_extra_users().expect("list");
        let id = records[0].id;

        let new_enc = crypto::encrypt(key, b"new-cs").expect("encrypt");
        storage
            .update_extra_user_connection(id, &new_enc)
            .expect("update");

        let updated = storage.list_provisioned_extra_users().expect("list");
        let plaintext = crypto::decrypt(key, &updated[0].encrypted).expect("decrypt");
        assert_eq!(plaintext.as_slice(), b"new-cs");
    }

    #[test]
    fn delete_instance_cascade_removes_instance_and_catalog_entries() {
        let dir = tempdir().expect("dir");
        let mut storage = Storage::open_at(dir.path().join("data.sqlite")).expect("storage");
        let key = b"01234567890123456789012345678901";
        let inst_enc = crypto::encrypt(key, b"postgresql://u:p@h/postgres").expect("encrypt");
        storage
            .save_instance_secret("legacy", &inst_enc)
            .expect("save instance");

        let db_enc = crypto::encrypt(key, b"postgresql://u:p@h/orders").expect("encrypt");
        storage
            .save_provisioned_database(
                "legacy",
                &ProvisionOutcome {
                    database_name: "orders".into(),
                    application_name: "orders".into(),
                    role_name: "orders_owner".into(),
                    connection_string: "cs".into(),
                    database_created: true,
                    role_created: true,
                },
                &db_enc,
            )
            .expect("save db");

        let user_enc = crypto::encrypt(key, b"postgresql://u:p@h/orders").expect("encrypt");
        storage
            .save_provisioned_extra_user(
                "legacy",
                &ExtraUserProvisionOutcome {
                    database_name: "orders".into(),
                    username: "orders_worker".into(),
                    application_name: "orders".into(),
                    connection_string: "cs".into(),
                    role_created: true,
                    grants_applied: true,
                },
                &user_enc,
            )
            .expect("save user");

        storage
            .delete_instance_cascade("legacy")
            .expect("cascade delete");

        assert!(
            storage
                .load_instance_secret("legacy")
                .expect("load")
                .is_none(),
            "instance secret must be removed"
        );
        assert!(
            storage
                .list_provisioned_databases()
                .expect("list")
                .is_empty(),
            "provisioned databases must be removed"
        );
        assert!(
            storage
                .list_provisioned_extra_users()
                .expect("list")
                .is_empty(),
            "provisioned extra users must be removed"
        );

        // The name becomes free again for re-registration.
        storage
            .save_instance_secret("legacy", &inst_enc)
            .expect("re-save instance under the same name");
    }

    #[test]
    fn reset_vault_moves_database_and_backups_aside() {
        let dir = tempdir().expect("dir");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        let storage = Storage::open_at(data_dir.join("data.sqlite")).expect("storage");
        storage.initialize_master_password("hunter2").expect("init");
        drop(storage);

        let backups = data_dir.join("backups");
        std::fs::create_dir_all(&backups).expect("backups dir");
        std::fs::write(backups.join("dump.pgdump.enc"), b"cipher").expect("backup file");

        let reset = reset_vault_at(&data_dir).expect("reset");
        assert_eq!(reset.moved.len(), 2, "db and backups must be moved");

        assert!(
            !data_dir.join("data.sqlite").exists(),
            "vault database must be gone from the original path"
        );
        assert!(!backups.exists(), "backups dir must be gone");
        for (from, to) in &reset.moved {
            assert!(
                to.exists(),
                "{} must exist at its new location",
                to.display()
            );
            let _ = from;
        }

        let fresh = Storage::open_at(data_dir.join("data.sqlite")).expect("fresh storage");
        assert!(
            !fresh.is_initialized().expect("initialized check"),
            "next launch must start a fresh first-run setup"
        );
    }

    #[test]
    fn reset_vault_with_empty_directory_reports_nothing() {
        let dir = tempdir().expect("dir");
        let reset = reset_vault_at(dir.path()).expect("reset");
        assert!(reset.moved.is_empty(), "nothing exists to move");
    }

    #[test]
    fn workspace_name_validation() {
        assert!(validate_workspace_name("default").is_ok());
        assert!(validate_workspace_name("prd-2024").is_ok());
        assert!(validate_workspace_name("a_b").is_ok());
        assert!(validate_workspace_name("").is_err());
        assert!(validate_workspace_name(".hidden").is_err());
        assert!(validate_workspace_name("has space").is_err());
        assert!(validate_workspace_name("-leading").is_err());
        assert!(validate_workspace_name(&"x".repeat(33)).is_err());
    }

    #[test]
    fn workspaces_list_create_delete_round_trip() {
        let dir = tempdir().expect("dir");
        let workspaces = dir.path().join("workspaces");

        assert!(
            list_workspaces_at(&workspaces)
                .expect("list empty")
                .is_empty(),
            "missing directory lists as empty"
        );

        Storage::open_at(workspace_file(&workspaces, "default").expect("path")).expect("open");
        Storage::open_at(workspace_file(&workspaces, "dev").expect("path")).expect("open");

        let mut names = list_workspaces_at(&workspaces).expect("list");
        assert_eq!(names, vec!["default".to_owned(), "dev".to_owned()]);

        assert!(
            delete_workspace_at(&workspaces, "dev").expect("delete"),
            "existing workspace must report deleted"
        );
        assert!(
            !delete_workspace_at(&workspaces, "dev").expect("delete again"),
            "deleting a missing workspace reports not-existed"
        );
        names = list_workspaces_at(&workspaces).expect("list");
        assert_eq!(names, vec!["default".to_owned()]);
    }

    #[test]
    fn migrate_legacy_vault_moves_data_sqlite_to_default_workspace() {
        let dir = tempdir().expect("dir");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        let legacy = Storage::open_at(data_dir.join("data.sqlite")).expect("legacy vault");
        legacy.initialize_master_password("old-pass").expect("init");
        drop(legacy);

        migrate_legacy_vault_at(&data_dir).expect("migrate");

        let migrated = Storage::open_at(
            workspace_file(&data_dir.join("workspaces"), "default").expect("path"),
        )
        .expect("migrated vault");
        assert!(
            migrated.is_initialized().expect("initialized check"),
            "migrated vault keeps its master password"
        );
        assert!(
            !data_dir.join("data.sqlite").exists(),
            "legacy file must be moved, not copied"
        );

        // Running again is a no-op.
        migrate_legacy_vault_at(&data_dir).expect("migrate twice");
        let names = list_workspaces_at(&data_dir.join("workspaces")).expect("list");
        assert_eq!(names, vec!["default".to_owned()]);
    }

    #[test]
    fn migrate_legacy_vault_falls_back_when_default_is_taken() {
        let dir = tempdir().expect("dir");
        let data_dir = dir.path().join("data");
        let workspaces = data_dir.join("workspaces");
        std::fs::create_dir_all(&workspaces).expect("workspaces dir");
        let default = Storage::open_at(workspaces.join("default.sqlite")).expect("default");
        default
            .initialize_master_password("new-pass")
            .expect("init");
        drop(default);

        let legacy = Storage::open_at(data_dir.join("data.sqlite")).expect("legacy vault");
        legacy.initialize_master_password("old-pass").expect("init");
        drop(legacy);

        migrate_legacy_vault_at(&data_dir).expect("migrate");

        // Legacy file moves to default-2 instead of being left behind.
        assert!(!data_dir.join("data.sqlite").exists());
        let names = list_workspaces_at(&workspaces).expect("list");
        assert_eq!(names, vec!["default".to_owned(), "default-2".to_owned()]);

        let migrated =
            Storage::open_at(workspaces.join("default-2.sqlite")).expect("migrated vault");
        assert!(migrated.is_initialized().expect("check"));
    }
}
