use std::time::{Duration, Instant};

use cais::models::{
    BackupConfig, ConflictPolicy, ExtraUserProvisionRequest, PgToolBackend, ProvisionFullRequest,
    ProvisionRequest,
};
use cais::postgres::{
    InstanceBackupContext, backup_instance_with_progress, check_pg_tools, list_database_tables,
    migrate_database_with_progress, provision_database_with_progress,
    provision_extra_user_with_progress, provision_full_with_progress, resolve_docker_image,
    restore_instance_with_progress, run_sql_query, run_table_page,
};
use postgres::{Client, NoTls};
use testcontainers::{GenericImage, Image, ImageExt, core::WaitFor, runners::SyncRunner};

/// Ephemeral PostgreSQL server backed by its own testcontainers container.
/// Every test starts its own instance: unique name, Docker-assigned host port
/// and automatic removal when the container is dropped.
struct DockerPostgres<I: Image> {
    port: u16,
    _container: testcontainers::Container<I>,
}

impl DockerPostgres<GenericImage> {
    fn start() -> Self {
        Self::start_with_tag("postgres:17")
    }

    fn start_with_tag(tag: &str) -> Self {
        let (name, image_tag) = match tag.split_once(':') {
            Some((name, tag)) => (name, tag),
            None => (tag, "latest"),
        };
        let container = GenericImage::new(name, image_tag)
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "postgres")
            .with_env_var("POSTGRES_USER", "postgres")
            .with_env_var("POSTGRES_DB", "postgres")
            .start()
            .expect("start postgres container");
        let port = container
            .get_host_port_ipv4(5432)
            .expect("mapped host port for 5432");
        let started = Self {
            port,
            _container: container,
        };
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

    /// The wait-for message can fire during initdb (before the server
    /// restarts), so poll until the server actually accepts connections.
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

/// Pick a PostgreSQL Docker tag compatible with the native pg_dump
/// version (if running natively). Falls back to postgres:17 otherwise.
fn native_compatible_pg_tag() -> String {
    match check_pg_tools() {
        PgToolBackend::Native { dump_ver, .. } => {
            let major = cais::postgres::extract_pg_major_version(&dump_ver);
            if major > 0 {
                format!("postgres:{major}")
            } else {
                "postgres:17".into()
            }
        }
        _ => "postgres:17".into(),
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
fn provision_existing_role_rotates_password_so_connection_works() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let request = ProvisionRequest {
        database_name: "staging".into(),
        application_name: "staging".into(),
    };

    let first = provision_database_with_progress(&pg.url(), &request, |_| {}).expect("first");
    Client::connect(&first.connection_string, NoTls).expect("connect with first credentials");

    let second = provision_database_with_progress(&pg.url(), &request, |_| {}).expect("second");
    assert!(!second.database_created);
    assert!(!second.role_created);
    // The role pre-existed, so the password must have been rotated for the
    // returned connection string to be usable.
    Client::connect(&second.connection_string, NoTls).expect("connect with rotated credentials");
}

#[test]
fn provision_full_without_dedicated_owner_reuses_base_user() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let request = ProvisionFullRequest {
        database_name: "legacy_db".into(),
        application_name: "legacy_db".into(),
        extra_username: None,
        extra_application_name: None,
        dedicated_owner: false,
    };
    let outcome = provision_full_with_progress(&pg.url(), &request, |_| {}).expect("provision");

    assert!(outcome.database_created);
    assert!(!outcome.role_created);
    assert_eq!(outcome.role_name, "postgres");

    let mut client = Client::connect(&pg.url(), NoTls).expect("connect");
    let owner_role = client
        .query_opt(
            "SELECT 1 FROM pg_roles WHERE rolname = 'legacy_db_owner'",
            &[],
        )
        .expect("query roles");
    assert!(
        owner_role.is_none(),
        "no dedicated owner role should be created"
    );

    // The returned connection string reuses the base credentials and works.
    Client::connect(&outcome.database_connection_string, NoTls)
        .expect("connect with returned connection string");
}

#[test]
fn query_console_lists_tables_and_pages_data() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let cs = pg.url();
    let mut client = Client::connect(&cs, NoTls).expect("connect");
    client
        .batch_execute(
            "CREATE TABLE query_console_items (id bigint PRIMARY KEY, label text); \
             INSERT INTO query_console_items \
             SELECT g, 'item-' || g FROM generate_series(1, 250) g;",
        )
        .expect("seed table");
    drop(client);

    let tables = list_database_tables(&cs).expect("list tables");
    let item = tables
        .iter()
        .find(|t| t.schema == "public" && t.name == "query_console_items")
        .expect("browsed table listed");
    assert_eq!(item.kind, "table");
    assert!(
        tables
            .iter()
            .all(|t| t.schema != "pg_catalog" && t.schema != "information_schema"),
        "system schemas must be excluded"
    );

    let page1 = run_table_page(&cs, "public", "query_console_items", 0).expect("page 1");
    assert_eq!(page1.rows.len(), 200);
    assert!(!page1.truncated);
    assert_eq!(page1.columns, vec!["id".to_owned(), "label".to_owned()]);

    let page2 = run_table_page(&cs, "public", "query_console_items", 200).expect("page 2");
    assert_eq!(page2.rows.len(), 50);

    let missing = run_table_page(&cs, "public", "does_not_exist", 0);
    assert!(missing.is_err(), "unknown table must fail");
}

#[test]
fn query_console_read_only_blocks_writes_and_cap_applies() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let pg = DockerPostgres::start();
    let cs = pg.url();
    let mut client = Client::connect(&cs, NoTls).expect("connect");
    client
        .batch_execute(
            "CREATE TABLE query_cap_items (id bigint PRIMARY KEY); \
             INSERT INTO query_cap_items SELECT g FROM generate_series(1, 501) g;",
        )
        .expect("seed table");
    drop(client);

    let blocked = run_sql_query(&cs, "INSERT INTO query_cap_items VALUES (9999)", true);
    assert!(blocked.is_err(), "read-only session must reject INSERT");

    let select = run_sql_query(&cs, "SELECT * FROM query_cap_items", true).expect("select");
    assert_eq!(select.rows.len(), 500);
    assert!(select.truncated, "result must be flagged as truncated");

    let write = run_sql_query(&cs, "INSERT INTO query_cap_items VALUES (9999)", false)
        .expect("write allowed with read_only off");
    assert_eq!(write.rows.len(), 0);
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
    let source_version =
        cais::postgres::detect_source_version(&source_cs).expect("detect source version");
    let major = cais::postgres::extract_pg_major_version(&source_version);
    let docker_image = format!("postgres:{major}-alpine");
    let dest_db_name = "migrated_via_docker";

    migrate_database_with_progress(
        &source_cs,
        &dest.url(),
        dest_db_name,
        &PgToolBackend::Docker {
            image: docker_image,
        },
        false,
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

    let tag = native_compatible_pg_tag();
    let source = DockerPostgres::start_with_tag(&tag);
    let dest = DockerPostgres::start_with_tag(&tag);

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
        false,
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

    let backend = check_pg_tools();
    match &backend {
        PgToolBackend::Docker { .. } | PgToolBackend::Native { .. } => {} // proceed
        PgToolBackend::NotFound => return,
    }

    let tag = match &backend {
        PgToolBackend::Native { .. } => native_compatible_pg_tag(),
        _ => "postgres:17".into(),
    };
    let source = DockerPostgres::start_with_tag(&tag);
    let dest = DockerPostgres::start_with_tag(&tag);

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

    let source_version =
        cais::postgres::detect_source_version(&source_cs).expect("detect source version");
    let major = cais::postgres::extract_pg_major_version(&source_version);
    let docker_image = format!("postgres:{major}-alpine");

    let migrate_backend = match backend {
        PgToolBackend::Native { .. } => backend.clone(),
        PgToolBackend::Docker { .. } => PgToolBackend::Docker {
            image: docker_image,
        },
        PgToolBackend::NotFound => unreachable!(),
    };

    let err = migrate_database_with_progress(
        &source_cs,
        &dest.url(),
        dest_db_name,
        &migrate_backend,
        false,
        &mut |_| {},
    )
    .expect_err("should fail when dest database already exists");

    assert!(
        err.to_string().contains("already exists"),
        "expected 'already exists' error, got: {err}"
    );
}

#[test]
fn migrate_replace_existing_drops_target_database() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let source = DockerPostgres::start();
    let dest = DockerPostgres::start();

    let db_name = "replace_source";
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
            .batch_execute("CREATE TABLE items (id SERIAL PRIMARY KEY, marker TEXT)")
            .expect("create table");
        client
            .execute("INSERT INTO items (marker) VALUES ($1)", &[&"fresh_copy"])
            .expect("insert");
    }

    let dest_db_name = "premature_target";
    {
        let mut client = Client::connect(&dest.url(), NoTls).expect("connect dest");
        client
            .batch_execute(&format!("CREATE DATABASE \"{dest_db_name}\""))
            .expect("pre-create target db");
    }
    {
        let dest_db_url = dest.db_url(dest_db_name);
        let mut client = Client::connect(&dest_db_url, NoTls).expect("connect premature db");
        client
            .batch_execute("CREATE TABLE leftover_marker (id int)")
            .expect("create junk table in premature target");
    }

    let source_cs = source.db_url(db_name);
    let source_version =
        cais::postgres::detect_source_version(&source_cs).expect("detect source version");
    let major = cais::postgres::extract_pg_major_version(&source_version);
    let migrate_backend = PgToolBackend::Docker {
        image: format!("postgres:{major}-alpine"),
    };

    let outcome = migrate_database_with_progress(
        &source_cs,
        &dest.url(),
        dest_db_name,
        &migrate_backend,
        true,
        &mut |_| {},
    )
    .expect("migration with replace_existing should succeed");

    assert!(outcome.database_created, "database must be recreated");

    let dest_db_url = dest.db_url(dest_db_name);
    let mut client = Client::connect(&dest_db_url, NoTls).expect("connect replaced db");
    let rows = client
        .query("SELECT marker FROM items ORDER BY id", &[])
        .expect("query replaced db");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "fresh_copy");

    let leftover = client
        .query_opt(
            "SELECT 1 FROM information_schema.tables WHERE table_name = 'leftover_marker'",
            &[],
        )
        .expect("check junk table gone");
    assert!(leftover.is_none(), "junk table must be dropped by replace");
}

#[test]
fn migrate_database_17_to_18_via_docker() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let source = DockerPostgres::start_with_tag("postgres:17");
    let dest = DockerPostgres::start_with_tag("postgres:18");

    let db_name = "upgrade_test";
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
            .batch_execute("CREATE TABLE items (id SERIAL PRIMARY KEY, value TEXT)")
            .expect("create table");
        client
            .execute("INSERT INTO items (value) VALUES ($1)", &[&"17to18"])
            .expect("insert");
    }

    // Use PostgreSQL 18 tools to dump the 17 source and restore into 18 dest
    let image = "postgres:18-alpine";
    let source_cs = source.db_url(db_name);
    migrate_database_with_progress(
        &source_cs,
        &dest.url(),
        db_name,
        &PgToolBackend::Docker {
            image: image.to_owned(),
        },
        false,
        &mut |_| {},
    )
    .expect("17→18 migration via Docker");

    let dest_db_url = dest.db_url(db_name);
    let mut client = Client::connect(&dest_db_url, NoTls).expect("connect dest db");
    let rows: Vec<String> = client
        .query("SELECT value FROM items ORDER BY id", &[])
        .expect("query dest")
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(rows, vec!["17to18"]);
}

#[test]
fn backup_and_restore_instance_17_to_18() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let source = DockerPostgres::start_with_tag("postgres:17");
    let dest = DockerPostgres::start_with_tag("postgres:18");

    let db_names = ["orders", "catalog"];
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
            .batch_execute("CREATE TABLE data (id SERIAL PRIMARY KEY, name TEXT)")
            .expect("create table");
        client
            .execute("INSERT INTO data (name) VALUES ($1)", &[&db_name])
            .expect("insert");
    }

    let image = "postgres:18-alpine";
    let backend = PgToolBackend::Docker {
        image: image.to_owned(),
    };
    let key = b"01234567890123456789012345678901";
    let dir = tempfile::tempdir().expect("tempdir");

    let outcome = backup_instance_with_progress(
        &source.url(),
        key,
        dir.path(),
        &backend,
        InstanceBackupContext {
            instance_name: "instance17",
            machine_id: "m17",
            hostname: "h17",
        },
        &db_names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        &BackupConfig::default(),
        &mut |_| {},
    )
    .expect("instance backup 17→18");

    let restored = restore_instance_with_progress(
        std::path::Path::new(&outcome.file_path),
        key,
        &dest.url(),
        &backend,
        ConflictPolicy::Skip,
        &mut |_| {},
    )
    .expect("instance restore 17→18");
    assert_eq!(restored.len(), 2);

    for db_name in &db_names {
        let db_url = dest.db_url(db_name);
        let mut client = Client::connect(&db_url, NoTls).expect("connect dest db");
        let rows: Vec<String> = client
            .query("SELECT name FROM data ORDER BY id", &[])
            .expect("query")
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(rows, vec![db_name.to_string()]);
    }
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
    let source_version = cais::postgres::detect_source_version(&source.url()).unwrap_or_default();
    let major = cais::postgres::extract_pg_major_version(&source_version);
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
        restored2.iter().all(|o| !o.database_created),
        "skip policy should yield no new restores (all database_created should be false)"
    );
}

#[test]
fn restore_with_replace_replaces_existing_data() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let source = DockerPostgres::start();
    let dest = DockerPostgres::start();

    let db_name = "replace_me";

    // Create source database with data
    {
        let mut client = Client::connect(&source.url(), NoTls).expect("connect source");
        client
            .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .expect("create source db");
    }
    {
        let db_url = source.db_url(db_name);
        let mut client = Client::connect(&db_url, NoTls).expect("connect source db");
        client
            .batch_execute("CREATE TABLE items (id SERIAL PRIMARY KEY, value TEXT)")
            .expect("create table");
        client
            .execute("INSERT INTO items (value) VALUES ($1)", &[&"new_data"])
            .expect("insert");
    }

    // Create destination database with DIFFERENT data
    {
        let mut client = Client::connect(&dest.url(), NoTls).expect("connect dest");
        client
            .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .expect("create dest db");
    }
    {
        let db_url = dest.db_url(db_name);
        let mut client = Client::connect(&db_url, NoTls).expect("connect dest db");
        client
            .batch_execute("CREATE TABLE items (id SERIAL PRIMARY KEY, value TEXT)")
            .expect("create table");
        client
            .execute("INSERT INTO items (value) VALUES ($1)", &[&"old_data"])
            .expect("insert old data");
    }

    // Backup source instance
    let key = b"01234567890123456789012345678901";
    let dir = tempfile::tempdir().expect("tempdir");
    let source_version = cais::postgres::detect_source_version(&source.url()).unwrap_or_default();
    let major = cais::postgres::extract_pg_major_version(&source_version);
    let backend = PgToolBackend::Docker {
        image: format!("postgres:{major}-alpine"),
    };
    let outcome = backup_instance_with_progress(
        &source.url(),
        key,
        dir.path(),
        &backend,
        InstanceBackupContext {
            instance_name: "replace-test",
            machine_id: "replace-machine",
            hostname: "replace-host",
        },
        &[db_name.to_string()],
        &BackupConfig::default(),
        &mut |_| {},
    )
    .expect("instance backup for replace test");

    // Restore with Replace policy — should drop old database and recreate with new data
    let restored = restore_instance_with_progress(
        std::path::Path::new(&outcome.file_path),
        key,
        &dest.url(),
        &backend,
        ConflictPolicy::Replace,
        &mut |_| {},
    )
    .expect("replace restore should succeed");
    assert_eq!(restored.len(), 1, "one database should be restored");

    // Verify old data is gone and new data is present
    let dest_db_url = dest.db_url(db_name);
    let mut client = Client::connect(&dest_db_url, NoTls).expect("connect dest db after replace");
    let rows: Vec<String> = client
        .query("SELECT value FROM items ORDER BY id", &[])
        .expect("query after replace")
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        rows,
        vec!["new_data"],
        "old data should be replaced by new data"
    );
}

#[test]
fn restore_with_replace_works_17_to_18() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        return;
    }

    let source = DockerPostgres::start_with_tag("postgres:17");
    let dest = DockerPostgres::start_with_tag("postgres:18");

    let db_name = "upgrade_replace";

    // Create source db with data
    {
        let mut client = Client::connect(&source.url(), NoTls).expect("connect source");
        client
            .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .expect("create source db");
    }
    {
        let db_url = source.db_url(db_name);
        let mut client = Client::connect(&db_url, NoTls).expect("connect source db");
        client
            .batch_execute("CREATE TABLE data (id SERIAL PRIMARY KEY, value TEXT)")
            .expect("create table");
        client
            .execute("INSERT INTO data (value) VALUES ($1)", &[&"v2_data"])
            .expect("insert v2 data");
    }

    // Create dest db with old data
    {
        let mut client = Client::connect(&dest.url(), NoTls).expect("connect dest");
        client
            .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .expect("create dest db");
    }
    {
        let db_url = dest.db_url(db_name);
        let mut client = Client::connect(&db_url, NoTls).expect("connect dest db");
        client
            .batch_execute("CREATE TABLE data (id SERIAL PRIMARY KEY, value TEXT)")
            .expect("create table");
        client
            .execute("INSERT INTO data (value) VALUES ($1)", &[&"v1_data"])
            .expect("insert v1 data");
    }

    let key = b"01234567890123456789012345678901";
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = PgToolBackend::Docker {
        image: "postgres:18-alpine".to_owned(),
    };
    let outcome = backup_instance_with_progress(
        &source.url(),
        key,
        dir.path(),
        &backend,
        InstanceBackupContext {
            instance_name: "upgrade-test",
            machine_id: "um",
            hostname: "uh",
        },
        &[db_name.to_string()],
        &BackupConfig::default(),
        &mut |_| {},
    )
    .expect("backup for 17→18 replace test");

    let restored = restore_instance_with_progress(
        std::path::Path::new(&outcome.file_path),
        key,
        &dest.url(),
        &backend,
        ConflictPolicy::Replace,
        &mut |_| {},
    )
    .expect("replace restore should succeed 17→18");
    assert_eq!(restored.len(), 1);

    let dest_db_url = dest.db_url(db_name);
    let mut client = Client::connect(&dest_db_url, NoTls).expect("connect after replace");
    let rows: Vec<String> = client
        .query("SELECT value FROM data ORDER BY id", &[])
        .expect("query")
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        rows,
        vec!["v2_data"],
        "data should be replaced during 17→18 restore"
    );
}

#[test]
fn resolve_docker_image_detects_timescaledb() {
    if std::env::var("RUN_DOCKER_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipping; set RUN_DOCKER_TESTS=1 to run docker integration tests");
        return;
    }

    // A plain PostgreSQL instance must resolve to the plain alpine image.
    let plain = DockerPostgres::start_with_tag("postgres:17");
    let backend = PgToolBackend::Docker {
        image: "postgres:18-alpine".to_owned(),
    };
    let plain_image = resolve_docker_image(&backend, Some(&plain.url()));
    assert_eq!(plain_image, "postgres:17-alpine");

    // A TimescaleDB instance must resolve to the timescale image, otherwise a
    // restore that creates the timescaledb extension would fail on a plain image.
    let ts = DockerPostgres::start_with_tag("timescale/timescaledb:2.28.2-pg18");
    let ts_image = resolve_docker_image(&backend, Some(&ts.url()));
    assert_eq!(ts_image, "timescale/timescaledb:latest-pg18");
}
