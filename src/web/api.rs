use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{FromRequestParts, Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::crypto;
use crate::models::{
    BackupConfig, BackupMetadata, ConflictPolicy, EncryptedValue, ExtraUserProvisionOutcome,
    PgToolBackend, ProvisionFullOutcome, ProvisionFullRequest, ProvisionOutcome,
    SavedConnectionRecord, TablespaceMode,
};
use crate::postgres;
use crate::storage::backup_directory;
use crate::validation::{normalize_application_name, validate_database_name};
use crate::web::ops::{OperationResult, OperationStatus};
use crate::web::state::{HealthInfo, WebState};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{error:#}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Session extractor
// ---------------------------------------------------------------------------

pub struct Session {
    pub key: Arc<Zeroizing<Vec<u8>>>,
    pub token: String,
}

impl FromRequestParts<Arc<WebState>> for Session {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<WebState>,
    ) -> Result<Self, Self::Rejection> {
        let token = parse_bearer(parts.headers.get(header::AUTHORIZATION))
            .ok_or_else(|| ApiError::unauthorized("missing or malformed Authorization header"))?;
        let key = state
            .session_key(&token)
            .ok_or_else(|| ApiError::unauthorized("session expired or invalid"))?;
        Ok(Session { key, token })
    }
}

fn parse_bearer(auth: Option<&header::HeaderValue>) -> Option<String> {
    let auth = auth?;
    let value = auth.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    Some(token.trim().to_owned())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

enum ConnectionKind {
    Db,
    User,
}

fn connection_kind(kind: &str) -> Result<ConnectionKind, ApiError> {
    match kind {
        "db" => Ok(ConnectionKind::Db),
        "user" => Ok(ConnectionKind::User),
        _ => Err(ApiError::bad_request(
            "connection kind must be 'db' or 'user'",
        )),
    }
}

fn decrypt_connection(key: &[u8], encrypted: &EncryptedValue) -> Result<String> {
    let plaintext = crypto::decrypt(key, encrypted)?;
    String::from_utf8(plaintext.to_vec()).context("connection string is not valid UTF-8")
}

fn record_encrypted(record: &SavedConnectionRecord) -> &EncryptedValue {
    match record {
        SavedConnectionRecord::Database(r) => &r.encrypted,
        SavedConnectionRecord::ExtraUser(r) => &r.encrypted,
        SavedConnectionRecord::Instance { encrypted, .. } => encrypted,
    }
}

fn find_connection(
    storage: &crate::storage::Storage,
    kind: &ConnectionKind,
    id: i64,
) -> Result<SavedConnectionRecord> {
    match kind {
        ConnectionKind::Db => storage
            .list_provisioned_databases()?
            .into_iter()
            .find(|record| record.id == id)
            .map(SavedConnectionRecord::Database)
            .context("database connection not found"),
        ConnectionKind::User => storage
            .list_provisioned_extra_users()?
            .into_iter()
            .find(|record| record.id == id)
            .map(SavedConnectionRecord::ExtraUser)
            .context("extra user connection not found"),
    }
}

fn resolved_backend(state: &WebState, source_cs: Option<&str>) -> PgToolBackend {
    match &state.pg_tool_backend {
        PgToolBackend::Docker { .. } => PgToolBackend::Docker {
            image: postgres::resolve_docker_image(&state.pg_tool_backend, source_cs),
        },
        other => other.clone(),
    }
}

fn parse_conflict_policy(raw: &str) -> Result<ConflictPolicy, ApiError> {
    match raw {
        "fail" => Ok(ConflictPolicy::Fail),
        "skip" => Ok(ConflictPolicy::Skip),
        "replace" => Ok(ConflictPolicy::Replace),
        _ => Err(ApiError::bad_request(
            "conflict policy must be fail, skip or replace",
        )),
    }
}

fn backup_path_for(file: &str) -> Result<std::path::PathBuf, ApiError> {
    let path = std::path::Path::new(file);
    if path.file_name().and_then(|name| name.to_str()) != Some(file) {
        return Err(ApiError::bad_request("invalid backup file name"));
    }
    Ok(backup_directory().map_err(ApiError::from)?.join(file))
}

/// Persists a completed provision/migrate outcome to the catalog, mirroring the
/// TUI behavior. Skipped databases (restore with a skip policy, or migrate onto
/// an existing database) are not saved unless `save_even_if_not_created` is
/// set. Provision passes `true` because re-provisioning an existing database
/// still yields a fresh, rotated password and a valid connection string that
/// should be recorded.
fn save_provision_outcome(
    storage: &crate::storage::Storage,
    key: &[u8],
    instance_name: &str,
    outcome: &ProvisionFullOutcome,
    save_even_if_not_created: bool,
) -> Result<()> {
    if !outcome.database_created && !save_even_if_not_created {
        return Ok(());
    }
    let db_encrypted = crypto::encrypt(key, outcome.database_connection_string.as_bytes())?;
    let db_record = ProvisionOutcome {
        database_name: outcome.database_name.clone(),
        application_name: outcome.application_name.clone(),
        role_name: outcome.role_name.clone(),
        connection_string: outcome.database_connection_string.clone(),
        database_created: outcome.database_created,
        role_created: outcome.role_created,
    };
    storage.save_provisioned_database(instance_name, &db_record, &db_encrypted)?;

    if let (Some(extra_username), Some(extra_cs)) =
        (&outcome.extra_username, &outcome.extra_connection_string)
    {
        let extra_encrypted = crypto::encrypt(key, extra_cs.as_bytes())?;
        let extra_record = ExtraUserProvisionOutcome {
            database_name: outcome.database_name.clone(),
            username: extra_username.clone(),
            application_name: outcome.application_name.clone(),
            connection_string: extra_cs.clone(),
            role_created: outcome.extra_role_created.unwrap_or(false),
            grants_applied: outcome.extra_grants_applied.unwrap_or(false),
        };
        storage.save_provisioned_extra_user(instance_name, &extra_record, &extra_encrypted)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct InitRequest {
    password: String,
    confirm: String,
}

#[derive(Deserialize)]
struct UnlockRequest {
    password: String,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SourceRef {
    Db { id: i64 },
    User { id: i64 },
    Instance { name: String },
}

#[derive(Deserialize)]
struct ProvisionRequest {
    instance_name: String,
    database_name: String,
    application_name: Option<String>,
    extra_username: Option<String>,
    extra_application_name: Option<String>,
    #[serde(default = "default_true")]
    dedicated_owner: bool,
}

#[derive(Deserialize)]
struct MigrateRequest {
    source: SourceRef,
    dest_instance: String,
    dest_db_name: String,
    #[serde(default)]
    replace_existing: bool,
}

#[derive(Deserialize)]
struct BackupRequest {
    source: SourceRef,
    #[serde(default)]
    databases: Vec<String>,
    #[serde(default = "default_true")]
    include_globals: bool,
    #[serde(default)]
    include_role_passwords: bool,
    #[serde(default = "default_true")]
    flatten_tablespaces: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct DiscoverRequest {
    source: SourceRef,
}

#[derive(Deserialize)]
struct RestorePreviewRequest {
    file: String,
}

#[derive(Deserialize)]
struct RestoreRequest {
    file: String,
    dest_instance: String,
    dest_db_name: Option<String>,
    conflict_policy: Option<String>,
}

#[derive(Deserialize)]
struct NameRequest {
    name: String,
}

#[derive(Deserialize)]
struct InstanceRequest {
    name: String,
    url: String,
}

#[derive(Deserialize)]
struct AdoptRequest {
    instance_name: String,
    database_name: String,
    application_name: Option<String>,
}

#[derive(Deserialize)]
struct QueryTablesRequest {
    source: SourceRef,
}

#[derive(Deserialize)]
struct QueryDataRequest {
    source: SourceRef,
    schema: String,
    table: String,
    offset: i64,
}

#[derive(Deserialize)]
struct QueryRunRequest {
    source: SourceRef,
    sql: String,
    #[serde(default = "default_true")]
    read_only: bool,
}

// ---------------------------------------------------------------------------
// Serialized responses
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DashboardDb {
    kind: &'static str,
    id: i64,
    database_name: String,
    application_name: String,
    role_or_username: String,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    database_created: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role_created: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grants_applied: Option<bool>,
}

#[derive(Serialize)]
struct DashboardInstance {
    name: String,
    host: Option<String>,
    port: Option<u16>,
    base_database: Option<String>,
    health: HealthInfo,
    databases: Vec<DashboardDb>,
}

#[derive(Serialize)]
struct DashboardTotals {
    databases: usize,
    extra_users: usize,
}

#[derive(Serialize)]
struct Dashboard {
    instances: Vec<DashboardInstance>,
    totals: DashboardTotals,
}

#[derive(Serialize)]
struct DiscoveredDb {
    name: String,
    owner: String,
    encoding: String,
    tablespace: String,
    connection_limit: i32,
}

#[derive(Serialize)]
struct BackupFileInfo {
    filename: String,
    size: u64,
    modified: String,
}

#[derive(Serialize)]
struct ConnectionString {
    connection_string: String,
}

#[derive(Serialize)]
struct InstanceInfo {
    name: String,
    host: String,
    port: u16,
    database: String,
}

#[derive(Serialize)]
struct TokenResponse {
    token: String,
}

#[derive(Serialize)]
struct QueryTableInfo {
    schema: String,
    name: String,
    kind: String,
    rows_estimate: i64,
    size_bytes: i64,
}

#[derive(Serialize)]
struct QueryRowsResponse {
    columns: Vec<String>,
    rows: Vec<Vec<Option<String>>>,
    row_count: usize,
    duration_ms: u64,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers: static assets
// ---------------------------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}

async fn app_js() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/javascript; charset=utf-8")
        .body(Body::from(include_str!("static/app.js")))
        .expect("static app.js")
}

async fn style_css() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "text/css; charset=utf-8")
        .body(Body::from(include_str!("static/style.css")))
        .expect("static style.css")
}

async fn favicon() -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "image/svg+xml")
        .body(Body::from(include_str!("static/favicon.svg")))
        .expect("static favicon.svg")
}

// ---------------------------------------------------------------------------
// Handlers: session
// ---------------------------------------------------------------------------

async fn get_status(State(state): State<Arc<WebState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "initialized": state.is_initialized()?,
        "tool_backend": match &state.pg_tool_backend {
            PgToolBackend::Native { dump_ver, restore_ver } => json!({
                "mode": "native",
                "detail": format!("pg_dump {}, pg_restore {}", dump_ver.trim(), restore_ver.trim()),
            }),
            PgToolBackend::Docker { image } => json!({ "mode": "docker", "detail": image }),
            PgToolBackend::NotFound => json!({
                "mode": "not_found",
                "detail": "Install PostgreSQL client tools or Docker",
            }),
        },
    })))
}

async fn post_init(
    State(state): State<Arc<WebState>>,
    Json(req): Json<InitRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    if state.is_initialized()? {
        return Err(ApiError::bad_request("already initialized; unlock instead"));
    }
    if req.password.is_empty() {
        return Err(ApiError::bad_request("master password cannot be empty"));
    }
    if req.password != req.confirm {
        return Err(ApiError::bad_request(
            "password confirmation does not match",
        ));
    }
    let token = state.initialize(&req.password)?;
    Ok(Json(TokenResponse { token }))
}

async fn post_unlock(
    State(state): State<Arc<WebState>>,
    Json(req): Json<UnlockRequest>,
) -> Result<Json<TokenResponse>, ApiError> {
    if !state.is_initialized()? {
        return Err(ApiError::bad_request(
            "not initialized; complete first-run setup",
        ));
    }
    let token = state
        .create_session(&req.password)
        .map_err(|_| ApiError::unauthorized("invalid master password"))?;
    Ok(Json(TokenResponse { token }))
}

async fn post_lock(
    Session { token, .. }: Session,
    State(state): State<Arc<WebState>>,
) -> Json<Value> {
    state.lock(&token);
    Json(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// Handlers: dashboard
// ---------------------------------------------------------------------------

fn build_dashboard(state: &WebState, key: &[u8]) -> Result<Dashboard> {
    let storage = state.storage.lock().unwrap();
    let health = state.health.lock().unwrap();
    let mut instances: HashMap<String, DashboardInstance> = HashMap::new();

    for name in storage.list_instances()? {
        let mut instance = DashboardInstance {
            name: name.clone(),
            host: None,
            port: None,
            base_database: None,
            health: health
                .get(&format!("instance:{name}"))
                .cloned()
                .unwrap_or_default(),
            databases: Vec::new(),
        };
        if let Some(secret) = storage.load_instance_secret(&name)?
            && let Ok(cs) = decrypt_connection(key, &secret.encrypted)
            && let Ok(parsed) = postgres::parse_database_url(&cs)
        {
            instance.host = Some(parsed.host);
            instance.port = Some(parsed.port);
            instance.base_database = Some(parsed.database);
        }
        instances.insert(name, instance);
    }

    for record in storage.list_provisioned_databases()? {
        let instance = instances
            .entry(record.instance_name.clone())
            .or_insert_with(|| DashboardInstance {
                name: record.instance_name.clone(),
                host: None,
                port: None,
                base_database: None,
                health: HealthInfo::default(),
                databases: Vec::new(),
            });
        instance.health = health
            .get(&format!("conn:db:{}", record.id))
            .cloned()
            .unwrap_or_default();
        instance.databases.push(DashboardDb {
            kind: "db",
            id: record.id,
            database_name: record.database_name.clone(),
            application_name: record.application_name.clone(),
            role_or_username: record.role_name.clone(),
            created_at: record.created_at.clone(),
            database_created: Some(record.database_created),
            role_created: Some(record.role_created),
            grants_applied: None,
        });
    }

    for record in storage.list_provisioned_extra_users()? {
        let instance = instances
            .entry(record.instance_name.clone())
            .or_insert_with(|| DashboardInstance {
                name: record.instance_name.clone(),
                host: None,
                port: None,
                base_database: None,
                health: HealthInfo::default(),
                databases: Vec::new(),
            });
        instance.health = health
            .get(&format!("conn:user:{}", record.id))
            .cloned()
            .unwrap_or_default();
        instance.databases.push(DashboardDb {
            kind: "user",
            id: record.id,
            database_name: record.database_name.clone(),
            application_name: record.application_name.clone(),
            role_or_username: record.username.clone(),
            created_at: record.created_at.clone(),
            database_created: None,
            role_created: Some(record.role_created),
            grants_applied: Some(record.grants_applied),
        });
    }
    drop(health);
    drop(storage);

    let mut instances: Vec<DashboardInstance> = instances.into_values().collect();
    instances.sort_by(|a, b| a.name.cmp(&b.name));
    for instance in &mut instances {
        instance.databases.sort_by(|a, b| {
            (a.database_name.clone(), a.kind).cmp(&(b.database_name.clone(), b.kind))
        });
    }

    let totals = DashboardTotals {
        databases: instances
            .iter()
            .flat_map(|i| &i.databases)
            .filter(|d| d.kind == "db")
            .count(),
        extra_users: instances
            .iter()
            .flat_map(|i| &i.databases)
            .filter(|d| d.kind == "user")
            .count(),
    };

    Ok(Dashboard { instances, totals })
}

async fn get_dashboard(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
) -> Result<Json<Dashboard>, ApiError> {
    Ok(Json(build_dashboard(&state, key.as_ref().as_slice())?))
}

// ---------------------------------------------------------------------------
// Handlers: connections
// ---------------------------------------------------------------------------

async fn get_connection(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Path((kind, id)): Path<(String, i64)>,
) -> Result<Json<ConnectionString>, ApiError> {
    let kind = connection_kind(&kind)?;
    let storage = state.storage.lock().unwrap();
    let record = find_connection(&storage, &kind, id)?;
    let cs = decrypt_connection(key.as_ref().as_slice(), record_encrypted(&record))?;
    Ok(Json(ConnectionString {
        connection_string: cs,
    }))
}

async fn get_instance_connection(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Path(name): Path<String>,
) -> Result<Json<ConnectionString>, ApiError> {
    let storage = state.storage.lock().unwrap();
    let secret = storage
        .load_instance_secret(&name)?
        .ok_or_else(|| ApiError::not_found(format!("no saved base URI for instance '{name}'")))?;
    let cs = decrypt_connection(key.as_ref().as_slice(), &secret.encrypted)?;
    Ok(Json(ConnectionString {
        connection_string: cs,
    }))
}

/// Tests a single saved connection without touching the shared health cache.
/// Always responds 200 so the UI can read the per-connection outcome.
async fn post_connection_health(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Path((kind, id)): Path<(String, i64)>,
) -> Result<Json<HealthInfo>, ApiError> {
    let kind = connection_kind(&kind)?;
    let cs = {
        let storage = state.storage.lock().unwrap();
        let record = find_connection(&storage, &kind, id)?;
        decrypt_connection(key.as_ref().as_slice(), record_encrypted(&record))?
    };
    let probe = tokio::task::spawn_blocking(move || postgres::check_connection_health(&cs))
        .await
        .map_err(|error| ApiError::from(anyhow::anyhow!("health check task failed: {error}")))?;
    Ok(Json(match probe {
        Ok((latency_ms, version)) => HealthInfo {
            status: "ok".to_owned(),
            latency_ms: Some(latency_ms),
            version: Some(version),
            error: None,
            timed_out: false,
        },
        Err(error) => {
            let message = format!("{error:#}");
            HealthInfo {
                status: "error".to_owned(),
                latency_ms: None,
                version: None,
                error: Some(message.clone()),
                timed_out: postgres::is_connect_timeout(&message),
            }
        }
    }))
}

async fn put_connection_name(
    Session { .. }: Session,
    State(state): State<Arc<WebState>>,
    Path((kind, id)): Path<(String, i64)>,
    Json(req): Json<NameRequest>,
) -> Result<Json<Value>, ApiError> {
    let name = req.name.trim().to_owned();
    if name.is_empty() {
        return Err(ApiError::bad_request("application name cannot be empty"));
    }
    let storage = state.storage.lock().unwrap();
    match connection_kind(&kind)? {
        ConnectionKind::Db => storage.update_database_application_name(id, &name)?,
        ConnectionKind::User => storage.update_extra_user_application_name(id, &name)?,
    }
    Ok(Json(json!({ "ok": true })))
}

/// Rotates the password of a saved database owner or extra-user role on its
/// instance and updates the catalog with the new connection string.
async fn post_rotate_connection(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Path((kind, id)): Path<(String, i64)>,
) -> Result<Json<Value>, ApiError> {
    let kind = connection_kind(&kind)?;
    let (instance_name, database_name, application_name, role_name) = {
        let storage = state.storage.lock().unwrap();
        let record = find_connection(&storage, &kind, id)?;
        match &record {
            SavedConnectionRecord::Database(r) => (
                r.instance_name.clone(),
                r.database_name.clone(),
                r.application_name.clone(),
                r.role_name.clone(),
            ),
            SavedConnectionRecord::ExtraUser(r) => (
                r.instance_name.clone(),
                r.database_name.clone(),
                r.application_name.clone(),
                r.username.clone(),
            ),
            SavedConnectionRecord::Instance { .. } => unreachable!(),
        }
    };

    let base_url = {
        let storage = state.storage.lock().unwrap();
        let secret = storage
            .load_instance_secret(&instance_name)?
            .ok_or_else(|| {
                ApiError::bad_request(format!("no saved base URI for instance '{instance_name}'"))
            })?;
        decrypt_connection(key.as_ref().as_slice(), &secret.encrypted)?
    };

    let rotate_base = base_url.clone();
    let rotate_db = database_name.clone();
    let rotate_role = role_name.clone();
    let rotate_app = application_name.clone();
    let new_cs = tokio::task::spawn_blocking(move || {
        postgres::rotate_database_credential(&rotate_base, &rotate_db, &rotate_role, &rotate_app)
    })
    .await
    .map_err(|error| ApiError::from(anyhow::anyhow!("rotate task failed: {error}")))?
    .map_err(ApiError::from)?;

    let encrypted = crypto::encrypt(key.as_ref().as_slice(), new_cs.as_bytes())?;
    let storage = state.storage.lock().unwrap();
    match kind {
        ConnectionKind::Db => storage.update_database_connection(id, &encrypted)?,
        ConnectionKind::User => storage.update_extra_user_connection(id, &encrypted)?,
    }

    Ok(Json(json!({
        "database_name": database_name,
        "role_name": role_name,
        "application_name": application_name,
        "connection_string": new_cs,
    })))
}

/// Rotates the password of an instance's base URI user and updates the saved
/// base DATABASE_URL in the catalog.
async fn post_rotate_instance(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let base_url = {
        let storage = state.storage.lock().unwrap();
        let secret = storage.load_instance_secret(&name)?.ok_or_else(|| {
            ApiError::bad_request(format!("no saved base URI for instance '{name}'"))
        })?;
        decrypt_connection(key.as_ref().as_slice(), &secret.encrypted)?
    };
    let new_url =
        tokio::task::spawn_blocking(move || postgres::rotate_base_url_password(&base_url))
            .await
            .map_err(|error| ApiError::from(anyhow::anyhow!("rotate task failed: {error}")))?
            .map_err(ApiError::from)?;
    let encrypted = crypto::encrypt(key.as_ref().as_slice(), new_url.as_bytes())?;
    state
        .storage
        .lock()
        .unwrap()
        .save_instance_secret(&name, &encrypted)?;
    Ok(Json(json!({
        "name": name,
        "connection_string": new_url,
    })))
}

async fn delete_connection(
    Session { .. }: Session,
    State(state): State<Arc<WebState>>,
    Path((kind, id)): Path<(String, i64)>,
) -> Result<Json<Value>, ApiError> {
    let storage = state.storage.lock().unwrap();
    match connection_kind(&kind)? {
        ConnectionKind::Db => storage.delete_provisioned_database(id)?,
        ConnectionKind::User => storage.delete_provisioned_extra_user(id)?,
    }
    Ok(Json(json!({ "ok": true })))
}

/// Tests a single instance base URI on demand and caches the result under
/// `instance:{name}` so the dashboard badge shows the last known state.
/// Always responds 200 so the UI can read the per-instance outcome.
async fn post_instance_health(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Path(name): Path<String>,
) -> Result<Json<HealthInfo>, ApiError> {
    let cs = {
        let storage = state.storage.lock().unwrap();
        let secret = storage
            .load_instance_secret(&name)?
            .ok_or_else(|| ApiError::not_found(format!("instance '{name}' not found")))?;
        decrypt_connection(key.as_ref().as_slice(), &secret.encrypted)?
    };
    let probe = tokio::task::spawn_blocking(move || postgres::check_connection_health(&cs))
        .await
        .map_err(|error| ApiError::from(anyhow::anyhow!("health check task failed: {error}")))?;
    let info = match probe {
        Ok((latency_ms, version)) => HealthInfo {
            status: "ok".to_owned(),
            latency_ms: Some(latency_ms),
            version: Some(version),
            error: None,
            timed_out: false,
        },
        Err(error) => {
            let message = format!("{error:#}");
            HealthInfo {
                status: "error".to_owned(),
                latency_ms: None,
                version: None,
                error: Some(message.clone()),
                timed_out: postgres::is_connect_timeout(&message),
            }
        }
    };
    state
        .health
        .lock()
        .unwrap()
        .insert(format!("instance:{name}"), info.clone());
    Ok(Json(info))
}

// ---------------------------------------------------------------------------
// Handlers: instances
// ---------------------------------------------------------------------------

async fn post_instances(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Json(req): Json<InstanceRequest>,
) -> Result<Json<InstanceInfo>, ApiError> {
    let name = req.name.trim().to_owned();
    let raw_url = req.url.trim().to_owned();
    if name.is_empty() {
        return Err(ApiError::bad_request("instance name cannot be empty"));
    }
    if raw_url.is_empty() {
        return Err(ApiError::bad_request("base DATABASE_URL cannot be empty"));
    }
    let parsed = postgres::parse_database_url(&raw_url)?;
    let encrypted = crypto::encrypt(key.as_ref().as_slice(), raw_url.as_bytes())?;
    state
        .storage
        .lock()
        .unwrap()
        .save_instance_secret(&name, &encrypted)?;
    Ok(Json(InstanceInfo {
        name,
        host: parsed.host,
        port: parsed.port,
        database: parsed.database,
    }))
}

async fn delete_instance(
    Session { .. }: Session,
    State(state): State<Arc<WebState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut storage = state.storage.lock().unwrap();
    storage
        .delete_instance_cascade(&name)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "ok": true })))
}

/// Adopts an already-existing database on an instance into the catalog. The
/// connection string reuses the instance base URI credentials, pointing at the
/// target database. The database must exist and accept connections.
async fn post_adopt(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Json(req): Json<AdoptRequest>,
) -> Result<Json<Value>, ApiError> {
    let instance_name = req.instance_name.trim().to_owned();
    if instance_name.is_empty() {
        return Err(ApiError::bad_request("instance name cannot be empty"));
    }
    let database_name = req.database_name.trim().to_owned();
    validate_database_name(&database_name).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let application_name = normalize_application_name(
        &database_name,
        req.application_name.as_deref().unwrap_or(""),
    );

    let base_url = {
        let storage = state.storage.lock().unwrap();
        let secret = storage
            .load_instance_secret(&instance_name)?
            .ok_or_else(|| {
                ApiError::bad_request(format!("no saved base URI for instance '{instance_name}'"))
            })?;
        decrypt_connection(key.as_ref().as_slice(), &secret.encrypted)?
    };

    let parsed = postgres::parse_database_url(&base_url)?;
    let connection_string = postgres::database_connection_string(&base_url, &database_name)?;
    let probe_cs = connection_string.clone();
    let probe_db = database_name.clone();
    let probe_instance = instance_name.clone();
    tokio::task::spawn_blocking(move || postgres::check_connection_health(&probe_cs))
        .await
        .map_err(|error| ApiError::from(anyhow::anyhow!("adopt probe task failed: {error}")))?
        .map_err(|error| {
            ApiError::bad_request(format!(
                "cannot connect to existing database '{probe_db}' on instance '{probe_instance}': {error:#}"
            ))
        })?;

    let encrypted = crypto::encrypt(key.as_ref().as_slice(), connection_string.as_bytes())?;
    let outcome = ProvisionOutcome {
        database_name: database_name.clone(),
        application_name: application_name.clone(),
        role_name: parsed.username.clone(),
        connection_string: connection_string.clone(),
        database_created: false,
        role_created: false,
    };
    state.storage.lock().unwrap().save_provisioned_database(
        &instance_name,
        &outcome,
        &encrypted,
    )?;

    Ok(Json(json!({
        "database_name": database_name,
        "application_name": application_name,
        "role_name": parsed.username,
        "connection_string": connection_string,
    })))
}

async fn post_discover(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Json(req): Json<DiscoverRequest>,
) -> Result<Json<Vec<DiscoveredDb>>, ApiError> {
    let source_cs = resolve_source_cs(&state, key.as_ref().as_slice(), &req.source)?;
    let discovered = tokio::task::spawn_blocking(move || postgres::discover_databases(&source_cs))
        .await
        .map_err(|error| ApiError::from(anyhow::anyhow!("discover task failed: {error}")))?
        .map_err(ApiError::from)?;
    Ok(Json(
        discovered
            .into_iter()
            .map(|db| DiscoveredDb {
                name: db.name,
                owner: db.owner,
                encoding: db.encoding,
                tablespace: db.tablespace,
                connection_limit: db.connection_limit,
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// Handlers: query console
// ---------------------------------------------------------------------------

/// SQL errors are part of the normal flow of a query console, so the run
/// endpoint always responds 200 and reports failures in the `error` field.
fn query_rows_response(result: anyhow::Result<crate::models::SqlQueryResult>) -> QueryRowsResponse {
    match result {
        Ok(r) => {
            let row_count = r.rows.len();
            QueryRowsResponse {
                columns: r.columns,
                rows: r.rows,
                row_count,
                duration_ms: r.duration_ms,
                truncated: r.truncated,
                error: None,
            }
        }
        Err(e) => QueryRowsResponse {
            columns: Vec::new(),
            rows: Vec::new(),
            row_count: 0,
            duration_ms: 0,
            truncated: false,
            error: Some(format!("{e:#}")),
        },
    }
}

async fn post_query_tables(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Json(req): Json<QueryTablesRequest>,
) -> Result<Json<Vec<QueryTableInfo>>, ApiError> {
    let cs = resolve_source_cs(&state, key.as_ref().as_slice(), &req.source)?;
    let tables = tokio::task::spawn_blocking(move || postgres::list_database_tables(&cs))
        .await
        .map_err(|error| ApiError::from(anyhow::anyhow!("tables task failed: {error}")))?
        .map_err(ApiError::from)?;
    Ok(Json(
        tables
            .into_iter()
            .map(|t| QueryTableInfo {
                schema: t.schema,
                name: t.name,
                kind: t.kind,
                rows_estimate: t.rows_estimate,
                size_bytes: t.size_bytes,
            })
            .collect(),
    ))
}

async fn post_query_data(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Json(req): Json<QueryDataRequest>,
) -> Result<Json<QueryRowsResponse>, ApiError> {
    let schema = req.schema.trim().to_owned();
    let table = req.table.trim().to_owned();
    if schema.is_empty() || table.is_empty() {
        return Err(ApiError::bad_request("schema and table cannot be empty"));
    }
    if req.offset < 0 {
        return Err(ApiError::bad_request("offset cannot be negative"));
    }
    let cs = resolve_source_cs(&state, key.as_ref().as_slice(), &req.source)?;
    let result = tokio::task::spawn_blocking(move || {
        postgres::run_table_page(&cs, &schema, &table, req.offset)
    })
    .await
    .map_err(|error| ApiError::from(anyhow::anyhow!("table page task failed: {error}")))?;
    Ok(Json(query_rows_response(result)))
}

async fn post_query_run(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Json(req): Json<QueryRunRequest>,
) -> Result<Json<QueryRowsResponse>, ApiError> {
    let cs = resolve_source_cs(&state, key.as_ref().as_slice(), &req.source)?;
    let result =
        tokio::task::spawn_blocking(move || postgres::run_sql_query(&cs, &req.sql, req.read_only))
            .await
            .map_err(|error| ApiError::from(anyhow::anyhow!("query task failed: {error}")))?;
    Ok(Json(query_rows_response(result)))
}

// ---------------------------------------------------------------------------
// Source resolution
// ---------------------------------------------------------------------------

struct ResolvedSource {
    cs: String,
    instance_name: String,
    database_name: String,
    application_name: String,
    is_instance: bool,
}

fn resolve_source(
    state: &WebState,
    key: &[u8],
    source: &SourceRef,
) -> Result<ResolvedSource, ApiError> {
    let storage = state.storage.lock().unwrap();
    let resolved = match source {
        SourceRef::Db { id } => {
            let record = find_connection(&storage, &ConnectionKind::Db, *id)?;
            let SavedConnectionRecord::Database(record) = record else {
                unreachable!()
            };
            ResolvedSource {
                cs: decrypt_connection(key, &record.encrypted)?,
                instance_name: record.instance_name,
                database_name: record.database_name,
                application_name: record.application_name,
                is_instance: false,
            }
        }
        SourceRef::User { id } => {
            let record = find_connection(&storage, &ConnectionKind::User, *id)?;
            let SavedConnectionRecord::ExtraUser(record) = record else {
                unreachable!()
            };
            ResolvedSource {
                cs: decrypt_connection(key, &record.encrypted)?,
                instance_name: record.instance_name,
                database_name: record.database_name,
                application_name: record.application_name,
                is_instance: false,
            }
        }
        SourceRef::Instance { name } => {
            let secret = storage.load_instance_secret(name)?.ok_or_else(|| {
                ApiError::bad_request(format!("no saved base URI for instance '{name}'"))
            })?;
            let cs = decrypt_connection(key, &secret.encrypted)?;
            let parsed = postgres::parse_database_url(&cs)?;
            ResolvedSource {
                database_name: parsed.database,
                cs,
                instance_name: name.clone(),
                application_name: String::new(),
                is_instance: true,
            }
        }
    };
    drop(storage);
    Ok(resolved)
}

fn resolve_source_cs(state: &WebState, key: &[u8], source: &SourceRef) -> Result<String, ApiError> {
    Ok(resolve_source(state, key, source)?.cs)
}

// ---------------------------------------------------------------------------
// Handlers: operations
// ---------------------------------------------------------------------------

async fn post_provision(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Json(req): Json<ProvisionRequest>,
) -> Result<Json<Value>, ApiError> {
    let instance_name = req.instance_name.trim().to_owned();
    let database_name = req.database_name.trim().to_owned();
    if instance_name.is_empty() {
        return Err(ApiError::bad_request("instance name cannot be empty"));
    }
    validate_database_name(&database_name).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let extra_username = req
        .extra_username
        .map(|u| u.trim().to_owned())
        .filter(|u| !u.is_empty());
    if let Some(ref extra) = extra_username {
        validate_database_name(extra).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    }
    let extra_app_name = req
        .extra_application_name
        .map(|a| a.trim().to_owned())
        .filter(|a| !a.is_empty())
        .or_else(|| extra_username.as_ref().map(|_| database_name.clone()));

    let base_url = {
        let storage = state.storage.lock().unwrap();
        let secret = storage
            .load_instance_secret(&instance_name)?
            .ok_or_else(|| {
                ApiError::bad_request(format!("no saved base URI for instance '{instance_name}'"))
            })?;
        decrypt_connection(key.as_ref().as_slice(), &secret.encrypted)?
    };

    let request = ProvisionFullRequest {
        database_name: database_name.clone(),
        application_name: normalize_application_name(
            &database_name,
            req.application_name.as_deref().unwrap_or(""),
        ),
        extra_username,
        extra_application_name: extra_app_name,
        dedicated_owner: req.dedicated_owner,
    };

    let op_key = key.clone();
    let storage = state.storage.clone();
    let op_instance = instance_name.clone();
    let id = state.ops.spawn(
        format!("Starting provisioning for database '{database_name}'"),
        move |log| {
            let result =
                postgres::provision_full_with_progress(&base_url, &request, &mut |step| log(step))
                    .map_err(|e| e.to_string());
            match result {
                Ok(outcome) => {
                    let k = op_key.as_ref().as_slice();
                    let storage = storage.lock().unwrap();
                    if let Err(e) =
                        save_provision_outcome(&storage, k, &op_instance, &outcome, true)
                    {
                        return Err(format!(
                            "provisioning succeeded but failed to save catalog: {e:#}"
                        ));
                    }
                    Ok(OperationResult::Provision {
                        database_name: outcome.database_name,
                        application_name: outcome.application_name,
                        role_name: outcome.role_name,
                        connection_string: outcome.database_connection_string,
                        extra_username: outcome.extra_username,
                        extra_connection_string: outcome.extra_connection_string,
                    })
                }
                Err(e) => Err(e),
            }
        },
    );

    Ok(Json(json!({ "operation_id": id.to_string() })))
}

async fn post_migrate(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Json(req): Json<MigrateRequest>,
) -> Result<Json<Value>, ApiError> {
    let source_cs = resolve_source_cs(&state, key.as_ref().as_slice(), &req.source)?;
    let dest_instance = req.dest_instance.trim().to_owned();
    let dest_db_name = req.dest_db_name.trim().to_owned();
    if dest_instance.is_empty() {
        return Err(ApiError::bad_request(
            "destination instance cannot be empty",
        ));
    }
    if dest_db_name.is_empty() {
        return Err(ApiError::bad_request(
            "destination database name cannot be empty",
        ));
    }
    validate_database_name(&dest_db_name).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;

    let dest_base_url = {
        let storage = state.storage.lock().unwrap();
        let secret = storage
            .load_instance_secret(&dest_instance)?
            .ok_or_else(|| {
                ApiError::bad_request(format!("no saved base URI for instance '{dest_instance}'"))
            })?;
        decrypt_connection(key.as_ref().as_slice(), &secret.encrypted)?
    };

    let backend = resolved_backend(&state, Some(&source_cs));
    let op_key = key.clone();
    let storage = state.storage.clone();
    let op_instance = dest_instance.clone();
    let id = state.ops.spawn(
        format!("Starting migration to '{dest_db_name}' in {dest_instance}"),
        move |log| {
            if let Some(warning) = postgres::check_version_warning(&source_cs, &backend) {
                log(warning);
            }
            let result = postgres::migrate_database_with_progress(
                &source_cs,
                &dest_base_url,
                &dest_db_name,
                &backend,
                req.replace_existing,
                &mut |step| log(step),
            )
            .map_err(|e| e.to_string());
            match result {
                Ok(outcome) => {
                    let k = op_key.as_ref().as_slice();
                    let storage = storage.lock().unwrap();
                    if let Err(e) =
                        save_provision_outcome(&storage, k, &op_instance, &outcome, false)
                    {
                        return Err(format!(
                            "migration succeeded but failed to save catalog: {e:#}"
                        ));
                    }
                    Ok(OperationResult::Migrate {
                        database_name: outcome.database_name,
                        instance_name: op_instance,
                        connection_string: outcome.database_connection_string,
                    })
                }
                Err(e) => Err(e),
            }
        },
    );

    Ok(Json(json!({ "operation_id": id.to_string() })))
}

async fn post_backup(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Json(req): Json<BackupRequest>,
) -> Result<Json<Value>, ApiError> {
    let source = resolve_source(&state, key.as_ref().as_slice(), &req.source)?;

    let mut selected_databases = req.databases.clone();
    if source.is_instance && selected_databases.is_empty() {
        selected_databases = postgres::discover_databases(&source.cs)?
            .into_iter()
            .map(|db| db.name)
            .collect();
    }

    let config = BackupConfig {
        include_globals: req.include_globals,
        include_role_passwords: req.include_role_passwords,
        tablespace_mode: if req.flatten_tablespaces {
            TablespaceMode::Flatten
        } else {
            TablespaceMode::Preserve
        },
    };

    let backend = resolved_backend(&state, Some(&source.cs));
    let op_key = key.clone();
    let machine_id = state.machine_id.clone();
    let hostname = state.hostname.clone();
    let instance_name = source.instance_name.clone();
    let db_name = source.database_name.clone();
    let app_name = source.application_name.clone();
    let is_instance = source.is_instance;
    let src_cs = source.cs;

    let metadata = BackupMetadata {
        machine_id: machine_id.clone(),
        hostname: hostname.clone(),
        instance_name: instance_name.clone(),
        database_name: db_name.clone(),
        application_name: app_name.clone(),
        engine: "postgresql".to_owned(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };

    let id = state
        .ops
        .spawn(format!("Starting backup of '{db_name}'"), move |log| {
            if let Some(warning) = postgres::check_version_warning(&src_cs, &backend) {
                log(warning);
            }
            let output_dir = match backup_directory() {
                Ok(path) => path,
                Err(e) => return Err(format!("{e:#}")),
            };
            let k = op_key.as_ref().as_slice();
            let result = if is_instance {
                postgres::backup_instance_with_progress(
                    &src_cs,
                    k,
                    &output_dir,
                    &backend,
                    postgres::InstanceBackupContext {
                        instance_name: &instance_name,
                        machine_id: &machine_id,
                        hostname: &hostname,
                    },
                    &selected_databases,
                    &config,
                    &mut |step| log(step),
                )
            } else {
                postgres::backup_database_with_progress(
                    &src_cs,
                    k,
                    &output_dir,
                    &backend,
                    Some(&metadata),
                    &mut |step| log(step),
                )
            }
            .map_err(|e| e.to_string())?;
            Ok(OperationResult::Backup {
                file_path: result.file_path,
                database_name: result.database_name,
                database_names: result.database_names,
            })
        });

    Ok(Json(json!({ "operation_id": id.to_string() })))
}

async fn get_backups(
    Session { .. }: Session,
    State(_state): State<Arc<WebState>>,
) -> Result<Json<Vec<BackupFileInfo>>, ApiError> {
    Ok(Json(list_backup_files()))
}

fn list_backup_files() -> Vec<BackupFileInfo> {
    let Ok(dir) = backup_directory() else {
        return Vec::new();
    };
    if !dir.exists() {
        return Vec::new();
    }
    let mut files: Vec<BackupFileInfo> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "enc") {
                let filename = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("")
                    .to_owned();
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let modified = std::fs::metadata(&path)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| {
                        let duration = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                        let secs = duration.as_secs();
                        chrono::DateTime::from_timestamp(secs as i64, 0)
                            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                            .unwrap_or_else(|| "unknown".to_owned())
                    })
                    .unwrap_or_else(|| "unknown".to_owned());
                files.push(BackupFileInfo {
                    filename,
                    size,
                    modified,
                });
            }
        }
    }
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    files
}

async fn post_restore_preview(
    Session { key, .. }: Session,
    State(_state): State<Arc<WebState>>,
    Json(req): Json<RestorePreviewRequest>,
) -> Result<Json<Value>, ApiError> {
    let path = backup_path_for(&req.file)?;
    if !path.is_file() {
        return Err(ApiError::bad_request("backup file does not exist"));
    }
    let key = key.as_ref().as_slice().to_vec();
    if postgres::is_instance_backup(&path, &key)? {
        let contents = postgres::read_instance_backup(&path, &key)?;
        Ok(Json(json!({
            "type": "bundle",
            "databases": contents.databases,
            "source_instance": contents.source_instance,
            "source_version": contents.source_version,
            "includes_globals": contents.includes_globals,
            "include_role_passwords": contents.include_role_passwords,
            "tablespace_mode": contents.tablespace_mode,
        })))
    } else {
        let (metadata, _) = postgres::read_encrypted_dump(&path, &key)?;
        Ok(Json(json!({
            "type": "single",
            "metadata": metadata.map(|m| {
                json!({
                    "instance_name": m.instance_name,
                    "database_name": m.database_name,
                    "hostname": m.hostname,
                    "version": m.version,
                    "timestamp": m.timestamp,
                })
            }),
        })))
    }
}

async fn post_restore(
    Session { key, .. }: Session,
    State(state): State<Arc<WebState>>,
    Json(req): Json<RestoreRequest>,
) -> Result<Json<Value>, ApiError> {
    let path = backup_path_for(&req.file)?;
    if !path.is_file() {
        return Err(ApiError::bad_request("backup file does not exist"));
    }
    let dest_instance = req.dest_instance.trim().to_owned();
    if dest_instance.is_empty() {
        return Err(ApiError::bad_request(
            "destination instance cannot be empty",
        ));
    }
    let dest_base_url = {
        let storage = state.storage.lock().unwrap();
        let secret = storage
            .load_instance_secret(&dest_instance)?
            .ok_or_else(|| {
                ApiError::bad_request(format!("no saved base URI for instance '{dest_instance}'"))
            })?;
        decrypt_connection(key.as_ref().as_slice(), &secret.encrypted)?
    };

    let key = key.as_ref().as_slice().to_vec();
    let instance_bundle = postgres::is_instance_backup(&path, &key)?;
    let conflict_policy = match req.conflict_policy.as_deref() {
        Some(raw) => parse_conflict_policy(raw)?,
        None if instance_bundle => ConflictPolicy::Skip,
        None => ConflictPolicy::Fail,
    };

    let dest_db_name = req
        .dest_db_name
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_owned();
    if !instance_bundle {
        if dest_db_name.is_empty() {
            return Err(ApiError::bad_request(
                "destination database name cannot be empty",
            ));
        }
        validate_database_name(&dest_db_name)
            .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    }

    let backend = resolved_backend(&state, Some(&dest_base_url));
    let op_key = key.clone();
    let storage = state.storage.clone();
    let op_instance = dest_instance.clone();
    let start_msg = format!(
        "Starting restore of '{}' to '{dest_db_name}' in {dest_instance}",
        req.file
    );

    let id = state.ops.spawn(start_msg, move |log| {
        let k = op_key.as_slice();
        if instance_bundle {
            let outcomes = postgres::restore_instance_with_progress(
                &path,
                k,
                &dest_base_url,
                &backend,
                conflict_policy,
                &mut |step| log(step),
            )
            .map_err(|e| e.to_string())?;
            let restored: Vec<String> = outcomes
                .iter()
                .filter(|o| o.database_created)
                .map(|o| o.database_name.clone())
                .collect();
            let skipped: Vec<String> = outcomes
                .iter()
                .filter(|o| !o.database_created)
                .map(|o| o.database_name.clone())
                .collect();
            Ok(OperationResult::Restore { restored, skipped })
        } else {
            let outcome = postgres::restore_database_with_progress(
                &path,
                k,
                &dest_base_url,
                &dest_db_name,
                &backend,
                conflict_policy,
                false,
                &mut |step| log(step),
            )
            .map_err(|e| e.to_string())?;
            let storage = storage.lock().unwrap();
            if let Err(e) = save_provision_outcome(&storage, k, &op_instance, &outcome, false) {
                return Err(format!(
                    "restore succeeded but failed to save catalog: {e:#}"
                ));
            }
            Ok(OperationResult::Restore {
                restored: vec![outcome.database_name],
                skipped: Vec::new(),
            })
        }
    });

    Ok(Json(json!({ "operation_id": id.to_string() })))
}

// ---------------------------------------------------------------------------
// Handlers: operations polling
// ---------------------------------------------------------------------------

async fn get_operation(
    Session { .. }: Session,
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::bad_request("invalid operation id"))?;
    let Some(handle) = state.ops.get(id) else {
        return Err(ApiError::not_found("operation not found"));
    };
    let guard = handle.lock().unwrap();
    let status = if guard.status == OperationStatus::Running {
        "running"
    } else {
        "done"
    };
    let result = match &guard.result {
        Some(Ok(value)) => Some(json!({ "ok": true, "value": value })),
        Some(Err(error)) => Some(json!({ "ok": false, "error": error })),
        None => None,
    };
    Ok(Json(json!({
        "id": id.to_string(),
        "status": status,
        "logs": guard.logs,
        "result": result,
    })))
}

async fn delete_operation(
    Session { .. }: Session,
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::bad_request("invalid operation id"))?;
    state.ops.remove(id);
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/favicon.svg", get(favicon))
        .route("/api/status", get(get_status))
        .route("/api/init", post(post_init))
        .route("/api/unlock", post(post_unlock))
        .route("/api/lock", post(post_lock))
        .route("/api/dashboard", get(get_dashboard))
        .route(
            "/api/connections/{kind}/{id}",
            get(get_connection).delete(delete_connection),
        )
        .route(
            "/api/connections/{kind}/{id}/health",
            post(post_connection_health),
        )
        .route(
            "/api/connections/{kind}/{id}/rotate",
            post(post_rotate_connection),
        )
        .route(
            "/api/connections/{kind}/{id}/name",
            put(put_connection_name),
        )
        .route(
            "/api/connections/instance/{name}",
            get(get_instance_connection),
        )
        .route("/api/instances", post(post_instances))
        .route("/api/instances/{name}", delete(delete_instance))
        .route("/api/instances/{name}/health", post(post_instance_health))
        .route("/api/instances/{name}/rotate", post(post_rotate_instance))
        .route("/api/adopt", post(post_adopt))
        .route("/api/discover", post(post_discover))
        .route("/api/provision", post(post_provision))
        .route("/api/migrate", post(post_migrate))
        .route("/api/backup", post(post_backup))
        .route("/api/backups", get(get_backups))
        .route("/api/restore/preview", post(post_restore_preview))
        .route("/api/restore", post(post_restore))
        .route("/api/query/tables", post(post_query_tables))
        .route("/api/query/data", post(post_query_data))
        .route("/api/query/run", post(post_query_run))
        .route(
            "/api/operations/{id}",
            get(get_operation).delete(delete_operation),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use tempfile::tempdir;

    #[test]
    fn backup_path_rejects_traversal() {
        assert!(backup_path_for("../etc/passwd").is_err());
        assert!(backup_path_for("a/b.pgdump.enc").is_err());
        assert!(backup_path_for("..").is_err());
        assert!(backup_path_for("safe.pgdump.enc").is_ok());
    }

    #[test]
    fn conflict_policy_parsing() {
        assert!(matches!(
            parse_conflict_policy("fail").unwrap(),
            ConflictPolicy::Fail
        ));
        assert!(matches!(
            parse_conflict_policy("skip").unwrap(),
            ConflictPolicy::Skip
        ));
        assert!(matches!(
            parse_conflict_policy("replace").unwrap(),
            ConflictPolicy::Replace
        ));
        assert!(parse_conflict_policy("nope").is_err());
    }

    #[test]
    fn router_builds_without_route_conflicts() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let _app = router(state);
    }

    #[test]
    fn dashboard_groups_connections_under_instances() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";

        let inst_enc =
            crypto::encrypt(key, b"postgresql://admin:pw@db.example.com:5432/postgres").unwrap();
        storage.save_instance_secret("prod", &inst_enc).unwrap();

        let db_enc = crypto::encrypt(key, b"postgresql://o:p@h/orders").unwrap();
        storage
            .save_provisioned_database(
                "prod",
                &ProvisionOutcome {
                    database_name: "orders".into(),
                    application_name: "orders-api".into(),
                    role_name: "orders_owner".into(),
                    connection_string: "cs".into(),
                    database_created: true,
                    role_created: true,
                },
                &db_enc,
            )
            .unwrap();

        let user_enc = crypto::encrypt(key, b"postgresql://w:p@h/orders").unwrap();
        storage
            .save_provisioned_extra_user(
                "prod",
                &ExtraUserProvisionOutcome {
                    database_name: "orders".into(),
                    username: "orders_worker".into(),
                    application_name: "orders-worker".into(),
                    connection_string: "cs".into(),
                    role_created: true,
                    grants_applied: true,
                },
                &user_enc,
            )
            .unwrap();

        let state = WebState::from_storage(storage).unwrap();
        let dash = build_dashboard(&state, key).unwrap();

        assert_eq!(dash.instances.len(), 1);
        let inst = &dash.instances[0];
        assert_eq!(inst.name, "prod");
        assert_eq!(inst.host.as_deref(), Some("db.example.com"));
        assert_eq!(inst.port, Some(5432));
        assert_eq!(inst.base_database.as_deref(), Some("postgres"));
        assert_eq!(inst.databases.len(), 2);
        assert_eq!(inst.databases[0].kind, "db");
        assert_eq!(inst.databases[0].role_or_username, "orders_owner");
        assert_eq!(inst.databases[1].kind, "user");
        assert_eq!(inst.databases[1].role_or_username, "orders_worker");
        assert_eq!(dash.totals.databases, 1);
        assert_eq!(dash.totals.extra_users, 1);
    }

    #[test]
    fn dashboard_lists_instances_without_connections() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let inst_enc = crypto::encrypt(key, b"postgresql://u:p@localhost:5433/shared").unwrap();
        storage.save_instance_secret("dev", &inst_enc).unwrap();

        let state = WebState::from_storage(storage).unwrap();
        let dash = build_dashboard(&state, key).unwrap();
        assert_eq!(dash.instances.len(), 1);
        assert_eq!(dash.instances[0].name, "dev");
        assert_eq!(dash.instances[0].databases.len(), 0);
    }

    fn test_session(key: &[u8; 32]) -> Session {
        Session {
            key: Arc::new(Zeroizing::new(key.to_vec())),
            token: "test-token".to_owned(),
        }
    }

    #[tokio::test]
    async fn discover_returns_error_instead_of_panicking_on_blocking_connect() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let inst_enc = crypto::encrypt(key, b"postgresql://u:p@127.0.0.1:1/nope").unwrap();
        storage.save_instance_secret("dev", &inst_enc).unwrap();
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let request = DiscoverRequest {
            source: SourceRef::Instance {
                name: "dev".to_owned(),
            },
        };
        let result = post_discover(test_session(key), State(state), Json(request)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn adopt_returns_error_instead_of_panicking_on_blocking_connect() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let inst_enc = crypto::encrypt(key, b"postgresql://u:p@127.0.0.1:1/postgres").unwrap();
        storage.save_instance_secret("dev", &inst_enc).unwrap();
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let request = AdoptRequest {
            instance_name: "dev".to_owned(),
            database_name: "orders".to_owned(),
            application_name: None,
        };
        let result = post_adopt(test_session(key), State(state), Json(request)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn connection_health_reports_error_instead_of_panicking() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let enc = crypto::encrypt(key, b"postgresql://u:p@127.0.0.1:1/orders").unwrap();
        storage
            .save_provisioned_database(
                "prod",
                &ProvisionOutcome {
                    database_name: "orders".into(),
                    application_name: "orders".into(),
                    role_name: "u".into(),
                    connection_string: "cs".into(),
                    database_created: true,
                    role_created: true,
                },
                &enc,
            )
            .unwrap();
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let result =
            post_connection_health(test_session(key), State(state), Path(("db".to_owned(), 1)))
                .await
                .expect("handler returns health info");
        assert_eq!(result.0.status, "error");
        assert!(!result.0.timed_out);
    }

    #[tokio::test]
    async fn instance_health_reports_error_instead_of_panicking() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let inst_enc = crypto::encrypt(key, b"postgresql://u:p@127.0.0.1:1/postgres").unwrap();
        storage.save_instance_secret("dev", &inst_enc).unwrap();
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let result = post_instance_health(test_session(key), State(state), Path("dev".to_owned()))
            .await
            .expect("handler returns health info");
        assert_eq!(result.0.status, "error");
        assert!(!result.0.timed_out);
    }

    #[tokio::test]
    async fn instance_health_fails_for_unknown_instance() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let result =
            post_instance_health(test_session(key), State(state), Path("ghost".to_owned())).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn query_tables_fails_for_unreachable_source() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let inst_enc = crypto::encrypt(key, b"postgresql://u:p@127.0.0.1:1/postgres").unwrap();
        storage.save_instance_secret("dev", &inst_enc).unwrap();
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let request = QueryTablesRequest {
            source: SourceRef::Instance {
                name: "dev".to_owned(),
            },
        };
        let result = post_query_tables(test_session(key), State(state), Json(request)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn query_data_rejects_negative_offset() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let inst_enc = crypto::encrypt(key, b"postgresql://u:p@127.0.0.1:1/postgres").unwrap();
        storage.save_instance_secret("dev", &inst_enc).unwrap();
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let request = QueryDataRequest {
            source: SourceRef::Instance {
                name: "dev".to_owned(),
            },
            schema: "public".to_owned(),
            table: "orders".to_owned(),
            offset: -1,
        };
        let result = post_query_data(test_session(key), State(state), Json(request)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn query_run_reports_sql_errors_in_error_field() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let inst_enc = crypto::encrypt(key, b"postgresql://u:p@127.0.0.1:1/postgres").unwrap();
        storage.save_instance_secret("dev", &inst_enc).unwrap();
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let request = QueryRunRequest {
            source: SourceRef::Instance {
                name: "dev".to_owned(),
            },
            sql: "SELECT 1".to_owned(),
            read_only: true,
        };
        let result = post_query_run(test_session(key), State(state), Json(request))
            .await
            .expect("handler responds even when the connection fails");
        assert!(result.0.error.is_some());
        assert_eq!(result.0.row_count, 0);
    }

    #[tokio::test]
    async fn rotate_connection_returns_error_instead_of_panicking_on_blocking_connect() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let inst_enc = crypto::encrypt(key, b"postgresql://u:p@127.0.0.1:1/postgres").unwrap();
        storage.save_instance_secret("dev", &inst_enc).unwrap();
        let enc = crypto::encrypt(key, b"postgresql://u:p@127.0.0.1:1/orders").unwrap();
        storage
            .save_provisioned_database(
                "dev",
                &ProvisionOutcome {
                    database_name: "orders".into(),
                    application_name: "orders".into(),
                    role_name: "orders_owner".into(),
                    connection_string: "cs".into(),
                    database_created: true,
                    role_created: true,
                },
                &enc,
            )
            .unwrap();
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let result =
            post_rotate_connection(test_session(key), State(state), Path(("db".to_owned(), 1)))
                .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rotate_instance_returns_error_instead_of_panicking_on_blocking_connect() {
        let dir = tempdir().unwrap();
        let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
        let key = b"01234567890123456789012345678901";
        let inst_enc = crypto::encrypt(key, b"postgresql://u:p@127.0.0.1:1/postgres").unwrap();
        storage.save_instance_secret("dev", &inst_enc).unwrap();
        let state = Arc::new(WebState::from_storage(storage).unwrap());
        let result =
            post_rotate_instance(test_session(key), State(state), Path("dev".to_owned())).await;
        assert!(result.is_err());
    }
}
