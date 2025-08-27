#![deny(clippy::print_stdout, clippy::print_stderr)]

mod cli;
pub use cli::ApiMode;
pub use cli::Cli;

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::routing::post;
use axum::Json;
use axum::Router;
use bytes::Bytes;
use codex_common::CliConfigOverrides;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::WireApi;
use codex_login::AuthMode;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::Any;
use tower_http::cors::CorsLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

// Use the same base instructions shipped in core for Responses API when using ChatGPT auth.
const CORE_BASE_INSTRUCTIONS: &str = include_str!("../../core/prompt.md");

#[derive(Clone)]
pub struct AppState {
    pub cfg: std::sync::Arc<Config>,
    pub bearer: Option<String>,
    pub allow_no_auth: bool,
}

pub async fn run_main(
    cli: Cli,
    codex_linux_sandbox_exe: Option<PathBuf>,
    cli_config_overrides: CliConfigOverrides,
) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Enforce auth safety: only allow --no-auth when binding to loopback.
    let is_loopback = cli.host == "127.0.0.1" || cli.host == "::1" || cli.host == "localhost";
    if cli.no_auth && !is_loopback {
        anyhow::bail!("--no-auth is only allowed when binding to localhost");
    }

    // Prepare config as in other modes, reusing overrides and defaults.
    let cli_kv_overrides = cli_config_overrides
        .parse_overrides()
        .map_err(|e| anyhow::anyhow!("error parsing -c overrides: {e}"))?;
    let cfg = Config::load_with_cli_overrides(cli_kv_overrides, ConfigOverrides::default())
        .map_err(|e| anyhow::anyhow!("error loading config: {e}"))?;

    // Server uses the same preferred auth method as other modes. Leave as-is.
    // For clarity, we keep ChatGPT by default as set by Config.
    if cfg.preferred_auth_method == AuthMode::ChatGPT {
        // no-op, present for documentation/readability
    }

    let app_state = AppState {
        cfg: std::sync::Arc::new(cfg),
        bearer: cli.token.clone(),
        allow_no_auth: cli.no_auth,
    };

    let app = build_app(app_state.clone(), &cli.cors_origins);

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    info!("codex-server listening on http://{}", addr);

    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;

    let _ = codex_linux_sandbox_exe; // currently unused; reserved for future policies
    Ok(())
}

pub fn build_app(state: AppState, cors_origins: &[String]) -> Router {
    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .with_state(state);

    if !cors_origins.is_empty() {
        let cors = if cors_origins.len() == 1 && cors_origins[0] == "*" {
            CorsLayer::new().allow_origin(Any)
        } else {
            let origins = cors_origins
                .iter()
                .filter_map(|o| o.parse().ok())
                .collect::<Vec<_>>();
            CorsLayer::new().allow_origin(origins)
        };
        app = app.layer(cors);
    }
    app
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !check_auth(&headers, &state) {
        return unauthorized().into_response();
    }
    let model_id = state.cfg.model.clone();
    let resp = serde_json::json!({
        "object": "list",
        "data": [{"id": model_id, "object": "model"}],
    });
    Json(resp).into_response()
}

#[derive(Debug, Deserialize)]
struct ChatCompletionsRequest {
    model: Option<String>,
    messages: Vec<OpenAIChatMessage>,
    #[allow(dead_code)]
    temperature: Option<f32>,
    #[allow(dead_code)]
    max_tokens: Option<u32>,
    stream: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct OpenAIChatMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct ChatCompletionsResponseChoice {
    index: u32,
    message: OpenAIChatMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatCompletionsResponse {
    id: String,
    object: String,
    created: i64,
    model: String,
    choices: Vec<ChatCompletionsResponseChoice>,
}

fn unauthorized() -> impl IntoResponse {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error":{"message":"Unauthorized"}})),
    )
}

fn check_auth(headers: &HeaderMap, state: &AppState) -> bool {
    if state.allow_no_auth {
        return true;
    }
    let Some(expected) = state.bearer.as_ref() else {
        return false;
    };
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());
    match header {
        Some(h) if h.starts_with("Bearer ") => h[7..] == *expected,
        _ => false,
    }
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ChatCompletionsRequest>,
) -> impl IntoResponse {
    if !check_auth(&headers, &state) {
        return unauthorized().into_response();
    }

    let model = body
        .model
        .clone()
        .unwrap_or_else(|| state.cfg.model.clone());
    let stream = body.stream.unwrap_or(false);

    // If provider uses the Responses API (e.g., ChatGPT), translate the request and proxy.
    if state.cfg.model_provider.wire_api == WireApi::Responses {
        return chat_to_responses_proxy_cc(state, model, stream, body.messages).await;
    }

    let mut payload = serde_json::json!({
        "model": model,
        "messages": body.messages,
    });
    if stream {
        payload["stream"] = serde_json::json!(true);
    }

    let client = reqwest::Client::new();
    let auth = codex_login::CodexAuth::from_codex_home(
        &state.cfg.codex_home,
        state.cfg.preferred_auth_method,
    )
    .ok()
    .flatten();

    let builder = match state
        .cfg
        .model_provider
        .create_request_builder(&client, &auth)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("auth/config error: {e}")}})),
            )
                .into_response();
        }
    };

    if !stream {
        match builder.json(&payload).send().await {
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let text = resp
                    .text()
                    .await
                    .unwrap_or_else(|e| format!("{{\"error\":{{\"message\":\"{e}\"}}}}"));
                let mut r = axum::response::Response::new(Body::from(text));
                *r.status_mut() = status;
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                );
                r
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("upstream error: {e}")}})),
            )
                .into_response(),
        }
    } else {
        let req = builder
            .header(axum::http::header::ACCEPT, "text/event-stream")
            .json(&payload);
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let upstream = resp.bytes_stream();
                let (tx, rx) = mpsc::channel::<Result<Bytes, std::convert::Infallible>>(16);
                tokio::spawn(async move {
                    use futures::StreamExt;
                    tokio::pin!(upstream);
                    while let Some(chunk) = upstream.next().await {
                        match chunk {
                            Ok(bytes) => {
                                if tx.send(Ok(bytes)).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
                let body = Body::from_stream(ReceiverStream::new(rx));
                let mut r = axum::response::Response::new(body);
                *r.status_mut() =
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("text/event-stream"),
                );
                r
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("upstream error: {e}")}})),
            )
                .into_response(),
        }
    }
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if !check_auth(&headers, &state) {
        return unauthorized().into_response();
    }

    // Only support providers that speak Responses API for this endpoint.
    if state.cfg.model_provider.wire_api != WireApi::Responses {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(serde_json::json!({
                "error": {"message": "Provider uses Chat Completions; /v1/responses not supported for this provider"}
            })),
        )
            .into_response();
    }

    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let client = reqwest::Client::new();
    let auth = codex_login::CodexAuth::from_codex_home(
        &state.cfg.codex_home,
        state.cfg.preferred_auth_method,
    )
    .ok()
    .flatten();

    // ChatGPT backend requires `store: false` and expects Codex base instructions.
    if matches!(auth.as_ref().map(|a| a.mode), Some(codex_login::AuthMode::ChatGPT)) {
        body["store"] = serde_json::Value::Bool(false);
        body["instructions"] = serde_json::Value::String(CORE_BASE_INSTRUCTIONS.to_string());
    }

    // Ensure `instructions` is present – Responses API requires it.
    if body.get("instructions").is_none() || body.get("instructions").and_then(|v| v.as_str()).is_none() {
        // Prefer config-provided base instructions if present; otherwise a minimal generic prompt.
        let fallback = state
            .cfg
            .base_instructions
            .clone()
            .unwrap_or_else(|| "You are Codex, a helpful coding assistant. Keep responses concise and accurate.".to_string());
        body["instructions"] = serde_json::Value::String(fallback);
    }

    let mut builder = match state
        .cfg
        .model_provider
        .create_request_builder(&client, &auth)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("auth/config error: {e}")}})),
            )
                .into_response();
        }
    };

    let originator = state.cfg.responses_originator_header.clone();
    builder = builder
        .header("OpenAI-Beta", "responses=experimental")
        .header("session_id", uuid::Uuid::new_v4().to_string())
        .header("originator", &originator)
        .header("User-Agent", codex_core::user_agent::get_codex_user_agent(Some(&originator)));

    if let Some(a) = auth.as_ref() {
        if a.mode == codex_login::AuthMode::ChatGPT {
            if let Some(account_id) = a.get_account_id() {
                builder = builder.header("chatgpt-account-id", account_id);
            }
        }
    }

    if !stream {
        match builder.json(&body).send().await {
            Ok(resp) => {
                let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let text = resp.text().await.unwrap_or_else(|e| format!("{{\"error\":{{\"message\":\"{e}\"}}}}"));
                let mut r = axum::response::Response::new(Body::from(text));
                *r.status_mut() = status;
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                );
                r
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("upstream error: {e}")}})),
            )
                .into_response(),
        }
    } else {
        let req = builder
            .header(axum::http::header::ACCEPT, "text/event-stream")
            .json(&body);
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let upstream = resp.bytes_stream();
                let (tx, rx) = mpsc::channel::<Result<Bytes, std::convert::Infallible>>(16);
                tokio::spawn(async move {
                    use futures::StreamExt;
                    tokio::pin!(upstream);
                    while let Some(chunk) = upstream.next().await {
                        match chunk {
                            Ok(bytes) => {
                                if tx.send(Ok(bytes)).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
                let body = Body::from_stream(ReceiverStream::new(rx));
                let mut r = axum::response::Response::new(body);
                *r.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("text/event-stream"),
                );
                r
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("upstream error: {e}")}})),
            )
                .into_response(),
        }
    }
}

fn map_chat_messages_to_responses_input(messages: &[OpenAIChatMessage]) -> serde_json::Value {
    let mut input = Vec::<serde_json::Value>::new();
    for m in messages {
        let role = m.role.clone();
        let text = match &m.content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => {
                let mut buf = String::new();
                for item in arr {
                    if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
                        if !buf.is_empty() {
                            buf.push_str("\n");
                        }
                        buf.push_str(s);
                    }
                }
                buf
            }
            _ => String::new(),
        };
        let ty = if role == "assistant" { "output_text" } else { "input_text" };
        input.push(serde_json::json!({
            "role": role,
            "content": [{"type": ty, "text": text}],
        }));
    }
    serde_json::Value::Array(input)
}

async fn chat_to_responses_proxy(
    state: AppState,
    model: String,
    stream: bool,
    messages: Vec<OpenAIChatMessage>,
) -> axum::response::Response {
    let mut body = serde_json::json!({
        "model": model,
        "input": map_chat_messages_to_responses_input(&messages),
        "tools": [],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "stream": stream,
        "include": [],
    });

    let client = reqwest::Client::new();
    let auth = codex_login::CodexAuth::from_codex_home(
        &state.cfg.codex_home,
        state.cfg.preferred_auth_method,
    )
    .ok()
    .flatten();

    if matches!(auth.as_ref().map(|a| a.mode), Some(codex_login::AuthMode::ChatGPT)) {
        body["store"] = serde_json::Value::Bool(false);
        body["instructions"] = serde_json::Value::String(CORE_BASE_INSTRUCTIONS.to_string());
    } else if body.get("instructions").is_none() {
        body["instructions"] = serde_json::Value::String(
            "You are Codex, a helpful coding assistant.".to_string(),
        );
    }

    let originator = state.cfg.responses_originator_header.clone();
    let mut builder = match state
        .cfg
        .model_provider
        .create_request_builder(&client, &auth)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("auth/config error: {e}")}})),
            )
                .into_response();
        }
    };

    builder = builder
        .header("OpenAI-Beta", "responses=experimental")
        .header("session_id", uuid::Uuid::new_v4().to_string())
        .header("originator", &originator)
        .header(
            "User-Agent",
            codex_core::user_agent::get_codex_user_agent(Some(&originator)),
        );
    if let Some(a) = auth.as_ref() {
        if a.mode == codex_login::AuthMode::ChatGPT {
            if let Some(account_id) = a.get_account_id() {
                builder = builder.header("chatgpt-account-id", account_id);
            }
        }
    }

    if !stream {
        match builder.json(&body).send().await {
            Ok(resp) => {
                let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let text = resp.text().await.unwrap_or_else(|e| format!("{{\"error\":{{\"message\":\"{e}\"}}}}"));
                let mut r = axum::response::Response::new(Body::from(text));
                *r.status_mut() = status;
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                );
                r
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("upstream error: {e}")}})),
            )
                .into_response(),
        }
    } else {
        let req = builder
            .header(axum::http::header::ACCEPT, "text/event-stream")
            .json(&body);
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let upstream = resp.bytes_stream();
                let (tx, rx) = mpsc::channel::<Result<Bytes, std::convert::Infallible>>(16);
                tokio::spawn(async move {
                    use futures::StreamExt;
                    tokio::pin!(upstream);
                    while let Some(chunk) = upstream.next().await {
                        match chunk {
                            Ok(bytes) => {
                                if tx.send(Ok(bytes)).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
                let body = Body::from_stream(ReceiverStream::new(rx));
                let mut r = axum::response::Response::new(body);
                *r.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("text/event-stream"),
                );
                r
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("upstream error: {e}")}})),
            )
                .into_response(),
        }
    }
}

async fn chat_to_responses_proxy_cc(
    state: AppState,
    model: String,
    stream: bool,
    messages: Vec<OpenAIChatMessage>,
) -> axum::response::Response {
    let mut body = serde_json::json!({
        "model": model,
        "input": map_chat_messages_to_responses_input(&messages),
        "tools": [],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "stream": stream,
        "include": [],
    });

    let client = reqwest::Client::new();
    let auth = codex_login::CodexAuth::from_codex_home(
        &state.cfg.codex_home,
        state.cfg.preferred_auth_method,
    )
    .ok()
    .flatten();

    if matches!(auth.as_ref().map(|a| a.mode), Some(codex_login::AuthMode::ChatGPT)) {
        body["store"] = serde_json::Value::Bool(false);
        body["instructions"] = serde_json::Value::String(CORE_BASE_INSTRUCTIONS.to_string());
    } else if body.get("instructions").is_none() {
        body["instructions"] = serde_json::Value::String(
            "You are Codex, a helpful coding assistant.".to_string(),
        );
    }

    let originator = state.cfg.responses_originator_header.clone();
    let mut builder = match state
        .cfg
        .model_provider
        .create_request_builder(&client, &auth)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("auth/config error: {e}")}})),
            )
                .into_response();
        }
    };

    builder = builder
        .header("OpenAI-Beta", "responses=experimental")
        .header("session_id", uuid::Uuid::new_v4().to_string())
        .header("originator", &originator)
        .header(
            "User-Agent",
            codex_core::user_agent::get_codex_user_agent(Some(&originator)),
        );
    if let Some(a) = auth.as_ref() {
        if a.mode == codex_login::AuthMode::ChatGPT {
            if let Some(account_id) = a.get_account_id() {
                builder = builder.header("chatgpt-account-id", account_id);
            }
        }
    }

    if !stream {
        match builder.json(&body).send().await {
            Ok(resp) => {
                let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                match resp.json::<serde_json::Value>().await {
                    Ok(v) => {
                        let output = v.get("output").cloned()
                            .or_else(|| v.get("response").and_then(|r| r.get("output")).cloned())
                            .unwrap_or(serde_json::json!([]));
                        let mut content = String::new();
                        if let Some(arr) = output.as_array() {
                            for item in arr {
                                if item.get("type").and_then(|s| s.as_str()) == Some("message") {
                                    if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                                        for part in parts {
                                            if part.get("type").and_then(|s| s.as_str()) == Some("output_text") {
                                                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                                                    content.push_str(t);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        let usage = v.get("usage").cloned()
                            .or_else(|| v.get("response").and_then(|r| r.get("usage")).cloned())
                            .unwrap_or(serde_json::json!({}));
                        let prompt_tokens = usage.get("input_tokens").and_then(|n| n.as_u64()).unwrap_or(0);
                        let completion_tokens = usage.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0);
                        let total_tokens = usage.get("total_tokens").and_then(|n| n.as_u64()).unwrap_or(prompt_tokens + completion_tokens);

                        let cc = serde_json::json!({
                            "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
                            "object": "chat.completion",
                            "created": time::OffsetDateTime::now_utc().unix_timestamp(),
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": content},
                                "finish_reason": "stop"
                            }],
                            "usage": {
                                "prompt_tokens": prompt_tokens,
                                "completion_tokens": completion_tokens,
                                "total_tokens": total_tokens
                            }
                        });

                        let mut r = axum::response::Response::new(Body::from(cc.to_string()));
                        *r.status_mut() = status;
                        r.headers_mut().insert(
                            axum::http::header::CONTENT_TYPE,
                            axum::http::HeaderValue::from_static("application/json"),
                        );
                        r
                    }
                    Err(e) => (
                        StatusCode::BAD_GATEWAY,
                        Json(serde_json::json!({"error": {"message": format!("invalid upstream json: {e}")}})),
                    ).into_response(),
                }
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("upstream error: {e}")}})),
            )
                .into_response(),
        }
    } else {
        let req = builder
            .header(axum::http::header::ACCEPT, "text/event-stream")
            .json(&body);
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let upstream = resp.bytes_stream();
                let (tx, rx) = mpsc::channel::<Result<Bytes, std::convert::Infallible>>(64);
                let cc_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
                let created = time::OffsetDateTime::now_utc().unix_timestamp();
                tokio::spawn(async move {
                    use futures::StreamExt;
                    tokio::pin!(upstream);
                    let mut buf: Vec<u8> = Vec::new();
                    let mut sent_role = false;
                    while let Some(chunk) = upstream.next().await {
                        let Ok(bytes) = chunk else { break };
                        buf.extend_from_slice(&bytes);
                        loop {
                            if let Some(pos) = memchr::memmem::find(&buf, b"\n\n") {
                                let event = buf.drain(..pos + 2).collect::<Vec<u8>>();
                                for line in event.split(|&b| b == b'\n') {
                                    let payload = if line.starts_with(b"data: ") { &line[6..] } else { continue };
                                    if payload == b"[DONE]" { continue; }
                                    if let Ok(s) = std::str::from_utf8(payload) {
                                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
                                            let kind = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                            if kind == "response.output_text.delta" {
                                                if let Some(delta) = val.get("delta").and_then(|v| v.as_str()) {
                                                    let mut delta_obj = serde_json::json!({"content": delta});
                                                    if !sent_role {
                                                        delta_obj["role"] = serde_json::Value::String("assistant".to_string());
                                                        sent_role = true;
                                                    }
                                                    let chunk = serde_json::json!({
                                                        "id": cc_id,
                                                        "object": "chat.completion.chunk",
                                                        "created": created,
                                                        "model": model,
                                                        "choices": [{
                                                            "index": 0,
                                                            "delta": delta_obj,
                                                            "finish_reason": serde_json::Value::Null
                                                        }]
                                                    });
                                                    let sse = format!("data: {}\n\n", chunk.to_string());
                                                    if tx.send(Ok(Bytes::from(sse))).await.is_err() { return; }
                                                }
                                            } else if kind == "response.completed" {
                                                let final_chunk = serde_json::json!({
                                                    "id": cc_id,
                                                    "object": "chat.completion.chunk",
                                                    "created": created,
                                                    "model": model,
                                                    "choices": [{
                                                        "index": 0,
                                                        "delta": {},
                                                        "finish_reason": "stop"
                                                    }]
                                                });
                                                let s1 = format!("data: {}\n\n", final_chunk.to_string());
                                                let s2 = "data: [DONE]\n\n".to_string();
                                                let _ = tx.send(Ok(Bytes::from(s1))).await;
                                                let _ = tx.send(Ok(Bytes::from(s2))).await;
                                                return;
                                            }
                                        }
                                    }
                                }
                            } else { break; }
                        }
                    }
                });
                let body = Body::from_stream(ReceiverStream::new(rx));
                let mut r = axum::response::Response::new(body);
                *r.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                r.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("text/event-stream"),
                );
                r
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": {"message": format!("upstream error: {e}")}})),
            )
                .into_response(),
        }
    }
}
