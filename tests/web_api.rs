use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

use cais::storage::Storage;
use cais::web::state::WebState;

fn build_app() -> (axum::Router, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_at(dir.path().join("data.sqlite")).unwrap();
    let state = Arc::new(WebState::from_storage(storage).unwrap());
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
async fn status_reports_not_initialized() {
    let (app, _dir) = build_app();
    let (status, body) = request(&app, "GET", "/api/status", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["initialized"], false);
}

#[tokio::test]
async fn init_unlock_lock_session_lifecycle() {
    let (app, _dir) = build_app();

    let (status, body) = request(
        &app,
        "POST",
        "/api/init",
        Some(r#"{"password":"secret","confirm":"secret"}"#),
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
        Some(r#"{"password":"secret","confirm":"different"}"#),
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
        Some(r#"{"password":"secret","confirm":"secret"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = request(
        &app,
        "POST",
        "/api/unlock",
        Some(r#"{"password":"wrong"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn init_rejected_when_already_initialized() {
    let (app, _dir) = build_app();
    let (status, _) = request(
        &app,
        "POST",
        "/api/init",
        Some(r#"{"password":"secret","confirm":"secret"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = request(
        &app,
        "POST",
        "/api/init",
        Some(r#"{"password":"secret","confirm":"secret"}"#),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
