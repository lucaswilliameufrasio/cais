use std::process::Command;
use std::time::{Duration, Instant};

use db_provisioner_tui::models::{
    BackupConfig, ConflictPolicy, ExtraUserProvisionRequest, PgToolBackend, ProvisionRequest,
};
use db_provisioner_tui::postgres::{
    InstanceBackupContext, backup_instance_with_progress, check_pg_tools,
    migrate_database_with_progress, provision_database_with_progress,
    provision_extra_user_with_progress, restore_instance_with_progress,
};
use postgres::{Client, NoTls};

struct DockerPostgres {
    name: String,
    port: u16,
}

impl DockerPostgres {
    fn start() -> Self {
        let name = format!("db-provisioner-tui-{}", uuid::Uuid::new_v4());
        let port = 55432 + (uuid::Uuid::new_v4().as_u128() % 1000) as u16;

        let run_status = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &name,
                "-e",
                "POSTGRES_PASSWORD=postgres",
                "-e",
                "POSTGRES_USER=postgres",
                "-e",
                "POSTGRES_DB=postgres",
                "-p",
                &format!("{port}:5432"),
                "postgres:17",
            ])
            .status()
            .expect("docker run");
        assert!(run_status.success(), "docker container failed to start");

        let started = DockerPostgres { name, port };
        started.wait_ready();
        started
    }

    fn url(&self) -> String {
        format!(
            "postgresql://postgres:postgres@127.0.0.1:{}/postgres",
            self.port
        )
    }

    fn db_url(&self, db: &str) -> String {
        format!(
            "postgresql://postgres:postgres@127.0.0.1:{}/{}",
            self.port, db
        )
    }

    fn wait_ready(&self) {
        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(60) {
                panic!("postgres container not ready");
            }
            if Client::connect(&self.url(), NoTls).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

impl Drop for DockerPostgres {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .status();
    }
}

#[test]
fn provision_database_creates_database_and_owner() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let request = ProvisionRequest {
        database_name: "billing_core".into(),
        application_name: "billing_core".into(),
    };
    let outcome = provision_database_with_progress(&pg.url(), &request, |_| {}).expect("provision");
    assert_eq!(outcome.database_name, "billing_core");

    let mut client = Client::connect(&pg.url(), NoTls).expect("connect");
    let db_exists = client
        .query_one(
            "SELECT 1 FROM pg_database WHERE datname = 'billing_core'",
            &[],
        )
        .expect("db exists");
    assert_eq!(db_exists.get::<_, i32>(0), 1);
    let role_exists = client
        .query_one(
            "SELECT 1 FROM pg_roles WHERE rolname = 'billing_core_owner'",
            &[],
        )
        .expect("role exists");
    assert_eq!(role_exists.get::<_, i32>(0), 1);
}

#[test]
fn provision_extra_user_applies_limited_grants() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let db_request = ProvisionRequest {
        database_name: "orders".into(),
        application_name: "orders".into(),
    };
    provision_database_with_progress(&pg.url(), &db_request, |_| {}).expect("provision db");

    let user_request = ExtraUserProvisionRequest {
        database_name: "orders".into(),
        username: "orders_app".into(),
        application_name: "orders".into(),
    };
    provision_extra_user_with_progress(&pg.url(), &user_request, |_| {}).expect("provision user");

    let mut client = Client::connect(&pg.url(), NoTls).expect("connect");
    let role_exists = client
        .query_one("SELECT 1 FROM pg_roles WHERE rolname = 'orders_app'", &[])
        .expect("role exists");
    assert_eq!(role_exists.get::<_, i32>(0), 1);

    let membership = client
        .query_opt(
            r#"
            SELECT 1
            FROM pg_auth_members m
            JOIN pg_roles parent ON parent.oid = m.roleid
            JOIN pg_roles child ON child.oid = m.member
            WHERE parent.rolname = 'orders_owner' AND child.rolname = 'orders_app'
            "#,
            &[],
        )
        .expect("membership query");
    assert!(membership.is_none());
}

#[test]
fn provision_extra_user_fails_when_database_missing() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let user_request = ExtraUserProvisionRequest {
        database_name: "missing".into(),
        username: "app_user".into(),
        application_name: "missing".into(),
    };
    let err = provision_extra_user_with_progress(&pg.url(), &user_request, |_| {})
        .expect_err("expected failure");
    assert!(err.to_string().contains("does not exist"));
}

#[test]
fn provision_database_is_idempotent() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let request = ProvisionRequest {
        database_name: "inventory".into(),
        application_name: "inventory".into(),
    };
    let first = provision_database_with_progress(&pg.url(), &request, |_| {}).expect("first");
    assert!(first.database_created);
    assert!(first.role_created);

    let second = provision_database_with_progress(&pg.url(), &request, |_| {}).expect("second");
    assert!(!second.database_created);
    assert!(!second.role_created);
}

#[test]
fn provision_extra_user_is_idempotent() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let db_request = ProvisionRequest {
        database_name: "analytics".into(),
        application_name: "analytics".into(),
    };
    provision_database_with_progress(&pg.url(), &db_request, |_| {}).expect("provision db");

    let user_request = ExtraUserProvisionRequest {
        database_name: "analytics".into(),
        username: "analytics_app".into(),
        application_name: "analytics".into(),
    };

    let first = provision_extra_user_with_progress(&pg.url(), &user_request, |_| {})
        .expect("first extra user");
    assert!(first.role_created);
    assert!(first.grants_applied);

    let second = provision_extra_user_with_progress(&pg.url(), &user_request, |_| {})
        .expect("second extra user");
    assert!(!second.role_created);
}

#[test]
fn extra_user_can_read_write_on_target_database() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let db_name = "billing_core";

    // Create the database and connect as superuser to add a table
    let db_request = ProvisionRequest {
        database_name: db_name.into(),
        application_name: db_name.into(),
    };
    let _owner_outcome =
        provision_database_with_progress(&pg.url(), &db_request, |_| {}).expect("provision db");

    // Connect as superuser to the target database and create a table with data
    let target_super_url = format!(
        "postgresql://postgres:postgres@127.0.0.1:{}/{}",
        pg.port, db_name
    );
    let mut super_client = Client::connect(&target_super_url, NoTls).expect("connect super");
    super_client
        .batch_execute("CREATE TABLE items (id SERIAL PRIMARY KEY, name TEXT)")
        .expect("create table");
    super_client
        .execute("INSERT INTO items (name) VALUES ($1)", &[&"widget"])
        .expect("insert row");
    drop(super_client);

    // Provision the extra user
    let extra_outcome = provision_extra_user_with_progress(
        &pg.url(),
        &ExtraUserProvisionRequest {
            database_name: db_name.into(),
            username: "billing_reader".into(),
            application_name: db_name.into(),
        },
        |_| {},
    )
    .expect("provision extra user");

    // Connect as the extra user to the target database and run DML
    let mut extra_client =
        Client::connect(&extra_outcome.connection_string, NoTls).expect("connect extra user");

    // SELECT
    let rows = extra_client
        .query("SELECT * FROM items ORDER BY id", &[])
        .expect("select");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(1), "widget");

    // INSERT
    extra_client
        .execute("INSERT INTO items (name) VALUES ($1)", &[&"gadget"])
        .expect("insert");
    let count: i64 = extra_client
        .query_one("SELECT COUNT(*) FROM items", &[])
        .expect("count")
        .get(0);
    assert_eq!(count, 2);

    // UPDATE
    extra_client
        .execute(
            "UPDATE items SET name = $1 WHERE name = $2",
            &[&"gadget_v2", &"gadget"],
        )
        .expect("update");

    // DELETE
    extra_client
        .execute("DELETE FROM items WHERE name = $1", &[&"widget"])
        .expect("delete");
    let count: i64 = extra_client
        .query_one("SELECT COUNT(*) FROM items", &[])
        .expect("count")
        .get(0);
    assert_eq!(count, 1);
}

#[test]
fn owner_default_privileges_propagate_to_extra_user() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let db_name = "saas";

    // Provision database + extra user
    let owner_outcome = provision_database_with_progress(
        &pg.url(),
        &ProvisionRequest {
            database_name: db_name.into(),
            application_name: db_name.into(),
        },
        |_| {},
    )
    .expect("provision db");

    let extra_outcome = provision_extra_user_with_progress(
        &pg.url(),
        &ExtraUserProvisionRequest {
            database_name: db_name.into(),
            username: "saas_app".into(),
            application_name: db_name.into(),
        },
        |_| {},
    )
    .expect("provision extra user");

    // Connect as the owner to the target database and create a table
    let mut owner_client =
        Client::connect(&owner_outcome.connection_string, NoTls).expect("connect owner");
    owner_client
        .batch_execute("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT)")
        .expect("owner create table");
    owner_client
        .execute(
            "INSERT INTO settings (key, value) VALUES ($1, $2)",
            &[&"theme", &"dark"],
        )
        .expect("owner insert");
    drop(owner_client);

    // Connect as the extra user and verify they can read the table owner just created
    let mut extra_client =
        Client::connect(&extra_outcome.connection_string, NoTls).expect("connect extra user");
    let rows = extra_client
        .query("SELECT * FROM settings ORDER BY key", &[])
        .expect("extra user select");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(1), "dark");
}

#[test]
fn check_pg_tools_returns_native_or_docker() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let backend = check_pg_tools();
    match backend {
        PgToolBackend::Native { .. } | PgToolBackend::Docker { .. } => {} // ok
        PgToolBackend::NotFound => {
            panic!("pg_dump/pg_restore should be available natively or via Docker");
        }
    }
}

#[test]
fn migrate_database_via_docker() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    match check_pg_tools() {
        PgToolBackend::Docker { .. } => {} // proceed
        PgToolBackend::Native { .. } => {
            // docker is still available to spin up postgres containers,
            // but pg_dump/pg_restore are native. We force Docker backend.
        }
        PgToolBackend::NotFound => return, // no tools at all
    }

    let source = DockerPostgres::start();
    let dest = DockerPostgres::start();

    let db_name = "migration_source";

    // Create database and data on source
    {
        let mut client = Client::connect(&source.url(), NoTls).expect("connect source");
        client
            .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .expect("create source db");
    }
    {
        let source_db_url = source.db_url(db_name);
        let mut client = Client::connect(&source_db_url, NoTls).expect("connect source db");
        client
            .batch_execute("CREATE TABLE items (id SERIAL PRIMARY KEY, name TEXT)")
            .expect("create table");
        client
            .execute("INSERT INTO items (name) VALUES ($1)", &[&"widget"])
            .expect("insert");
    }

    let source_cs = source.db_url(db_name);

    // detect the source version so we use a matching pg_dump image
    let source_version = db_provisioner_tui::postgres::detect_source_version(&source_cs)
        .expect("detect source version");
    let major = db_provisioner_tui::postgres::extract_pg_major_version(&source_version);
    let docker_image = format!("postgres:{major}-alpine");
    let dest_db_name = "migrated_via_docker";

    migrate_database_with_progress(
        &source_cs,
        &dest.url(),
        dest_db_name,
        &PgToolBackend::Docker {
            image: docker_image,
        },
        &mut |_| {},
    )
    .expect("migration via Docker should succeed");

    // Verify data on destination
    let dest_db_url = dest.db_url(dest_db_name);
    let mut client = Client::connect(&dest_db_url, NoTls).expect("connect dest db");
    let rows = client
        .query("SELECT name FROM items ORDER BY id", &[])
        .expect("query dest");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "widget");
}

#[test]
fn migrate_database_via_native() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let backend = check_pg_tools();
    let pg_tool_backend = match &backend {
        PgToolBackend::Native { .. } => backend.clone(),
        _ => return, // native tools not available, skip
    };

    let source = DockerPostgres::start();
    let dest = DockerPostgres::start();

    let db_name = "native_source";

    // Create database and data on source
    {
        let mut client = Client::connect(&source.url(), NoTls).expect("connect source");
        client
            .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .expect("create source db");
    }
    {
        let source_db_url = source.db_url(db_name);
        let mut client = Client::connect(&source_db_url, NoTls).expect("connect source db");
        client
            .batch_execute("CREATE TABLE users (id SERIAL PRIMARY KEY, email TEXT)")
            .expect("create table");
        client
            .execute(
                "INSERT INTO users (email) VALUES ($1)",
                &[&"test@example.com"],
            )
            .expect("insert");
    }

    let source_cs = source.db_url(db_name);
    let dest_db_name = "migrated_via_native";

    migrate_database_with_progress(
        &source_cs,
        &dest.url(),
        dest_db_name,
        &pg_tool_backend,
        &mut |_| {},
    )
    .expect("migration via native should succeed");

    // Verify data on destination
    let dest_db_url = dest.db_url(dest_db_name);
    let mut client = Client::connect(&dest_db_url, NoTls).expect("connect dest db");
    let rows = client
        .query("SELECT email FROM users ORDER BY id", &[])
        .expect("query dest");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "test@example.com");
}

#[test]
fn migrate_database_dest_already_exists() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    match check_pg_tools() {
        PgToolBackend::Docker { .. } | PgToolBackend::Native { .. } => {} // proceed
        PgToolBackend::NotFound => return,
    }

    let source = DockerPostgres::start();
    let dest = DockerPostgres::start();

    let db_name = "source_data";

    // Create database on source with some data
    {
        let mut client = Client::connect(&source.url(), NoTls).expect("connect source");
        client
            .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .expect("create source db");
    }

    let source_cs = source.db_url(db_name);
    let dest_db_name = "already_there";

    // Pre-create the database on destination
    {
        let mut client = Client::connect(&dest.url(), NoTls).expect("connect dest");
        client
            .batch_execute(&format!("CREATE DATABASE \"{dest_db_name}\""))
            .expect("create dest db upfront");
    }

    let source_version = db_provisioner_tui::postgres::detect_source_version(&source_cs)
        .expect("detect source version");
    let major = db_provisioner_tui::postgres::extract_pg_major_version(&source_version);
    let docker_image = format!("postgres:{major}-alpine");

    let backend = match check_pg_tools() {
        PgToolBackend::Native { .. } => PgToolBackend::Native {
            dump_ver: String::new(),
            restore_ver: String::new(),
        },
        PgToolBackend::Docker { image: _ } => PgToolBackend::Docker {
            image: docker_image,
        },
        PgToolBackend::NotFound => unreachable!(),
    };

    let err = migrate_database_with_progress(
        &source_cs,
        &dest.url(),
        dest_db_name,
        &backend,
        &mut |_| {},
    )
    .expect_err("should fail when dest database already exists");

    assert!(
        err.to_string().contains("already exists"),
        "expected 'already exists' error, got: {err}"
    );
}

#[test]
fn backup_and_restore_instance_via_docker() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let source = DockerPostgres::start();
    let dest = DockerPostgres::start();

    let db_names = ["analytics", "metrics"];

    // Create databases and populate with data on source
    for db_name in &db_names {
        {
            let mut client = Client::connect(&source.url(), NoTls).expect("connect source");
            client
                .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
                .expect("create source db");
        }
        let db_url = source.db_url(db_name);
        let mut client = Client::connect(&db_url, NoTls).expect("connect source db");
        client
            .batch_execute("CREATE TABLE items (id SERIAL PRIMARY KEY, value TEXT)")
            .expect("create table");
        client
            .execute(
                "INSERT INTO items (value) VALUES ($1)",
                &[&format!("data_{db_name}")],
            )
            .expect("insert");
    }

    // Back up the entire instance
    let key = b"01234567890123456789012345678901";
    let dir = tempfile::tempdir().expect("tempdir");
    let source_version =
        db_provisioner_tui::postgres::detect_source_version(&source.url()).unwrap_or_default();
    let major = db_provisioner_tui::postgres::extract_pg_major_version(&source_version);
    let backend = PgToolBackend::Docker {
        image: format!("postgres:{major}-alpine"),
    };
    let outcome = backup_instance_with_progress(
        &source.url(),
        key,
        dir.path(),
        &backend,
        InstanceBackupContext {
            instance_name: "test-instance",
            machine_id: "test-machine",
            hostname: "test-host",
        },
        &db_names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        &BackupConfig::default(),
        &mut |_| {},
    )
    .expect("instance backup");
    assert_eq!(outcome.database_names.len(), 2);
    assert!(outcome.database_names.contains(&"analytics".to_owned()));
    assert!(outcome.database_names.contains(&"metrics".to_owned()));

    // Restore to destination instance
    let restored = restore_instance_with_progress(
        std::path::Path::new(&outcome.file_path),
        key,
        &dest.url(),
        &backend,
        ConflictPolicy::Skip,
        &mut |_| {},
    )
    .expect("instance restore");
    assert_eq!(restored.len(), 2);

    // Verify data in each restored database
    for db_name in &db_names {
        let db_url = dest.db_url(db_name);
        let mut client = Client::connect(&db_url, NoTls).expect("connect dest db");
        let rows: Vec<String> = client
            .query("SELECT value FROM items ORDER BY id", &[])
            .expect("query")
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(rows, vec![format!("data_{db_name}")]);
    }

    // Verify skip policy: restoring again skips existing databases
    let restored2 = restore_instance_with_progress(
        std::path::Path::new(&outcome.file_path),
        key,
        &dest.url(),
        &backend,
        ConflictPolicy::Skip,
        &mut |_| {},
    )
    .expect("second restore should not fail");
    assert!(
        restored2.is_empty(),
        "skip policy should yield no new restores"
    );
}
