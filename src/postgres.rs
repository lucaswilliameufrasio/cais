use anyhow::{Context, Result};
use postgres::{Client, NoTls, SimpleQueryMessage};
use url::{Host, Url, form_urlencoded::byte_serialize};

use crate::crypto;
use crate::models::{
    ActiveQuery, BackupConfig, BackupMetadata, BackupOutcome, ConflictPolicy, DatabaseTableInfo,
    DiscoveredDatabase, ExtraUserProvisionOutcome, ExtraUserProvisionRequest, ParsedDatabaseUrl,
    PgToolBackend, ProvisionFullOutcome, ProvisionFullRequest, ProvisionOutcome, ProvisionRequest,
    SqlQueryResult, TablespaceMode,
};
use crate::validation::{normalize_application_name, validate_database_name};

pub fn parse_database_url(raw: &str) -> Result<ParsedDatabaseUrl> {
    let url = Url::parse(raw).context("base DATABASE_URL is not a valid URI")?;
    let scheme = url.scheme();
    if scheme != "postgresql" && scheme != "postgres" {
        anyhow::bail!("DATABASE_URL must use the postgres or postgresql scheme")
    }

    let host = match url.host() {
        Some(Host::Domain(value)) => value.to_owned(),
        Some(Host::Ipv4(value)) => value.to_string(),
        Some(Host::Ipv6(value)) => format!("[{value}]"),
        None => anyhow::bail!("DATABASE_URL is missing a host"),
    };

    let username = url.username().to_owned();
    let database = url
        .path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .filter(|value| !value.is_empty())
        .context("DATABASE_URL is missing a database name")?
        .to_owned();

    Ok(ParsedDatabaseUrl {
        username,
        host,
        port: url.port().unwrap_or(5432),
        database,
    })
}

/// Derives a connection string for a specific database on the same cluster as
/// `base_url`, keeping the base URL's credentials, host, port and any query
/// parameters (for example `sslmode`).
pub fn database_connection_string(base_url: &str, database_name: &str) -> Result<String> {
    let mut url = Url::parse(base_url).context("base DATABASE_URL is not a valid URI")?;
    let scheme = url.scheme();
    if scheme != "postgresql" && scheme != "postgres" {
        anyhow::bail!("DATABASE_URL must use the postgres or postgresql scheme");
    }
    url.set_path(&format!("/{database_name}"));
    Ok(url.to_string())
}

pub fn provision_database_with_progress<F>(
    base_url: &str,
    request: &ProvisionRequest,
    mut emit: F,
) -> Result<ProvisionOutcome>
where
    F: FnMut(String),
{
    validate_database_name(&request.database_name)?;
    let application_name =
        normalize_application_name(&request.database_name, &request.application_name);
    let parsed = parse_database_url(base_url)?;

    let mut client =
        connect_client(base_url).context("failed to connect to the shared database")?;
    let mut steps = Vec::new();
    push_step(
        &mut steps,
        &mut emit,
        format!(
            "Connected to {}:{} / {} as {}",
            parsed.host, parsed.port, parsed.database, parsed.username
        ),
    );

    let database_created =
        ensure_database(&mut client, &request.database_name, &mut steps, &mut emit)?;
    let role_name = format!("{}_owner", request.database_name);
    push_step(
        &mut steps,
        &mut emit,
        format!("Generating password for role {role_name}"),
    );
    let generated_password = crypto::generate_password()?;
    let role_created = ensure_role(
        &mut client,
        &role_name,
        &generated_password,
        &mut steps,
        &mut emit,
    )?;
    set_owner(
        &mut client,
        &request.database_name,
        &role_name,
        &mut steps,
        &mut emit,
    )?;

    let connection_string = build_connection_string(
        &parsed.host,
        parsed.port,
        &request.database_name,
        &role_name,
        &generated_password,
        &application_name,
    );
    push_step(
        &mut steps,
        &mut emit,
        "Constructed new connection string".to_owned(),
    );

    Ok(ProvisionOutcome {
        database_name: request.database_name.clone(),
        application_name,
        role_name,
        connection_string,
        database_created,
        role_created,
    })
}

pub fn provision_extra_user_with_progress<F>(
    base_url: &str,
    request: &ExtraUserProvisionRequest,
    mut emit: F,
) -> Result<ExtraUserProvisionOutcome>
where
    F: FnMut(String),
{
    validate_database_name(&request.database_name)?;
    validate_database_name(&request.username)?;
    let application_name =
        normalize_application_name(&request.database_name, &request.application_name);
    let parsed = parse_database_url(base_url)?;

    let mut client =
        connect_client(base_url).context("failed to connect to the shared database")?;
    let mut steps = Vec::new();
    push_step(
        &mut steps,
        &mut emit,
        format!(
            "Connected to {}:{} / {} as {}",
            parsed.host, parsed.port, parsed.database, parsed.username
        ),
    );

    ensure_database_exists(&mut client, &request.database_name, &mut steps, &mut emit)?;

    let generated_password = crypto::generate_password()?;
    push_step(
        &mut steps,
        &mut emit,
        format!("Generating password for extra user {}", request.username),
    );

    let role_created = ensure_role(
        &mut client,
        &request.username,
        &generated_password,
        &mut steps,
        &mut emit,
    )?;

    // GRANT CONNECT ON DATABASE is a cluster-level operation — run from base DB
    grant_connect_on_database(
        &mut client,
        &request.database_name,
        &request.username,
        &mut steps,
        &mut emit,
    )?;

    // Schema-level grants and default privileges must run inside the target database
    let target_url = target_url(base_url, &request.database_name)?;
    let mut target_client =
        connect_client(&target_url).context("failed to connect to target database")?;
    let grants_applied =
        apply_schema_grants(&mut target_client, &request.username, &mut steps, &mut emit)?;

    let owner_role = format!("{}_owner", request.database_name);
    apply_default_privileges(
        &mut target_client,
        &owner_role,
        &request.username,
        &mut steps,
        &mut emit,
    )?;
    drop(target_client);

    let connection_string = build_connection_string(
        &parsed.host,
        parsed.port,
        &request.database_name,
        &request.username,
        &generated_password,
        &application_name,
    );
    push_step(
        &mut steps,
        &mut emit,
        "Constructed new connection string".to_owned(),
    );

    Ok(ExtraUserProvisionOutcome {
        database_name: request.database_name.clone(),
        username: request.username.clone(),
        application_name,
        connection_string,
        role_created,
        grants_applied,
    })
}

pub fn provision_full_with_progress<F>(
    base_url: &str,
    request: &ProvisionFullRequest,
    mut emit: F,
) -> Result<ProvisionFullOutcome>
where
    F: FnMut(String),
{
    validate_database_name(&request.database_name)?;
    let application_name =
        normalize_application_name(&request.database_name, &request.application_name);
    let parsed = parse_database_url(base_url)?;

    let mut client =
        connect_client(base_url).context("failed to connect to the shared database")?;
    push_step(
        &mut Vec::new(),
        &mut emit,
        format!(
            "Connected to {}:{} / {} as {}",
            parsed.host, parsed.port, parsed.database, parsed.username
        ),
    );

    let database_created = ensure_database(
        &mut client,
        &request.database_name,
        &mut Vec::new(),
        &mut emit,
    )?;

    let (role_name, role_created, database_connection_string) = if request.dedicated_owner {
        let role_name = format!("{}_owner", request.database_name);
        let generated_password = crypto::generate_password()?;
        let role_created = ensure_role(
            &mut client,
            &role_name,
            &generated_password,
            &mut Vec::new(),
            &mut emit,
        )?;
        set_owner(
            &mut client,
            &request.database_name,
            &role_name,
            &mut Vec::new(),
            &mut emit,
        )?;

        let cs = build_connection_string(
            &parsed.host,
            parsed.port,
            &request.database_name,
            &role_name,
            &generated_password,
            &application_name,
        );
        (role_name, role_created, cs)
    } else {
        // No dedicated role: the database is owned by the base URL user and
        // the saved connection string reuses the instance base credentials.
        push_step(
            &mut Vec::new(),
            &mut emit,
            format!(
                "Using base URL user {} as owner (no dedicated role)",
                parsed.username
            ),
        );
        let cs = database_connection_string(base_url, &request.database_name)?;
        push_step(
            &mut Vec::new(),
            &mut emit,
            "Constructed new connection string".to_owned(),
        );
        (parsed.username.clone(), false, cs)
    };

    let (extra_username, extra_connection_string, extra_role_created, extra_grants_applied) =
        if let Some(ref extra_username) = request.extra_username {
            validate_database_name(extra_username)?;
            let extra_app_name = normalize_application_name(
                &request.database_name,
                request.extra_application_name.as_deref().unwrap_or(""),
            );
            let extra_password = crypto::generate_password()?;
            let ec_created = ensure_role(
                &mut client,
                extra_username,
                &extra_password,
                &mut Vec::new(),
                &mut emit,
            )?;
            grant_connect_on_database(
                &mut client,
                &request.database_name,
                extra_username,
                &mut Vec::new(),
                &mut emit,
            )?;
            let target_url = target_url(base_url, &request.database_name)?;
            let mut target_client =
                connect_client(&target_url).context("failed to connect to target database")?;
            let ga_applied = apply_schema_grants(
                &mut target_client,
                extra_username,
                &mut Vec::new(),
                &mut emit,
            )?;
            apply_default_privileges(
                &mut target_client,
                &role_name,
                extra_username,
                &mut Vec::new(),
                &mut emit,
            )?;
            drop(target_client);

            let extra_cs = build_connection_string(
                &parsed.host,
                parsed.port,
                &request.database_name,
                extra_username,
                &extra_password,
                &extra_app_name,
            );
            (
                Some(extra_username.clone()),
                Some(extra_cs),
                Some(ec_created),
                Some(ga_applied),
            )
        } else {
            (None, None, None, None)
        };

    Ok(ProvisionFullOutcome {
        database_name: request.database_name.clone(),
        application_name,
        role_name,
        database_connection_string,
        database_created,
        role_created,
        extra_username,
        extra_connection_string,
        extra_role_created,
        extra_grants_applied,
    })
}

fn ensure_database(
    client: &mut Client,
    database_name: &str,
    steps: &mut Vec<String>,
    emit: &mut impl FnMut(String),
) -> Result<bool> {
    let exists = client.query_opt(
        "SELECT 1 FROM pg_database WHERE datname = $1",
        &[&database_name],
    )?;
    if exists.is_some() {
        push_step(
            steps,
            emit,
            format!("Database {database_name} already exists"),
        );
        return Ok(false);
    }

    let query = format!("CREATE DATABASE \"{}\"", escape_ident(database_name));
    client.batch_execute(&query)?;
    push_step(steps, emit, format!("Created database {database_name}"));
    Ok(true)
}

fn ensure_database_exists(
    client: &mut Client,
    database_name: &str,
    steps: &mut Vec<String>,
    emit: &mut impl FnMut(String),
) -> Result<()> {
    let exists = client.query_opt(
        "SELECT 1 FROM pg_database WHERE datname = $1",
        &[&database_name],
    )?;
    if exists.is_some() {
        push_step(
            steps,
            emit,
            format!("Database {database_name} already exists"),
        );
        Ok(())
    } else {
        anyhow::bail!("database {database_name} does not exist")
    }
}

fn ensure_role(
    client: &mut Client,
    role_name: &str,
    password: &str,
    steps: &mut Vec<String>,
    emit: &mut impl FnMut(String),
) -> Result<bool> {
    let escaped_password = escape_literal(password);
    let exists = client.query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role_name])?;
    if exists.is_some() {
        let query = format!(
            "ALTER ROLE \"{}\" WITH PASSWORD '{}'",
            escape_ident(role_name),
            escaped_password
        );
        client.batch_execute(&query)?;
        push_step(
            steps,
            emit,
            format!("Role {role_name} already exists; password rotated"),
        );
        return Ok(false);
    }

    let query = format!(
        "CREATE ROLE \"{}\" WITH LOGIN PASSWORD '{}' CREATEDB",
        escape_ident(role_name),
        escaped_password
    );
    client.batch_execute(&query)?;
    push_step(steps, emit, format!("Created role {role_name}"));
    Ok(true)
}

fn set_owner(
    client: &mut Client,
    database_name: &str,
    role_name: &str,
    steps: &mut Vec<String>,
    emit: &mut impl FnMut(String),
) -> Result<()> {
    let query = format!(
        "ALTER DATABASE \"{}\" OWNER TO \"{}\"",
        escape_ident(database_name),
        escape_ident(role_name)
    );
    client.batch_execute(&query)?;
    push_step(
        steps,
        emit,
        format!("Assigned owner {} to database {}", role_name, database_name),
    );
    Ok(())
}

fn target_url(base_url: &str, database_name: &str) -> Result<String> {
    let mut url = Url::parse(base_url).context("invalid base URL")?;
    url.set_path(&format!("/{}", database_name));
    Ok(url.to_string())
}

/// Connects to `url` enforcing the shared CONNECT_TIMEOUT so unreachable hosts
/// (dropped packets, firewall) fail fast instead of hanging the caller on the
/// operating system's default TCP timeout.
fn connect_client(url: &str) -> Result<Client> {
    let mut config: postgres::Config = url
        .parse()
        .context("connection string is not a valid DATABASE_URL")?;
    config.connect_timeout(CONNECT_TIMEOUT);
    Ok(config.connect(NoTls)?)
}

/// Appends libpq's `connect_timeout` to a connection string handed to an
/// external tool (pg_restore), which would otherwise wait on the OS TCP
/// timeout when the target host is unreachable.
fn with_connect_timeout_param(url: &str) -> Result<String> {
    let mut parsed = Url::parse(url).context("invalid connection URL")?;
    parsed
        .query_pairs_mut()
        .append_pair("connect_timeout", &CONNECT_TIMEOUT.as_secs().to_string());
    Ok(parsed.to_string())
}

fn grant_connect_on_database(
    client: &mut Client,
    database_name: &str,
    username: &str,
    steps: &mut Vec<String>,
    emit: &mut impl FnMut(String),
) -> Result<()> {
    let query = format!(
        "GRANT CONNECT ON DATABASE \"{}\" TO \"{}\"",
        escape_ident(database_name),
        escape_ident(username)
    );
    client.batch_execute(&query)?;
    push_step(
        steps,
        emit,
        format!("Granted CONNECT on database {database_name} to {username}"),
    );
    Ok(())
}

fn apply_schema_grants(
    client: &mut Client,
    username: &str,
    steps: &mut Vec<String>,
    emit: &mut impl FnMut(String),
) -> Result<bool> {
    client.batch_execute(&format!(
        "GRANT USAGE ON SCHEMA public TO \"{}\"",
        escape_ident(username)
    ))?;
    client.batch_execute(&format!(
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO \"{}\"",
        escape_ident(username)
    ))?;
    client.batch_execute(&format!(
        "GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO \"{}\"",
        escape_ident(username)
    ))?;
    push_step(
        steps,
        emit,
        format!("Applied app_rw_limited schema grants to {username}"),
    );
    Ok(true)
}

fn apply_default_privileges(
    client: &mut Client,
    owner_role: &str,
    username: &str,
    steps: &mut Vec<String>,
    emit: &mut impl FnMut(String),
) -> Result<()> {
    let exists = client.query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&owner_role])?;
    if exists.is_none() {
        anyhow::bail!("owner role {owner_role} does not exist")
    }

    client.batch_execute(&format!(
        "ALTER DEFAULT PRIVILEGES FOR ROLE \"{}\" IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO \"{}\"",
        escape_ident(owner_role),
        escape_ident(username)
    ))?;
    client.batch_execute(&format!(
        "ALTER DEFAULT PRIVILEGES FOR ROLE \"{}\" IN SCHEMA public GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO \"{}\"",
        escape_ident(owner_role),
        escape_ident(username)
    ))?;
    push_step(
        steps,
        emit,
        format!(
            "Applied default privileges from {} to {}",
            owner_role, username
        ),
    );
    Ok(())
}

fn push_step(steps: &mut Vec<String>, emit: &mut impl FnMut(String), step: impl Into<String>) {
    let step = step.into();
    emit(step.clone());
    steps.push(step);
}

fn build_connection_string(
    host: &str,
    port: u16,
    database_name: &str,
    role_name: &str,
    password: &str,
    application_name: &str,
) -> String {
    let encoded_password: String = byte_serialize(password.as_bytes()).collect();
    let encoded_application_name: String = byte_serialize(application_name.as_bytes()).collect();
    format!(
        "postgresql://{}:{}@{}:{}/{}?application_name={}",
        role_name, encoded_password, host, port, database_name, encoded_application_name
    )
}

fn escape_ident(value: &str) -> String {
    value.replace('"', "\"\"")
}

fn escape_literal(value: &str) -> String {
    value.replace('\'', "''")
}

pub fn check_pg_tools() -> PgToolBackend {
    let try_native = || -> Option<(String, String)> {
        let dump_ver = run_tool("pg_dump", ["--version"]).ok()?;
        let restore_ver = run_tool("pg_restore", ["--version"]).ok()?;
        Some((dump_ver, restore_ver))
    };

    if let Some((dump_ver, restore_ver)) = try_native() {
        return PgToolBackend::Native {
            dump_ver,
            restore_ver,
        };
    }

    let docker_ok = std::process::Command::new("docker")
        .args(["--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if docker_ok {
        PgToolBackend::Docker {
            image: "postgres:18-alpine".to_owned(),
        }
    } else {
        PgToolBackend::NotFound
    }
}

pub fn detect_source_version(source_cs: &str) -> Result<String> {
    let mut client = connect_client(source_cs).context("failed to connect to source database")?;
    let row = client
        .query_one("SELECT version()", &[])
        .context("failed to query source version")?;
    let version: String = row.get(0);
    Ok(version)
}

/// Lists ordinary, connectable databases in a PostgreSQL instance. Template
/// databases are excluded because they are cluster implementation details,
/// not application databases.
pub fn discover_databases(instance_cs: &str) -> Result<Vec<DiscoveredDatabase>> {
    let mut client =
        connect_client(instance_cs).context("failed to connect while discovering databases")?;
    let rows = client.query(
        "SELECT datname, pg_get_userbyid(datdba), pg_encoding_to_char(encoding), \
                dattablespace::regclass::text, datconnlimit \
         FROM pg_database \
         WHERE datallowconn AND NOT datistemplate \
         ORDER BY datname",
        &[],
    )?;
    Ok(rows
        .into_iter()
        .map(|row| DiscoveredDatabase {
            name: row.get(0),
            owner: row.get(1),
            encoding: row.get(2),
            tablespace: row.get(3),
            connection_limit: row.get(4),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Query console (web)
// ---------------------------------------------------------------------------

/// Fixed page size of the query console's table data browser.
pub const TABLE_PAGE_SIZE: i64 = 200;

/// Row cap applied to manually written queries in the web console.
pub const QUERY_ROW_CAP: usize = 500;

const QUERY_STATEMENT_TIMEOUT_MS: u64 = 30_000;

/// Lists ordinary tables and views of a database for the web query console.
/// System schemas (`pg_catalog`, `information_schema`, TOAST) are excluded.
pub fn list_database_tables(client_cs: &str) -> Result<Vec<DatabaseTableInfo>> {
    let mut client = connect_client(client_cs).context("failed to connect while listing tables")?;
    let rows = client.query(
        r#"
        SELECT n.nspname,
               c.relname,
               CASE c.relkind
                   WHEN 'r' THEN 'table'
                   WHEN 'p' THEN 'table'
                   WHEN 'v' THEN 'view'
                   WHEN 'm' THEN 'matview'
                   ELSE 'other'
               END,
               GREATEST(c.reltuples, 0)::bigint,
               COALESCE(pg_total_relation_size(c.oid), 0)::bigint
        FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE c.relkind IN ('r', 'p', 'v', 'm')
          AND n.nspname NOT IN ('pg_catalog', 'information_schema')
          AND n.nspname NOT LIKE 'pg_toast%'
        ORDER BY n.nspname, c.relname
        "#,
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|row| DatabaseTableInfo {
            schema: row.get(0),
            name: row.get(1),
            kind: row.get(2),
            rows_estimate: row.get(3),
            size_bytes: row.get(4),
        })
        .collect())
}

/// Browses one page of rows from `schema.table` (always read-only). The table
/// must exist and be a plain table, partitioned table or view.
pub fn run_table_page(
    client_cs: &str,
    schema: &str,
    table: &str,
    offset: i64,
) -> Result<SqlQueryResult> {
    let mut client = connect_client(client_cs).context("failed to connect while browsing table")?;
    let exists = client.query_opt(
        "SELECT 1 FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r', 'p', 'v', 'm')",
        &[&schema, &table],
    )?;
    if exists.is_none() {
        anyhow::bail!("table '{schema}.{table}' does not exist or is not browsable");
    }
    apply_query_console_settings(&mut client, true)?;
    let sql = format!(
        "SELECT * FROM \"{}\".\"{}\" OFFSET {} LIMIT {}",
        escape_ident(schema),
        escape_ident(table),
        offset.max(0),
        TABLE_PAGE_SIZE,
    );
    let cap = usize::try_from(TABLE_PAGE_SIZE.max(0)).unwrap_or(0);
    execute_simple_query(&mut client, &sql, cap)
}

/// Runs a manually written query. With `read_only`, the session rejects any
/// statement that writes to the database. Results are capped at
/// [`QUERY_ROW_CAP`] rows (flagged via `truncated`).
pub fn run_sql_query(client_cs: &str, sql: &str, read_only: bool) -> Result<SqlQueryResult> {
    if sql.trim().is_empty() {
        anyhow::bail!("query is empty");
    }
    let mut client = connect_client(client_cs).context("failed to connect while running query")?;
    apply_query_console_settings(&mut client, read_only)?;
    execute_simple_query(&mut client, sql, QUERY_ROW_CAP)
}

fn apply_query_console_settings(client: &mut Client, read_only: bool) -> Result<()> {
    client.batch_execute(&format!(
        "SET statement_timeout = {QUERY_STATEMENT_TIMEOUT_MS}; \
         SET default_transaction_read_only = {};",
        if read_only { "on" } else { "off" }
    ))?;
    Ok(())
}

/// Executes `sql` with the simple query protocol (text values, multiple
/// statements allowed) and caps the serialized rows at `row_cap`.
fn execute_simple_query(client: &mut Client, sql: &str, row_cap: usize) -> Result<SqlQueryResult> {
    let start = std::time::Instant::now();
    let messages = client.simple_query(sql)?;
    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut truncated = false;
    for message in &messages {
        let SimpleQueryMessage::Row(row) = message else {
            continue;
        };
        if columns.is_empty() {
            columns = row
                .columns()
                .iter()
                .map(|column| column.name().to_owned())
                .collect();
        }
        if rows.len() >= row_cap {
            truncated = true;
            break;
        }
        let mut values = Vec::with_capacity(row.len());
        for index in 0..row.len() {
            values.push(row.get(index).map(str::to_owned));
        }
        rows.push(values);
    }
    Ok(SqlQueryResult {
        columns,
        rows,
        truncated,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

pub fn resolve_docker_image(backend: &PgToolBackend, source_cs: Option<&str>) -> String {
    match backend {
        PgToolBackend::Docker { image } => {
            let Some(source_cs) = source_cs else {
                return image.clone();
            };
            match detect_source_version(source_cs) {
                Ok(version) => {
                    let major = extract_pg_major_version(&version);
                    if detect_timescale_installed(source_cs) {
                        format!("timescale/timescaledb:latest-pg{major}")
                    } else {
                        format!("postgres:{major}-alpine")
                    }
                }
                Err(_) => image.clone(),
            }
        }
        _ => "postgres:18-alpine".to_owned(),
    }
}

/// Reports whether the source database has the TimescaleDB extension installed.
/// When it does, pg_dump/pg_restore must run inside an image that ships the
/// extension (timescale/timescaledb), otherwise restoring a backup that uses
/// hypertables fails because the extension is missing from the plain
/// postgres:<major>-alpine image.
fn detect_timescale_installed(source_cs: &str) -> bool {
    let Ok(mut client) = connect_client(source_cs) else {
        return false;
    };
    let found = client
        .query_one(
            "SELECT 1 FROM pg_extension WHERE extname = 'timescaledb'",
            &[],
        )
        .is_ok();
    let _ = client.close();
    found
}

pub fn extract_pg_major_version(version: &str) -> u32 {
    let mut tokens = version.split_whitespace();
    while let Some(token) = tokens.next() {
        if (token == "PostgreSQL" || token == "YugabyteDB")
            && let Some(ver_token) = tokens.next()
            && let Some(major_str) = ver_token.split('.').next()
            && let Ok(major) = major_str.parse::<u32>()
        {
            return major;
        }
    }
    // Fallback: try the last token (handles "pg_dump (PostgreSQL) 16.4")
    if let Some(last) = version.split_whitespace().last()
        && let Some(major_str) = last.split('.').next()
        && let Ok(major) = major_str.parse::<u32>()
    {
        return major;
    }
    0 // unknown; callers must not treat an unparseable version as compatible
}

pub fn check_version_warning(source_cs: &str, backend: &PgToolBackend) -> Option<String> {
    let source_ver = match detect_source_version(source_cs) {
        Ok(v) => v,
        Err(_) => return None,
    };
    let source_major = extract_pg_major_version(&source_ver);

    match backend {
        PgToolBackend::Native { dump_ver, .. } => {
            let dump_major = extract_pg_major_version(dump_ver);
            if source_major == 0 || dump_major == 0 || source_major < dump_major {
                return Some(format!(
                    "Incompatible PostgreSQL tools: pg_dump is v{dump_major}, \
                     source server is PostgreSQL {source_major}. Use a pg_dump version \
                     at least as new as the source server."
                ));
            }
        }
        PgToolBackend::Docker { .. } => {
            // Docker resolves the image to match the server — no warning needed
        }
        PgToolBackend::NotFound => {}
    }
    None
}

/// Pull a Docker image silently, so pull progress doesn't contaminate
/// stderr of subsequent Docker commands.
fn docker_pull_silent(image: &str) -> Result<()> {
    let pull = std::process::Command::new("docker")
        .args(["pull", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .with_context(|| format!("failed to execute docker pull for image '{}'", image))?;
    if !pull.status.success() {
        let stderr = String::from_utf8_lossy(&pull.stderr);
        anyhow::bail!("failed to pull Docker image '{}': {}", image, stderr.trim());
    }
    Ok(())
}

fn dump_globals(
    source_cs: &str,
    include_role_passwords: bool,
    backend: &PgToolBackend,
) -> Result<Vec<u8>> {
    let mut args = vec!["--globals-only"];
    if !include_role_passwords {
        args.push("--no-role-passwords");
    }
    args.push("-d");
    args.push(source_cs);
    match backend {
        PgToolBackend::Native { .. } => {
            let output = std::process::Command::new("pg_dumpall")
                .args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .context("failed to execute pg_dumpall")?;
            if !output.status.success() {
                anyhow::bail!(
                    "pg_dumpall failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )
            }
            Ok(output.stdout)
        }
        PgToolBackend::Docker { image } => {
            docker_pull_silent(image)?;
            let mut docker_args = vec![
                "run",
                "--rm",
                "-i",
                "--network",
                "host",
                image.as_str(),
                "pg_dumpall",
            ];
            docker_args.push("--globals-only");
            if !include_role_passwords {
                docker_args.push("--no-role-passwords");
            }
            docker_args.push("-d");
            docker_args.push(source_cs);
            let output = std::process::Command::new("docker")
                .args(&docker_args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .context("failed to execute Docker pg_dumpall")?;
            if !output.status.success() {
                anyhow::bail!(
                    "pg_dumpall failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )
            }
            Ok(output.stdout)
        }
        PgToolBackend::NotFound => anyhow::bail!("PostgreSQL tools are unavailable"),
    }
}

fn restore_globals(dest_cs: &str, globals: &[u8], backend: &PgToolBackend) -> Result<()> {
    if globals.is_empty() {
        return Ok(());
    }
    let mut command = match backend {
        PgToolBackend::Native { .. } => {
            let mut command = std::process::Command::new("psql");
            command.args(["-X", "-d", dest_cs]);
            command
        }
        PgToolBackend::Docker { image } => {
            docker_pull_silent(image)?;
            let mut command = std::process::Command::new("docker");
            command.args([
                "run",
                "--rm",
                "-i",
                "--network",
                "host",
                image.as_str(),
                "psql",
                "-X",
                "-d",
                dest_cs,
            ]);
            command
        }
        PgToolBackend::NotFound => anyhow::bail!("PostgreSQL tools are unavailable"),
    };
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to execute psql for cluster globals")?;
    use std::io::Write;
    child
        .stdin
        .take()
        .context("failed to open psql stdin")?
        .write_all(globals)
        .context("failed to send cluster globals to psql")?;
    let output = child
        .wait_with_output()
        .context("failed waiting for psql cluster globals restore")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("already exists") {
            // Role / tablespace already exists — benign during migration
            eprintln!(
                "  [warn] globals restore completed with messages:\n{}",
                stderr.trim()
            );
        } else {
            anyhow::bail!("restoring cluster globals failed: {}", stderr.trim())
        }
    }
    Ok(())
}

pub fn migrate_database_with_progress<F>(
    source_cs: &str,
    dest_base_url: &str,
    dest_db_name: &str,
    backend: &PgToolBackend,
    emit: &mut F,
) -> Result<ProvisionFullOutcome>
where
    F: FnMut(String),
{
    if let PgToolBackend::NotFound = backend {
        anyhow::bail!(
            "pg_dump/pg_restore not found. Install PostgreSQL client tools (brew install postgresql, \
             apt install postgresql-client, dnf install postgresql) or Docker."
        );
    }

    let parsed = parse_database_url(dest_base_url)?;
    let dump_dir = tempfile::tempdir().context("failed to create temp directory")?;
    let dump_path = dump_dir.path().join(format!("{}.pgdump", dest_db_name));

    push_step(&mut Vec::new(), emit, "Starting pg_dump...".to_owned());

    let dump_output = match backend {
        PgToolBackend::Native { .. } => std::process::Command::new("pg_dump")
            .args(["-Fc", "-f"])
            .arg(&dump_path)
            .args(["-d", source_cs])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .context("failed to execute pg_dump")?,
        PgToolBackend::Docker { image } => {
            docker_pull_silent(image)?;
            let output = std::process::Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "-i",
                    "--network",
                    "host",
                    image.as_str(),
                    "pg_dump",
                    "-Fc",
                    "-d",
                    source_cs,
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .context("failed to execute docker pg_dump")?;
            std::fs::write(&dump_path, &output.stdout)
                .context("failed to write pg_dump output to file")?;
            output
        }
        PgToolBackend::NotFound => unreachable!(),
    };

    if !dump_output.status.success() {
        let stderr = String::from_utf8_lossy(&dump_output.stderr);
        anyhow::bail!("pg_dump failed: {}", stderr.trim());
    }

    let dump_stderr = String::from_utf8_lossy(&dump_output.stderr);
    for line in dump_stderr.lines() {
        let line = line.trim();
        if !line.is_empty() {
            push_step(&mut Vec::new(), emit, format!("pg_dump: {line}"));
        }
    }

    push_step(
        &mut Vec::new(),
        emit,
        format!("pg_dump completed. Checking if '{dest_db_name}' exists on target..."),
    );

    let admin_url = dest_base_url;
    let mut admin_client =
        connect_client(admin_url).context("failed to connect to destination cluster")?;

    let db_exists = admin_client
        .query_opt(
            "SELECT 1 FROM pg_database WHERE datname = $1",
            &[&dest_db_name],
        )?
        .is_some();

    if db_exists {
        anyhow::bail!("database '{dest_db_name}' already exists on the target cluster");
    }

    push_step(
        &mut Vec::new(),
        emit,
        format!("Creating database '{dest_db_name}' on target..."),
    );

    let create_query = format!(
        "CREATE DATABASE \"{}\" TEMPLATE template0",
        escape_ident(dest_db_name)
    );
    admin_client
        .batch_execute(&create_query)
        .context("failed to create destination database")?;

    let dest_url = with_connect_timeout_param(&target_url(dest_base_url, dest_db_name)?)?;
    push_step(
        &mut Vec::new(),
        emit,
        format!(
            "Starting pg_restore to {}:{} / {}...",
            parsed.host, parsed.port, dest_db_name
        ),
    );

    let restore_result = match backend {
        PgToolBackend::Native { .. } => std::process::Command::new("pg_restore")
            .args([
                "--dbname",
                &dest_url,
                "--no-owner",
                "--no-privileges",
                "--exit-on-error",
            ])
            .arg(&dump_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .context("failed to execute pg_restore"),
        PgToolBackend::Docker { image } => (|| -> Result<std::process::Output> {
            docker_pull_silent(image)?;
            let file = std::fs::File::open(&dump_path)
                .context("failed to open dump file for pg_restore")?;
            std::process::Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "-i",
                    "--network",
                    "host",
                    image.as_str(),
                    "pg_restore",
                    "--dbname",
                    &dest_url,
                    "--no-owner",
                    "--no-privileges",
                    "--exit-on-error",
                ])
                .stdin(file)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .context("failed to execute docker pg_restore")
        })(),
        PgToolBackend::NotFound => unreachable!(),
    };

    let restore_output = match restore_result {
        Ok(output) => output,
        Err(error) => {
            let cleanup = drop_database(&mut admin_client, dest_db_name);
            return Err(combine_restore_cleanup_error(error, cleanup));
        }
    };

    // Drop the created database if restore fails
    if !restore_output.status.success() {
        let stderr = String::from_utf8_lossy(&restore_output.stderr);
        let error = anyhow::anyhow!("pg_restore failed: {}", stderr.trim());
        let cleanup = drop_database(&mut admin_client, dest_db_name);
        return Err(combine_restore_cleanup_error(error, cleanup));
    }

    let restore_stderr = String::from_utf8_lossy(&restore_output.stderr);
    for line in restore_stderr.lines() {
        let line = line.trim();
        if !line.is_empty() {
            push_step(&mut Vec::new(), emit, format!("pg_restore: {line}"));
        }
    }

    push_step(
        &mut Vec::new(),
        emit,
        format!("Migration to '{dest_db_name}' completed successfully."),
    );

    let outcome = ProvisionFullOutcome {
        database_name: dest_db_name.to_owned(),
        application_name: dest_db_name.to_owned(),
        role_name: parsed.username,
        database_connection_string: dest_url.clone(),
        database_created: true,
        role_created: false,
        extra_username: None,
        extra_connection_string: None,
        extra_role_created: None,
        extra_grants_applied: None,
    };

    Ok(outcome)
}

/// Magic marker "DBP1" at the start of decrypted backup plaintext.
///
/// `DBP1` = "Database Provisioner v1". When present, the decrypted payload
/// contains a JSON metadata header before the engine dump bytes. When absent,
/// the file is in the legacy format (raw dump data only).
///
/// Detecting this marker allows backward compatibility with backup files
/// created before the metadata feature was introduced.
///
/// Post-decryption format with magic:
///   [4B magic "DBP1"][4B LE metadata_len][JSON metadata][engine dump bytes]
const BACKUP_MAGIC: [u8; 4] = *b"DBP1";

/// Writes an encrypted dump file: [nonce_len:4 LE][nonce][ciphertext]
///
/// When `metadata` is `Some`, the decrypted payload will be:
///   [magic][meta_len][JSON metadata][dump_data]
/// Otherwise the plain payload is just `dump_data` (legacy format).
pub fn create_encrypted_dump(
    dump_data: &[u8],
    metadata: Option<&BackupMetadata>,
    encrypt_key: &[u8],
    output_path: &std::path::Path,
) -> Result<()> {
    let plaintext = if let Some(meta) = metadata {
        let meta_json = serde_json::to_vec(meta).context("failed to serialize backup metadata")?;
        let meta_len = (meta_json.len() as u32).to_le_bytes();
        let mut buf = Vec::with_capacity(4 + 4 + meta_json.len() + dump_data.len());
        buf.extend_from_slice(&BACKUP_MAGIC);
        buf.extend_from_slice(&meta_len);
        buf.extend_from_slice(&meta_json);
        buf.extend_from_slice(dump_data);
        buf
    } else {
        dump_data.to_vec()
    };

    let encrypted = crate::crypto::encrypt(encrypt_key, &plaintext)?;
    let partial_path = output_path.with_extension("enc.partial");
    let mut file = std::fs::File::create(&partial_path)
        .with_context(|| format!("failed to create {}", partial_path.display()))?;
    use std::io::Write;
    let nonce_len = encrypted.nonce.len() as u32;
    let result = (|| -> Result<()> {
        file.write_all(&nonce_len.to_le_bytes())?;
        file.write_all(&encrypted.nonce)?;
        file.write_all(&encrypted.ciphertext)?;
        file.sync_all()
            .context("failed to flush encrypted backup")?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&partial_path);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&partial_path, output_path) {
        let _ = std::fs::remove_file(&partial_path);
        return Err(error).with_context(|| {
            format!(
                "failed to publish encrypted backup to {}",
                output_path.display()
            )
        });
    }
    Ok(())
}

fn safe_filename_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "unnamed".to_owned()
    } else {
        sanitized
    }
}

fn drop_database(client: &mut Client, database_name: &str) -> Result<()> {
    let query = format!(
        "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
        escape_ident(database_name)
    );
    client
        .batch_execute(&query)
        .context("failed to clean up partially restored database")
}

fn combine_restore_cleanup_error(
    restore_error: anyhow::Error,
    cleanup: Result<()>,
) -> anyhow::Error {
    match cleanup {
        Ok(()) => restore_error,
        Err(cleanup_error) => {
            anyhow::anyhow!("{restore_error}; cleanup also failed: {cleanup_error}")
        }
    }
}

fn error_string_database_exists(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .contains("already exists on the target cluster")
}

/// Reads an encrypted dump file, returns (optional metadata, decrypted dump bytes).
///
/// Detects the `BACKUP_MAGIC` marker to differentiate new-format files
/// (with embedded metadata) from legacy-format files (raw dump only).
pub fn read_encrypted_dump(
    input_path: &std::path::Path,
    decrypt_key: &[u8],
) -> Result<(Option<BackupMetadata>, Vec<u8>)> {
    let data = std::fs::read(input_path)
        .with_context(|| format!("failed to read {}", input_path.display()))?;
    if data.len() < 4 {
        anyhow::bail!("file too small to contain encrypted data");
    }
    let nonce_len = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
    if data.len() < 4 + nonce_len {
        anyhow::bail!("file is truncated: missing nonce");
    }
    let nonce = data[4..4 + nonce_len].to_vec();
    let ciphertext = data[4 + nonce_len..].to_vec();
    let encrypted = crate::models::EncryptedValue { ciphertext, nonce };
    let plaintext = crate::crypto::decrypt(decrypt_key, &encrypted)?;

    if plaintext.len() < 4 {
        return Ok((None, plaintext.to_vec()));
    }

    if plaintext[..4] == BACKUP_MAGIC {
        let rest = &plaintext[4..];
        if rest.len() < 4 {
            anyhow::bail!("backup file has magic marker but is missing metadata length");
        }
        let meta_len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        if rest.len() < 4 + meta_len {
            anyhow::bail!("backup file has magic marker but metadata is truncated");
        }
        let meta_json = &rest[4..4 + meta_len];
        let metadata: BackupMetadata =
            serde_json::from_slice(meta_json).context("failed to parse backup metadata")?;
        let dump_bytes = rest[4 + meta_len..].to_vec();
        Ok((Some(metadata), dump_bytes))
    } else {
        Ok((None, plaintext.to_vec()))
    }
}

pub fn backup_database_with_progress<F>(
    source_cs: &str,
    encrypt_key: &[u8],
    output_dir: &std::path::Path,
    backend: &PgToolBackend,
    metadata: Option<&BackupMetadata>,
    emit: &mut F,
) -> Result<BackupOutcome>
where
    F: FnMut(String),
{
    if let PgToolBackend::NotFound = backend {
        anyhow::bail!("pg_dump not found. Install PostgreSQL client tools or Docker.")
    }

    let parsed = parse_database_url(source_cs)?;
    let dump_dir = tempfile::tempdir().context("failed to create temp directory")?;
    let dump_path = dump_dir.path().join(format!("{}.pgdump", parsed.database));

    push_step(&mut Vec::new(), emit, "Starting pg_dump...".to_owned());
    let dump_output = match backend {
        PgToolBackend::Native { .. } => std::process::Command::new("pg_dump")
            .args(["-Fc", "-f"])
            .arg(&dump_path)
            .args(["-d", source_cs])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .context("failed to execute pg_dump")?,
        PgToolBackend::Docker { image } => {
            docker_pull_silent(image)?;
            let output = std::process::Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "-i",
                    "--network",
                    "host",
                    image.as_str(),
                    "pg_dump",
                    "-Fc",
                    "-d",
                    source_cs,
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .context("failed to execute docker pg_dump")?;
            std::fs::write(&dump_path, &output.stdout)
                .context("failed to write pg_dump output to file")?;
            output
        }
        PgToolBackend::NotFound => unreachable!(),
    };

    if !dump_output.status.success() {
        let stderr = String::from_utf8_lossy(&dump_output.stderr);
        anyhow::bail!("pg_dump failed: {}", stderr.trim());
    }

    let dump_stderr = String::from_utf8_lossy(&dump_output.stderr);
    for line in dump_stderr.lines() {
        let line = line.trim();
        if !line.is_empty() {
            push_step(&mut Vec::new(), emit, format!("pg_dump: {line}"));
        }
    }

    push_step(
        &mut Vec::new(),
        emit,
        "pg_dump completed, encrypting dump...".to_owned(),
    );

    let dump_data = std::fs::read(&dump_path).context("failed to read dump file")?;
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = if let Some(meta) = metadata {
        format!(
            "{}_{}_{}_{}.pgdump.enc",
            safe_filename_component(&meta.hostname),
            safe_filename_component(&meta.instance_name),
            safe_filename_component(&meta.database_name),
            timestamp
        )
    } else {
        format!(
            "{}_{}.pgdump.enc",
            safe_filename_component(&parsed.database),
            timestamp
        )
    };
    let output_path = output_dir.join(&filename);

    std::fs::create_dir_all(output_dir).context("failed to create output directory")?;
    create_encrypted_dump(&dump_data, metadata, encrypt_key, &output_path)?;

    push_step(
        &mut Vec::new(),
        emit,
        format!("Encrypted backup saved to {}", output_path.display()),
    );

    Ok(BackupOutcome {
        file_path: output_path.to_string_lossy().to_string(),
        database_name: parsed.database.clone(),
        database_names: vec![parsed.database],
    })
}

const INSTANCE_BACKUP_MAGIC: [u8; 4] = *b"DBP2";

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct InstanceBackupManifest {
    format: String,
    source_instance: String,
    source_version: String,
    created_at: String,
    databases: Vec<String>,
    includes_globals: bool,
    include_role_passwords: bool,
    tablespace_mode: String,
}

pub struct InstanceBackupContext<'a> {
    pub instance_name: &'a str,
    pub machine_id: &'a str,
    pub hostname: &'a str,
}

pub struct InstanceBackupContents {
    pub databases: Vec<String>,
    pub dumps: Vec<(String, Vec<u8>)>,
    pub globals: Vec<u8>,
    pub source_instance: String,
    pub source_version: String,
    pub includes_globals: bool,
    pub include_role_passwords: bool,
    pub tablespace_mode: String,
}

/// Creates one encrypted bundle containing one custom-format dump per
/// connectable, non-template database in the instance.
#[allow(clippy::too_many_arguments)]
pub fn backup_instance_with_progress<F>(
    source_cs: &str,
    encrypt_key: &[u8],
    output_dir: &std::path::Path,
    backend: &PgToolBackend,
    context: InstanceBackupContext<'_>,
    selected_database_names: &[String],
    config: &BackupConfig,
    emit: &mut F,
) -> Result<BackupOutcome>
where
    F: FnMut(String),
{
    let discovered = discover_databases(source_cs)?;
    let databases: Vec<DiscoveredDatabase> = discovered
        .into_iter()
        .filter(|database| {
            selected_database_names
                .iter()
                .any(|name| name == &database.name)
        })
        .collect();
    if databases.is_empty() {
        anyhow::bail!("instance contains no connectable non-template databases")
    }

    let source_version = detect_source_version(source_cs).unwrap_or_else(|_| "unknown".to_owned());

    let staging =
        tempfile::tempdir().context("failed to create instance backup staging directory")?;
    let globals = if config.include_globals {
        emit("Backing up cluster roles and memberships...".to_owned());
        dump_globals(source_cs, config.include_role_passwords, backend)?
    } else {
        Vec::new()
    };
    let mut dumps = Vec::with_capacity(databases.len());
    for (index, database) in databases.iter().enumerate() {
        emit(format!(
            "Backing up database {}/{}: {}",
            index + 1,
            databases.len(),
            database.name
        ));
        let source_db_url = target_url(source_cs, &database.name)?;
        let metadata = BackupMetadata {
            machine_id: context.machine_id.to_owned(),
            hostname: context.hostname.to_owned(),
            instance_name: context.instance_name.to_owned(),
            database_name: database.name.clone(),
            application_name: String::new(),
            engine: "postgresql".to_owned(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let outcome = backup_database_with_progress(
            &source_db_url,
            encrypt_key,
            staging.path(),
            backend,
            Some(&metadata),
            emit,
        )?;
        let (_, dump) = read_encrypted_dump(std::path::Path::new(&outcome.file_path), encrypt_key)?;
        dumps.push((database.name.clone(), dump));
    }

    let tspace_mode = match config.tablespace_mode {
        TablespaceMode::Flatten => "flatten",
        TablespaceMode::Preserve => "preserve",
    };
    let manifest = InstanceBackupManifest {
        format: "DBP2".to_owned(),
        source_instance: context.instance_name.to_owned(),
        source_version,
        created_at: chrono::Utc::now().to_rfc3339(),
        databases: databases
            .iter()
            .map(|database| database.name.clone())
            .collect(),
        includes_globals: config.include_globals,
        include_role_passwords: config.include_role_passwords,
        tablespace_mode: tspace_mode.to_owned(),
    };
    let manifest_json =
        serde_json::to_vec(&manifest).context("failed to serialize instance backup manifest")?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&INSTANCE_BACKUP_MAGIC);
    payload.extend_from_slice(&(manifest_json.len() as u32).to_le_bytes());
    payload.extend_from_slice(&manifest_json);
    payload.extend_from_slice(&(dumps.len() as u32).to_le_bytes());
    for (name, dump) in &dumps {
        payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
        payload.extend_from_slice(name.as_bytes());
        payload.extend_from_slice(&(dump.len() as u64).to_le_bytes());
        payload.extend_from_slice(dump);
    }
    payload.extend_from_slice(&(globals.len() as u64).to_le_bytes());
    payload.extend_from_slice(&globals);

    std::fs::create_dir_all(output_dir).context("failed to create output directory")?;
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S_%f");
    let filename = format!(
        "{}_{}_{}_{}.cluster.pgdump.enc",
        safe_filename_component(context.hostname),
        safe_filename_component(context.instance_name),
        dumps.len(),
        timestamp
    );
    let output_path = output_dir.join(filename);
    create_encrypted_dump(&payload, None, encrypt_key, &output_path)?;
    emit(format!(
        "Encrypted instance backup saved to {}",
        output_path.display()
    ));
    Ok(BackupOutcome {
        file_path: output_path.to_string_lossy().to_string(),
        database_name: context.instance_name.to_owned(),
        database_names: manifest.databases,
    })
}

pub fn read_instance_backup(
    input_path: &std::path::Path,
    decrypt_key: &[u8],
) -> Result<InstanceBackupContents> {
    let (_, payload) = read_encrypted_dump(input_path, decrypt_key)?;
    if payload.len() < 8 || payload[..4] != INSTANCE_BACKUP_MAGIC {
        anyhow::bail!("backup is not a DBP2 instance bundle")
    }
    let manifest_len = u32::from_le_bytes(payload[4..8].try_into().unwrap()) as usize;
    let manifest_end = 8usize
        .checked_add(manifest_len)
        .context("instance backup manifest length overflow")?;
    if payload.len() < manifest_end + 4 {
        anyhow::bail!("instance backup manifest is truncated")
    }
    let manifest: InstanceBackupManifest = serde_json::from_slice(&payload[8..manifest_end])
        .context("failed to parse instance backup manifest")?;
    let mut cursor = manifest_end;
    let count = u32::from_le_bytes(payload[cursor..cursor + 4].try_into().unwrap()) as usize;
    cursor += 4;
    let mut dumps = Vec::with_capacity(count);
    for _ in 0..count {
        if payload.len() < cursor + 4 {
            anyhow::bail!("instance backup database name is truncated")
        }
        let name_len = u32::from_le_bytes(payload[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;
        if payload.len() < cursor + name_len + 8 {
            anyhow::bail!("instance backup database entry is truncated")
        }
        let name = String::from_utf8(payload[cursor..cursor + name_len].to_vec())
            .context("instance backup contains an invalid database name")?;
        cursor += name_len;
        let dump_len = u64::from_le_bytes(payload[cursor..cursor + 8].try_into().unwrap()) as usize;
        cursor += 8;
        let end = cursor
            .checked_add(dump_len)
            .context("instance dump length overflow")?;
        if payload.len() < end {
            anyhow::bail!("instance backup dump is truncated")
        }
        dumps.push((name, payload[cursor..end].to_vec()));
        cursor = end;
    }
    if payload.len() < cursor + 8 {
        anyhow::bail!("instance backup globals artifact is truncated")
    }
    let globals_len = u64::from_le_bytes(payload[cursor..cursor + 8].try_into().unwrap()) as usize;
    cursor += 8;
    let globals_end = cursor
        .checked_add(globals_len)
        .context("instance globals length overflow")?;
    if payload.len() < globals_end {
        anyhow::bail!("instance backup globals artifact is truncated")
    }
    let globals = payload[cursor..globals_end].to_vec();
    if manifest.includes_globals == globals.is_empty() {
        anyhow::bail!("instance backup globals metadata does not match its artifact")
    }
    if manifest.databases
        != dumps
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    {
        anyhow::bail!("instance backup manifest does not match its database entries")
    }
    Ok(InstanceBackupContents {
        databases: manifest.databases,
        dumps,
        globals,
        source_instance: manifest.source_instance,
        source_version: manifest.source_version,
        includes_globals: manifest.includes_globals,
        include_role_passwords: manifest.include_role_passwords,
        tablespace_mode: manifest.tablespace_mode,
    })
}

pub fn is_instance_backup(input_path: &std::path::Path, decrypt_key: &[u8]) -> Result<bool> {
    let (_, payload) = read_encrypted_dump(input_path, decrypt_key)?;
    Ok(payload.starts_with(&INSTANCE_BACKUP_MAGIC))
}

#[allow(clippy::too_many_arguments)]
pub fn restore_database_with_progress<F>(
    input_path: &std::path::Path,
    decrypt_key: &[u8],
    dest_base_url: &str,
    dest_db_name: &str,
    backend: &PgToolBackend,
    conflict_policy: ConflictPolicy,
    tablespace_flatten: bool,
    emit: &mut F,
) -> Result<ProvisionFullOutcome>
where
    F: FnMut(String),
{
    if let PgToolBackend::NotFound = backend {
        anyhow::bail!("pg_restore not found. Install PostgreSQL client tools or Docker.")
    }

    let parsed = parse_database_url(dest_base_url)?;
    let dump_dir = tempfile::tempdir().context("failed to create temp directory")?;
    let dump_path = dump_dir.path().join(format!("{}.pgdump", dest_db_name));

    push_step(
        &mut Vec::new(),
        emit,
        "Reading encrypted backup and decrypting...".to_owned(),
    );

    let (backup_meta, decrypted) = read_encrypted_dump(input_path, decrypt_key)?;

    if let Some(ref meta) = backup_meta {
        push_step(
            &mut Vec::new(),
            emit,
            format!(
                "Backup origin: {} / {} on {} (v{}, {})",
                meta.instance_name, meta.database_name, meta.hostname, meta.version, meta.timestamp
            ),
        );
    }

    std::fs::write(&dump_path, &decrypted).context("failed to write decrypted dump")?;

    push_step(
        &mut Vec::new(),
        emit,
        format!("Decrypted. Checking if '{dest_db_name}' exists on target..."),
    );

    let mut admin_client =
        connect_client(dest_base_url).context("failed to connect to destination cluster")?;

    let db_exists = admin_client
        .query_opt(
            "SELECT 1 FROM pg_database WHERE datname = $1",
            &[&dest_db_name],
        )?
        .is_some();

    if db_exists {
        match conflict_policy {
            ConflictPolicy::Fail => {
                anyhow::bail!("database '{dest_db_name}' already exists on the target cluster");
            }
            ConflictPolicy::Skip => {
                push_step(
                    &mut Vec::new(),
                    emit,
                    format!("  [skip] '{dest_db_name}' already exists — leaving untouched"),
                );
                let parsed = parse_database_url(dest_base_url)?;
                let dest_url = target_url(dest_base_url, dest_db_name)?;
                return Ok(ProvisionFullOutcome {
                    database_name: dest_db_name.to_owned(),
                    application_name: dest_db_name.to_owned(),
                    role_name: parsed.username,
                    database_connection_string: dest_url,
                    database_created: false,
                    role_created: false,
                    extra_username: None,
                    extra_connection_string: None,
                    extra_role_created: None,
                    extra_grants_applied: None,
                });
            }
            ConflictPolicy::Replace => {
                push_step(
                    &mut Vec::new(),
                    emit,
                    format!("Dropping existing database '{dest_db_name}' with (FORCE)..."),
                );
                drop_database(&mut admin_client, dest_db_name)?;
            }
        }
    }

    if !db_exists || conflict_policy == ConflictPolicy::Replace {
        push_step(
            &mut Vec::new(),
            emit,
            format!("Creating database '{dest_db_name}' on target..."),
        );

        let create_query = format!(
            "CREATE DATABASE \"{}\" TEMPLATE template0",
            escape_ident(dest_db_name)
        );
        admin_client
            .batch_execute(&create_query)
            .context("failed to create destination database")?;
    }

    let dest_url = with_connect_timeout_param(&target_url(dest_base_url, dest_db_name)?)?;
    push_step(
        &mut Vec::new(),
        emit,
        format!(
            "Starting pg_restore to {}:{} / {}...",
            parsed.host, parsed.port, dest_db_name
        ),
    );

    let restore_result = match backend {
        PgToolBackend::Native { .. } => {
            let mut command = std::process::Command::new("pg_restore");
            command.args([
                "--dbname",
                &dest_url,
                "--no-owner",
                "--no-privileges",
                "--exit-on-error",
            ]);
            if tablespace_flatten {
                command.arg("--no-tablespaces");
            }
            command
                .arg(&dump_path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .context("failed to execute pg_restore")
        }
        PgToolBackend::Docker { image } => (|| -> Result<std::process::Output> {
            docker_pull_silent(image)?;
            let file = std::fs::File::open(&dump_path)
                .context("failed to open dump file for pg_restore")?;
            let mut args = vec![
                "run",
                "--rm",
                "-i",
                "--network",
                "host",
                image.as_str(),
                "pg_restore",
                "--dbname",
                &dest_url,
                "--no-owner",
                "--no-privileges",
                "--exit-on-error",
            ];
            if tablespace_flatten {
                args.push("--no-tablespaces");
            }
            std::process::Command::new("docker")
                .args(&args)
                .stdin(file)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .context("failed to execute docker pg_restore")
        })(),
        PgToolBackend::NotFound => unreachable!(),
    };

    let restore_output = match restore_result {
        Ok(output) => output,
        Err(error) => {
            let cleanup = drop_database(&mut admin_client, dest_db_name);
            return Err(combine_restore_cleanup_error(error, cleanup));
        }
    };

    if !restore_output.status.success() {
        let stderr = String::from_utf8_lossy(&restore_output.stderr);
        let error = anyhow::anyhow!("pg_restore failed: {}", stderr.trim());
        let cleanup = drop_database(&mut admin_client, dest_db_name);
        return Err(combine_restore_cleanup_error(error, cleanup));
    }

    let restore_stderr = String::from_utf8_lossy(&restore_output.stderr);
    for line in restore_stderr.lines() {
        let line = line.trim();
        if !line.is_empty() {
            push_step(&mut Vec::new(), emit, format!("pg_restore: {line}"));
        }
    }

    push_step(
        &mut Vec::new(),
        emit,
        format!("Restore to '{dest_db_name}' completed successfully."),
    );

    Ok(ProvisionFullOutcome {
        database_name: dest_db_name.to_owned(),
        application_name: dest_db_name.to_owned(),
        role_name: parsed.username,
        database_connection_string: dest_url,
        database_created: true,
        role_created: false,
        extra_username: None,
        extra_connection_string: None,
        extra_role_created: None,
        extra_grants_applied: None,
    })
}

pub fn restore_instance_with_progress<F>(
    input_path: &std::path::Path,
    decrypt_key: &[u8],
    dest_base_url: &str,
    backend: &PgToolBackend,
    on_conflict: ConflictPolicy,
    emit: &mut F,
) -> Result<Vec<ProvisionFullOutcome>>
where
    F: FnMut(String),
{
    let contents = read_instance_backup(input_path, decrypt_key)?;
    let dumps = contents.dumps;
    let globals = contents.globals;
    emit("Restoring cluster roles and memberships...".to_owned());
    restore_globals(dest_base_url, &globals, backend)?;
    let total = dumps.len();
    let staging = tempfile::tempdir().context("failed to create restore staging directory")?;
    let mut outcomes = Vec::with_capacity(total);
    for (index, (database_name, dump)) in dumps.into_iter().enumerate() {
        emit(format!(
            "Restoring database {}/{}: {}",
            index + 1,
            total,
            database_name
        ));
        let result = (|| -> Result<ProvisionFullOutcome> {
            let encrypted_path = staging.path().join(format!("{index}.pgdump.enc"));
            create_encrypted_dump(&dump, None, decrypt_key, &encrypted_path)?;
            restore_database_with_progress(
                &encrypted_path,
                decrypt_key,
                dest_base_url,
                &database_name,
                backend,
                on_conflict,
                contents.tablespace_mode == "flatten",
                emit,
            )
        })();
        match result {
            Ok(outcome) => outcomes.push(outcome),
            Err(error)
                if on_conflict == ConflictPolicy::Skip && error_string_database_exists(&error) =>
            {
                emit(format!(
                    "  [skip] '{}' already exists — leaving untouched",
                    database_name
                ));
            }
            Err(error) => {
                let cleanup = (|| -> Result<()> {
                    let mut admin = connect_client(dest_base_url)
                        .context("failed to connect for batch restore cleanup")?;
                    for outcome in &outcomes {
                        drop_database(&mut admin, &outcome.database_name)?;
                    }
                    Ok(())
                })();
                return Err(combine_restore_cleanup_error(error, cleanup));
            }
        }
    }
    Ok(outcomes)
}

fn run_tool(name: &str, args: impl IntoIterator<Item = &'static str>) -> Result<String> {
    let output = std::process::Command::new(name)
        .args(args)
        .output()
        .with_context(|| format!("{name} not found. Install postgresql-client tools."))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        anyhow::bail!("{name} returned empty output");
    }
    Ok(text)
}

pub fn mask_connection_string(value: &str) -> String {
    match Url::parse(value) {
        Ok(mut url) => {
            if !url.username().is_empty() {
                let _ = url.set_username("****");
            }
            let _ = url.set_password(Some("****"));
            url.to_string()
        }
        Err(_) => "<invalid connection string>".to_owned(),
    }
}

pub fn fetch_active_queries(url: &str) -> Result<Vec<ActiveQuery>> {
    let mut client = connect_client(url).context("failed to connect")?;
    let rows = client
        .query(
            "SELECT pid, usename, datname, COALESCE(client_addr::text, 'local'),
                    extract(epoch from (now() - query_start))::bigint AS duration,
                    state, query
             FROM pg_stat_activity
             WHERE state IS NOT NULL
               AND query NOT LIKE '%pg_stat_activity%'
               AND pid != pg_backend_pid()
             ORDER BY duration DESC",
            &[],
        )
        .context("failed to query pg_stat_activity")?;
    let queries = rows
        .iter()
        .map(|row| ActiveQuery {
            pid: row.get(0),
            user: row.get(1),
            database: row.get(2),
            client_addr: row.get(3),
            duration_secs: row.get(4),
            state: row.get(5),
            query: row.get(6),
        })
        .collect();
    let _ = client.close();
    Ok(queries)
}

pub fn kill_query(url: &str, pid: i32) -> Result<String> {
    let mut client = connect_client(url).context("failed to connect")?;
    let result = client
        .query_one("SELECT pg_terminate_backend($1)", &[&pid])
        .context("failed to terminate query")?;
    let terminated: bool = result.get(0);
    let _ = client.close();
    if terminated {
        Ok(format!("Query pid={pid} terminated successfully"))
    } else {
        Err(anyhow::anyhow!("Failed to terminate query pid={pid}"))
    }
}

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Probes a connection and returns the round-trip latency plus the server
/// version. A socket-level connect timeout bounds how long an unreachable host
/// can stall the check (the OS TCP timeout alone can take a minute or more).
pub fn check_connection_health(url: &str) -> Result<(u64, String)> {
    let start = std::time::Instant::now();
    let mut config: postgres::Config = url
        .parse()
        .context("connection string is not a valid DATABASE_URL")?;
    config.connect_timeout(CONNECT_TIMEOUT);
    let mut client = config.connect(NoTls).context("connection failed")?;
    let latency = start.elapsed().as_millis() as u64;
    let version = client
        .query_one("SELECT version()", &[])
        .context("failed to query version")?
        .get::<_, String>(0);
    let _ = client.close();
    Ok((latency, version))
}

/// Reports whether a health-check failure was caused by the connect timeout
/// rather than a refusal, DNS or auth problem. Matches the message produced by
/// the `tokio-postgres` connect timeout (e.g. "connection timed out").
pub fn is_connect_timeout(error: &str) -> bool {
    error.contains("connection timed out")
}

/// Rotates a role's password by connecting as an administrator (the instance
/// base URI user). The role must exist and the connecting user must have
/// privileges to alter it.
pub fn rotate_role_password(admin_cs: &str, role_name: &str, new_password: &str) -> Result<()> {
    let mut client = connect_client(admin_cs)
        .context("failed to connect to instance while rotating role password")?;
    let query = format!(
        "ALTER ROLE \"{}\" WITH PASSWORD '{}'",
        escape_ident(role_name),
        escape_literal(new_password)
    );
    client
        .batch_execute(&query)
        .context("failed to rotate role password")?;
    Ok(())
}

/// Rotates the password of `role_name` on the cluster of `admin_cs` and
/// returns a fresh connection string for `database_name` using the new
/// password. The previous password is invalidated immediately.
pub fn rotate_database_credential(
    admin_cs: &str,
    database_name: &str,
    role_name: &str,
    application_name: &str,
) -> Result<String> {
    let new_password = crypto::generate_password()?;
    rotate_role_password(admin_cs, role_name, &new_password)?;
    let parsed = parse_database_url(admin_cs)?;
    Ok(build_connection_string(
        &parsed.host,
        parsed.port,
        database_name,
        role_name,
        &new_password,
        application_name,
    ))
}

/// Rotates the password of the administrator user on `base_url` and returns
/// the base URI with the new password, preserving any query parameters.
pub fn rotate_base_url_password(base_url: &str) -> Result<String> {
    let parsed = parse_database_url(base_url)?;
    let new_password = crypto::generate_password()?;
    rotate_role_password(base_url, &parsed.username, &new_password)?;
    let mut url = Url::parse(base_url).context("invalid base DATABASE_URL")?;
    url.set_password(Some(&new_password))
        .map_err(|_| anyhow::anyhow!("failed to set password in base DATABASE_URL"))?;
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        INSTANCE_BACKUP_MAGIC, PgToolBackend, create_encrypted_dump, database_connection_string,
        detect_timescale_installed, docker_pull_silent, error_string_database_exists,
        extract_pg_major_version, is_connect_timeout, mask_connection_string, parse_database_url,
        read_instance_backup, resolve_docker_image, safe_filename_component,
    };

    #[test]
    fn parses_database_url() {
        let parsed = parse_database_url(
            "postgresql://admin:p%40ss@db.example.com:5432/shared?application_name=infra",
        )
        .expect("parse");
        assert_eq!(parsed.username, "admin");
        assert_eq!(parsed.host, "db.example.com");
        assert_eq!(parsed.port, 5432);
        assert_eq!(parsed.database, "shared");
    }

    #[test]
    fn derives_database_connection_string() {
        let derived = database_connection_string(
            "postgresql://admin:p%40ss@db.example.com:5432/shared?application_name=infra",
            "orders",
        )
        .expect("derive connection string");
        assert_eq!(
            derived,
            "postgresql://admin:p%40ss@db.example.com:5432/orders?application_name=infra"
        );
    }

    #[test]
    fn rejects_invalid_scheme_when_deriving_connection_string() {
        assert!(database_connection_string("http://example.com/db", "orders").is_err());
    }

    #[test]
    fn detects_connect_timeout_messages() {
        assert!(is_connect_timeout(
            "connection failed: connection timed out"
        ));
        assert!(!is_connect_timeout("connection refused"));
        assert!(!is_connect_timeout("password authentication failed"));
    }

    #[test]
    fn parses_url_without_password() {
        let parsed =
            parse_database_url("postgres://yugabyte@localhost:5433/nakama?sslmode=disable")
                .expect("parse url without password");
        assert_eq!(parsed.username, "yugabyte");
        assert_eq!(parsed.host, "localhost");
        assert_eq!(parsed.port, 5433);
        assert_eq!(parsed.database, "nakama");
    }

    #[test]
    fn masks_connection_string() {
        let masked = mask_connection_string(
            "postgresql://owner:secret@db.example.com/orders?application_name=orders",
        );
        assert!(masked.contains("****:****"));
    }

    #[test]
    fn extract_pg_major_version_16() {
        assert_eq!(
            extract_pg_major_version("PostgreSQL 16.4 on x86_64-pc-linux-gnu"),
            16
        );
    }

    #[test]
    fn extract_pg_major_version_14() {
        assert_eq!(
            extract_pg_major_version("PostgreSQL 14.12 on aarch64-unknown-linux-gnu"),
            14
        );
    }

    #[test]
    fn extract_pg_major_version_9() {
        assert_eq!(
            extract_pg_major_version("PostgreSQL 9.6.24 on x86_64-pc-linux-gnu"),
            9
        );
    }

    #[test]
    fn extract_pg_major_version_yugabytedb() {
        assert_eq!(
            extract_pg_major_version("YugabyteDB 11.2-YB-2.14.0.0 on x86_64"),
            11
        );
    }

    #[test]
    fn extract_pg_major_version_fallback() {
        assert_eq!(extract_pg_major_version("MySQL unknown"), 0);
    }

    #[test]
    fn extract_pg_major_version_empty_fallback() {
        assert_eq!(extract_pg_major_version(""), 0);
    }

    #[test]
    fn resolve_docker_image_native_returns_default() {
        let backend = PgToolBackend::Native {
            dump_ver: "15".into(),
            restore_ver: "15".into(),
        };
        let result = resolve_docker_image(&backend, None);
        assert_eq!(result, "postgres:18-alpine");
    }

    #[test]
    fn resolve_docker_image_docker_no_source_returns_default_image() {
        let backend = PgToolBackend::Docker {
            image: "my-custom:14".into(),
        };
        let result = resolve_docker_image(&backend, None);
        assert_eq!(result, "my-custom:14");
    }

    #[test]
    fn resolve_docker_image_docker_with_invalid_source_returns_default() {
        let backend = PgToolBackend::Docker {
            image: "postgres:16-alpine".into(),
        };
        let result = resolve_docker_image(&backend, Some("postgres://invalid:0/postgres"));
        assert_eq!(result, "postgres:16-alpine");
    }

    #[test]
    fn detect_timescale_installed_invalid_connection_returns_false() {
        assert!(!detect_timescale_installed("postgres://invalid:0/postgres"));
    }

    #[test]
    fn migrate_database_not_found_fails_fast() {
        let err = super::migrate_database_with_progress(
            "postgres://user:pass@localhost:0/source",
            "postgres://user:pass@localhost:0/dest",
            "test_db",
            &PgToolBackend::NotFound,
            &mut |_| {},
        )
        .expect_err("should fail with NotFound");

        let msg = err.to_string();
        assert!(
            msg.contains("pg_dump/pg_restore not found") || msg.contains("Install PostgreSQL"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn check_pg_tools_returns_something() {
        let result = super::check_pg_tools();
        match result {
            PgToolBackend::Native {
                dump_ver,
                restore_ver,
            } => {
                assert!(!dump_ver.is_empty());
                assert!(!restore_ver.is_empty());
            }
            PgToolBackend::Docker { image } => {
                assert!(!image.is_empty());
            }
            PgToolBackend::NotFound => {
                // acceptable on minimal CI environments
            }
        }
    }

    #[test]
    fn extract_pg_major_version_from_pg_dump_output_16() {
        assert_eq!(extract_pg_major_version("pg_dump (PostgreSQL) 16.4"), 16);
    }

    #[test]
    fn extract_pg_major_version_from_pg_dump_output_14() {
        assert_eq!(extract_pg_major_version("pg_dump (PostgreSQL) 14.12"), 14);
    }

    #[test]
    fn check_version_warning_native_mismatch() {
        let backend = PgToolBackend::Native {
            dump_ver: "pg_dump (PostgreSQL) 14.12".into(),
            restore_ver: "pg_restore (PostgreSQL) 14.12".into(),
        };
        // Returns warning when source version differs from pg_dump version
        // We can't actually connect to a DB, but with invalid URL it returns None
        let result = super::check_version_warning("postgres://invalid:0/test", &backend);
        assert!(result.is_none());
    }

    #[test]
    fn check_version_warning_docker_returns_none_even_with_invalid() {
        let backend = PgToolBackend::Docker {
            image: "postgres:16-alpine".into(),
        };
        let result = super::check_version_warning("postgres://invalid:0/test", &backend);
        assert!(result.is_none());
    }

    #[test]
    fn docker_pull_silent_nonexistent_image_gives_clear_error() {
        let result = docker_pull_silent("postgres:nonexistent-tag-that-does-not-exist-v999");
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("failed to pull Docker image")
                        || msg.contains("not found")
                        || msg.contains("docker pull"),
                    "unclear error message: {msg}"
                );
                // Should mention the image name
                assert!(
                    msg.contains("nonexistent-tag-that-does-not-exist-v999"),
                    "error should reference the image name: {msg}"
                );
            }
            Ok(()) => {
                // If it somehow succeeds (e.g. someone actually published this tag),
                // that's fine — the test shouldn't fail
            }
        }
    }

    #[test]
    fn docker_pull_silent_already_cached_does_not_pollute_stderr() {
        // Pull once to ensure the image is cached — if this fails (e.g. Windows
        // cannot pull Linux images, or Docker is not installed), skip the test.
        let image = "postgres:16-alpine";
        if docker_pull_silent(image).is_err() {
            return;
        }
        // Run a docker command that would have triggered a pull on stderr
        let output = std::process::Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                "host",
                image,
                "pg_dump",
                "--version",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        match output {
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                assert!(
                    !stderr.contains("Unable to find image"),
                    "stderr should not contain Docker pull messages after explicit pull: {stderr}"
                );
                assert!(
                    !stderr.contains("Pulling from"),
                    "stderr should not contain Docker pull progress after explicit pull: {stderr}"
                );
            }
            Err(_) => {
                // Docker not available — skip assertion
            }
        }
    }

    #[test]
    fn encrypted_dump_round_trip_without_metadata() {
        let key = b"01234567890123456789012345678901";
        let dump_data = b"this is some pgdump data";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pgdump.enc");

        super::create_encrypted_dump(dump_data, None, key, &path).unwrap();
        let (meta, decrypted) = super::read_encrypted_dump(&path, key).unwrap();

        assert!(meta.is_none());
        assert_eq!(decrypted, dump_data);
    }

    #[test]
    fn encrypted_dump_round_trip_with_metadata() {
        let key = b"01234567890123456789012345678901";
        let dump_data = b"pgdump binary content here";
        let metadata = crate::models::BackupMetadata {
            machine_id: "uuid-v7-123".into(),
            hostname: "myhost".into(),
            instance_name: "production".into(),
            database_name: "orders".into(),
            application_name: "orders-api".into(),
            engine: "postgresql".into(),
            timestamp: "2026-07-08T12:00:00Z".into(),
            version: "0.1.0".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_with_meta.pgdump.enc");

        super::create_encrypted_dump(dump_data, Some(&metadata), key, &path).unwrap();
        let (meta, decrypted) = super::read_encrypted_dump(&path, key).unwrap();

        let meta = meta.expect("metadata should be present");
        assert_eq!(meta.machine_id, "uuid-v7-123");
        assert_eq!(meta.hostname, "myhost");
        assert_eq!(meta.instance_name, "production");
        assert_eq!(meta.database_name, "orders");
        assert_eq!(meta.application_name, "orders-api");
        assert_eq!(meta.engine, "postgresql");
        assert_eq!(meta.timestamp, "2026-07-08T12:00:00Z");
        assert_eq!(meta.version, "0.1.0");
        assert_eq!(decrypted, dump_data);
    }

    #[test]
    fn encrypted_dump_with_metadata_backward_compat() {
        // Legacy file (no magic) should still be readable as (None, data)
        let key = b"01234567890123456789012345678901";
        let dump_data = b"legacy format dump bytes";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.pgdump.enc");

        super::create_encrypted_dump(dump_data, None, key, &path).unwrap();
        let (meta, decrypted) = super::read_encrypted_dump(&path, key).unwrap();

        assert!(meta.is_none(), "legacy file should have no metadata");
        assert_eq!(decrypted, dump_data);
    }

    #[test]
    fn encrypted_dump_rejects_truncated_file() {
        let key = b"01234567890123456789012345678901";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.pgdump.enc");
        // Write a file that's too small
        std::fs::write(&path, [1u8, 2, 3]).unwrap();
        let result = super::read_encrypted_dump(&path, key);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("too small"),
            "expected 'too small' error, got: {err}"
        );
    }

    #[test]
    fn instance_backup_round_trip_preserves_database_entries() {
        let key = b"01234567890123456789012345678901";
        let manifest = serde_json::json!({
            "format": "DBP2",
            "source_instance": "source",
            "source_version": "PostgreSQL 17.0",
            "created_at": "2026-01-01T00:00:00Z",
            "databases": ["accounts", "billing"],
            "includes_globals": true,
            "include_role_passwords": false,
            "tablespace_mode": "flatten"
        });
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let mut payload = Vec::new();
        payload.extend_from_slice(&INSTANCE_BACKUP_MAGIC);
        payload.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        payload.extend_from_slice(&manifest);
        payload.extend_from_slice(&2u32.to_le_bytes());
        for (name, dump) in [
            ("accounts", b"accounts dump".as_slice()),
            ("billing", b"billing dump".as_slice()),
        ] {
            payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
            payload.extend_from_slice(name.as_bytes());
            payload.extend_from_slice(&(dump.len() as u64).to_le_bytes());
            payload.extend_from_slice(dump);
        }
        payload.extend_from_slice(&4u64.to_le_bytes());
        payload.extend_from_slice(b"role");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance.cluster.pgdump.enc");
        super::create_encrypted_dump(&payload, None, key, &path).unwrap();

        let contents = read_instance_backup(&path, key).unwrap();
        assert_eq!(contents.databases, ["accounts", "billing"]);
        assert_eq!(contents.dumps[0].1, b"accounts dump");
        assert_eq!(contents.dumps[1].1, b"billing dump");
        assert_eq!(contents.globals, b"role");
    }

    #[test]
    fn encrypted_dump_rejects_wrong_key() {
        let key = b"01234567890123456789012345678901";
        let wrong_key = b"11111111111111111111111111111111";
        let dump_data = b"some data";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrong_key.pgdump.enc");

        super::create_encrypted_dump(dump_data, None, key, &path).unwrap();
        let result = super::read_encrypted_dump(&path, wrong_key);
        assert!(result.is_err());
    }

    #[test]
    fn backup_metadata_does_not_corrupt_dump_data() {
        let key = b"01234567890123456789012345678901";
        let dump_data =
            b"some actual pgdump custom format content with \x00 bytes and \xFF special chars";
        let metadata = crate::models::BackupMetadata {
            machine_id: "m".into(),
            hostname: "h".into(),
            instance_name: "i".into(),
            database_name: "d".into(),
            application_name: "a".into(),
            engine: "postgresql".into(),
            timestamp: "t".into(),
            version: "v".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corruption_check.pgdump.enc");

        // Write with metadata
        super::create_encrypted_dump(dump_data, Some(&metadata), key, &path).unwrap();
        // Read back
        let (_, decrypted) = super::read_encrypted_dump(&path, key).unwrap();
        // Verify dump bytes are bit-exact
        assert_eq!(
            decrypted, dump_data,
            "dump data with metadata must be bit-exact identical to input"
        );
    }

    #[test]
    fn multiple_backup_files_with_different_metadata() {
        let key = b"01234567890123456789012345678901";
        let dir = tempfile::tempdir().unwrap();

        let meta1 = crate::models::BackupMetadata {
            machine_id: "m1".into(),
            hostname: "h1".into(),
            instance_name: "prod".into(),
            database_name: "db1".into(),
            application_name: "app1".into(),
            engine: "postgresql".into(),
            timestamp: "t1".into(),
            version: "v1".into(),
        };
        let meta2 = crate::models::BackupMetadata {
            machine_id: "m2".into(),
            hostname: "h2".into(),
            instance_name: "staging".into(),
            database_name: "db2".into(),
            application_name: "app2".into(),
            engine: "postgresql".into(),
            timestamp: "t2".into(),
            version: "v2".into(),
        };

        let path1 = dir.path().join("backup1.pgdump.enc");
        let path2 = dir.path().join("backup2.pgdump.enc");

        super::create_encrypted_dump(b"data1", Some(&meta1), key, &path1).unwrap();
        super::create_encrypted_dump(b"data2", Some(&meta2), key, &path2).unwrap();

        let (m1, d1) = super::read_encrypted_dump(&path1, key).unwrap();
        let (m2, d2) = super::read_encrypted_dump(&path2, key).unwrap();

        assert_eq!(m1.unwrap().instance_name, "prod");
        assert_eq!(m2.unwrap().instance_name, "staging");
        assert_eq!(d1, b"data1");
        assert_eq!(d2, b"data2");
    }

    #[test]
    fn error_string_database_exists_positive() {
        let error = anyhow::anyhow!("database 'my_db' already exists on the target cluster");
        assert!(error_string_database_exists(&error));
    }

    #[test]
    fn error_string_database_exists_negative() {
        let error = anyhow::anyhow!("connection refused");
        assert!(!error_string_database_exists(&error));
    }

    #[test]
    fn is_instance_backup_detects_dbp2() {
        let key = b"01234567890123456789012345678901";
        let manifest = serde_json::json!({
            "format": "DBP2",
            "source_instance": "src",
            "source_version": "17",
            "created_at": "now",
            "databases": ["db1"],
            "includes_globals": false,
            "include_role_passwords": false,
            "tablespace_mode": "flatten"
        });
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let mut payload = Vec::new();
        payload.extend_from_slice(&INSTANCE_BACKUP_MAGIC);
        payload.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        payload.extend_from_slice(&manifest);
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&(3u32).to_le_bytes());
        payload.extend_from_slice(b"db1");
        payload.extend_from_slice(&4u64.to_le_bytes());
        payload.extend_from_slice(b"data");
        payload.extend_from_slice(&0u64.to_le_bytes()); // empty globals
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cluster.pgdump.enc");
        super::create_encrypted_dump(&payload, None, key, &path).unwrap();
        assert!(super::is_instance_backup(&path, key).unwrap());
    }

    #[test]
    fn is_instance_backup_rejects_dbp1() {
        let key = b"01234567890123456789012345678901";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.pgdump.enc");
        super::create_encrypted_dump(b"some dump data", None, key, &path).unwrap();
        assert!(!super::is_instance_backup(&path, key).unwrap());
    }

    #[test]
    fn safe_filename_component_removes_special_chars() {
        assert_eq!(safe_filename_component("hello"), "hello");
        assert_eq!(safe_filename_component("my instance!"), "my_instance_");
        assert_eq!(safe_filename_component("prod/db/1"), "prod_db_1");
        assert_eq!(safe_filename_component(""), "unnamed");
        assert_eq!(safe_filename_component("a.b-c_d"), "a.b-c_d");
        assert_eq!(safe_filename_component("!@#$"), "____");
    }

    #[test]
    fn instance_backup_contents_has_all_metadata_fields() {
        let key = b"01234567890123456789012345678901";
        let manifest = serde_json::json!({
            "format": "DBP2",
            "source_instance": "prod-cluster",
            "source_version": "PostgreSQL 17.0",
            "created_at": "2026-07-20T00:00:00Z",
            "databases": ["orders", "analytics"],
            "includes_globals": true,
            "include_role_passwords": false,
            "tablespace_mode": "flatten"
        });
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let mut payload = Vec::new();
        payload.extend_from_slice(&INSTANCE_BACKUP_MAGIC);
        payload.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        payload.extend_from_slice(&manifest);
        payload.extend_from_slice(&2u32.to_le_bytes());
        for (name, dump) in [
            ("orders", b"order_data" as &[u8]),
            ("analytics", b"analytics_data" as &[u8]),
        ] {
            payload.extend_from_slice(&(name.len() as u32).to_le_bytes());
            payload.extend_from_slice(name.as_bytes());
            payload.extend_from_slice(&(dump.len() as u64).to_le_bytes());
            payload.extend_from_slice(dump);
        }
        payload.extend_from_slice(&8u64.to_le_bytes());
        payload.extend_from_slice(b"some_sql");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meta.cluster.pgdump.enc");
        create_encrypted_dump(&payload, None, key, &path).unwrap();

        let contents = read_instance_backup(&path, key).unwrap();
        assert_eq!(contents.source_instance, "prod-cluster");
        assert_eq!(contents.source_version, "PostgreSQL 17.0");
        assert!(contents.includes_globals);
        assert!(!contents.include_role_passwords);
        assert_eq!(contents.tablespace_mode, "flatten");
        assert_eq!(contents.databases, ["orders", "analytics"]);
        assert!(!contents.globals.is_empty());
    }

    #[test]
    fn database_backup_encrypts_and_decrypts_roundtrip() {
        let key = b"01234567890123456789012345678901";
        let dump_data = b"pgdump custom format data \x00 with nulls";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup.pgdump.enc");
        create_encrypted_dump(dump_data, None, key, &path).unwrap();

        let path2 = dir.path().join("backup.copy.pgdump.enc");
        std::fs::rename(&path, &path2).unwrap();
        create_encrypted_dump(dump_data, None, key, &path).unwrap();

        let (meta, data) = super::read_encrypted_dump(&path, key).unwrap();
        assert!(meta.is_none());
        assert_eq!(data, dump_data);
    }
}
