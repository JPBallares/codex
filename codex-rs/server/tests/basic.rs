use axum::body::to_bytes;
use axum::body::Body;
use axum::http::Method;
use axum::http::Request;
use axum::http::StatusCode;
use axum::Router;
use codex_common::CliConfigOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_server::build_app;
use codex_server::AppState;
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

fn make_app(allow_no_auth: bool, bearer: Option<&str>) -> Router {
    let cfg = Config::load_with_cli_overrides(
        CliConfigOverrides::default()
            .parse_overrides()
            .expect("parse overrides"),
        ConfigOverrides::default(),
    )
    .expect("load config");
    let state = AppState {
        cfg: Arc::new(cfg),
        bearer: bearer.map(|s| s.to_string()),
        allow_no_auth,
    };
    build_app(state, &[])
}

#[tokio::test]
async fn healthz_ok() {
    let app = make_app(true, None);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn models_unauthorized_without_token() {
    let app = make_app(false, Some("secret"));
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn models_authorized_with_bearer() {
    let app = make_app(false, Some("secret"));
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/models")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body_bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let v: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(v["data"][0]["id"].is_string());
}

#[tokio::test]
async fn responses_unauthorized_without_token() {
    // Default built-in provider is OpenAI Responses, so this path is valid.
    let app = make_app(false, Some("secret"));
    let body = serde_json::json!({
        "model": "gpt-4o-mini",
        "input": [{"role":"user","content":[{"type":"input_text","text":"hello"}]}],
        "stream": true
    });
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn chat_completions_unauthorized_without_token() {
    // Provider is Responses; compatibility layer for /v1/chat/completions should still enforce auth.
    let app = make_app(false, Some("secret"));
    let body = serde_json::json!({
        "model": "gpt-4o-mini",
        "messages": [{"role":"user","content":"hello"}],
        "stream": false
    });
    let res = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
