use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

use cais::web::state::WebState;

fn build_app() -> (axum::Router, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(WebState::open_at(dir.path()).unwrap());
    (cais::web::api::router(state), dir)
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<&str>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let req = if let Some(body) = body {
        builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    };
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let collected = response.into_body().collect().await.unwrap();
    let value = serde_json::from_slice(&collected.to_bytes()).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn status_reports_tool_backend() {
    let (app, _dir) = build_app();
    let (status, body) = request(&app, "GET", "/api/status", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["tool_backend"].is_object());
}

#[tokio::test]
async fn workspaces_start_empty_and_support_full_lifecycle() {
    let (app, _dir) = build_app();

    let (status, body) = request(&app, "GET", "/api/workspaces", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);

    // Create via init.
    let (status, body) = request(
        &app,
        "POST",
        "/api/init",
        Some(r#"{"workspace":"default","password":"secret","confirm":"secret"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let token = body["token"].as_str().unwrap().to_owned();

    // Cannot remove a workspace with an active session.
    let (status, _) = request(&app, "DELETE", "/api/workspaces/default", None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Lock, then removal succeeds.
    let _ = request(&app, "POST", "/api/lock", Some("{}"), Some(&token)).await;
    let (status, _) = request(&app, "DELETE", "/api/workspaces/default", None, None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request(&app, "GET", "/api/workspaces", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn init_unlock_lock_session_lifecycle() {
    let (app, _dir) = build_app();

    let (status, body) = request(
        &app,
        "POST",
        "/api/init",
        Some(r#"{"workspace":"default","password":"secret","confirm":"secret"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let token = body["token"].as_str().unwrap().to_owned();

    // Unauthenticated data access is rejected.
    let (status, _) = request(&app, "GET", "/api/dashboard", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Authenticated access works.
    let (status, body) = request(&app, "GET", "/api/dashboard", None, Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["instances"].as_array().unwrap().len(), 0);

    // Locking invalidates the session.
    let (status, _) = request(&app, "POST", "/api/lock", Some("{}"), Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(&app, "GET", "/api/dashboard", None, Some(&token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn init_requires_matching_confirmation() {
    let (app, _dir) = build_app();
    let (status, body) = request(
        &app,
        "POST",
        "/api/init",
        Some(r#"{"workspace":"default","password":"secret","confirm":"different"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("confirmation"));
}

#[tokio::test]
async fn unlock_rejects_wrong_password() {
    let (app, _dir) = build_app();
    let (status, _) = request(
        &app,
        "POST",
        "/api/init",
        Some(r#"{"workspace":"default","password":"secret","confirm":"secret"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = request(
        &app,
        "POST",
        "/api/unlock",
        Some(r#"{"workspace":"default","password":"wrong"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn init_rejected_when_already_initialized() {
    let (app, _dir) = build_app();
    let payload = r#"{"workspace":"default","password":"secret","confirm":"secret"}"#;
    let (status, _) = request(&app, "POST", "/api/init", Some(payload), None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request(&app, "POST", "/api/init", Some(payload), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("already has a master password"),
        "body: {body}"
    );
}

#[tokio::test]
async fn workspaces_keep_vaults_isolated() {
    let (app, _dir) = build_app();

    // Unlock workspace alpha and add an instance to it.
    let (status, body) = request(
        &app,
        "POST",
        "/api/init",
        Some(r#"{"workspace":"alpha","password":"alpha-pass","confirm":"alpha-pass"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let alpha_token = body["token"].as_str().unwrap().to_owned();

    let (status, _) = request(
        &app,
        "POST",
        "/api/instances",
        Some(r#"{"name":"prod","url":"postgresql://admin:pw@db.example.com:5432/postgres"}"#),
        Some(&alpha_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request(&app, "GET", "/api/dashboard", None, Some(&alpha_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["instances"].as_array().unwrap().len(), 1);

    // Switch to a brand-new workspace beta: it must see an empty vault.
    let _ = request(&app, "POST", "/api/lock", Some("{}"), Some(&alpha_token)).await;
    let (status, body) = request(
        &app,
        "POST",
        "/api/init",
        Some(r#"{"workspace":"beta","password":"beta-pass","confirm":"beta-pass"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let beta_token = body["token"].as_str().unwrap().to_owned();

    let (status, body) = request(&app, "GET", "/api/dashboard", None, Some(&beta_token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["instances"].as_array().unwrap().len(),
        0,
        "beta must not see alpha's instances"
    );

    // Alpha's password does not open beta, and vice versa.
    let _ = request(&app, "POST", "/api/lock", Some("{}"), Some(&beta_token)).await;
    let (status, _) = request(
        &app,
        "POST",
        "/api/unlock",
        Some(r#"{"workspace":"beta","password":"alpha-pass"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = request(
        &app,
        "POST",
        "/api/unlock",
        Some(r#"{"workspace":"alpha","password":"beta-pass"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Unlocking alpha again brings its vault back untouched.
    let (status, body) = request(
        &app,
        "POST",
        "/api/unlock",
        Some(r#"{"workspace":"alpha","password":"alpha-pass"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let alpha_token = body["token"].as_str().unwrap().to_owned();

    let (status, body) = request(&app, "GET", "/api/dashboard", None, Some(&alpha_token)).await;
    assert_eq!(status, StatusCode::OK);
    let instances = body["instances"].as_array().unwrap();
    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0]["name"], "prod");
}
