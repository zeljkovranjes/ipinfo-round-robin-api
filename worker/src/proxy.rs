// Same routing structure and handler logic as server/src/proxy.rs.
// Differences:
//   - worker::Fetch replaces reqwest
//   - Workers Cache API (worker::Cache) replaces moka
//   - No singleflight (Workers handles request coalescing at infra level)
//   - No Stats struct (per-isolate counters reset on restart; not useful for Workers)
//   - worker::console_log! replaces tracing

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use worker::{Cache, Fetch, Headers, Method, Request as WRequest, RequestInit, Response as WResponse};

use crate::{
    config::{mask_key, Config},
    rotator::Rotator,
};

/// Shared application state, cheaply cloned via Arc.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub rotator: Arc<Rotator>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let rotator = Arc::new(Rotator::new(config.keys.clone(), config.cooldown_seconds));
        AppState { rotator, config: Arc::new(config) }
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Internal endpoints (registered first so they win over /:ip)
        .route("/health", get(health_handler))
        .route("/stats", get(stats_handler))
        .route("/cache", delete(cache_delete_handler))
        // IPInfo proxy endpoints
        .route("/", get(proxy_handler))
        .route("/me", get(proxy_handler))
        .route("/batch", post(proxy_handler))
        .route("/:ip", get(proxy_handler))
        .route("/:ip/:field", get(proxy_handler))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Internal handlers
// ---------------------------------------------------------------------------

async fn health_handler(State(s): State<AppState>) -> impl IntoResponse {
    let key_stats = s.rotator.stats();
    let status = if key_stats.active > 0 { "ok" } else { "degraded" };
    Json(serde_json::json!({ "status": status, "keys": key_stats }))
}

async fn stats_handler(State(s): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "keys": s.rotator.stats(),
        "note": "per-isolate counters; resets when isolate restarts"
    }))
}

async fn cache_delete_handler() -> impl IntoResponse {
    // Workers Cache API has no bulk-invalidation. Entries expire via Cache-Control max-age.
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "Bulk cache invalidation is not supported in the Workers runtime. Entries expire via Cache-Control max-age."
        })),
    )
}

// ---------------------------------------------------------------------------
// Proxy handler
// ---------------------------------------------------------------------------

async fn proxy_handler(State(s): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    let uri = req.uri().clone();

    let is_post_batch = method == axum::http::Method::POST;
    let is_me = uri.path() == "/me";
    let no_cache = req
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("no-cache"))
        .unwrap_or(false);

    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let upstream_url = format!("{}{}", s.config.ipinfo_base_url, path_and_query);

    // --- Workers Cache API read (skip for POST /batch, no-cache, and /me) ---
    if !is_post_batch && !no_cache && !is_me {
        if let Ok(cache_req) = WRequest::new(&upstream_url, Method::Get) {
            match Cache::default().get(cache_req, false).await {
                Ok(Some(mut cached)) => {
                    let status = cached.status_code();
                    let content_type = cached
                        .headers()
                        .get("content-type")
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "application/json".to_string());
                    if let Ok(body) = cached.bytes().await {
                        return build_response(status, &content_type, body.into());
                    }
                }
                _ => {}
            }
        }
    }

    // --- Read request body (needed for POST /batch) ---
    let body_bytes = if is_post_batch {
        match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(b) => b,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "Failed to read request body").into_response()
            }
        }
    } else {
        axum::body::Bytes::new()
    };

    // --- Get API key ---
    let key = match s.rotator.next_key() {
        Some(k) => k,
        None => {
            let retry_after = s.config.cooldown_seconds.to_string();
            let mut resp = (
                StatusCode::SERVICE_UNAVAILABLE,
                "All API keys are rate limited. Try again later.",
            )
                .into_response();
            if let Ok(v) = HeaderValue::from_str(&retry_after) {
                resp.headers_mut().insert("retry-after", v);
            }
            return resp;
        }
    };

    // --- Build upstream request ---
    let mut w_headers = Headers::new();
    let _ = w_headers.set("Authorization", &format!("Bearer {key}"));
    let _ = w_headers.set("Accept", "application/json");

    let w_method = if is_post_batch { Method::Post } else { Method::Get };
    let mut init = RequestInit::new();
    init.with_method(w_method).with_headers(w_headers);

    if is_post_batch && !body_bytes.is_empty() {
        // POST /batch body is JSON — safe to treat as UTF-8 string
        let body_str = String::from_utf8_lossy(&body_bytes).to_string();
        init.with_body(Some(wasm_bindgen::JsValue::from_str(&body_str)));
    }

    let upstream_req = match WRequest::new_with_init(&upstream_url, &init) {
        Ok(r) => r,
        Err(e) => {
            worker::console_log!("failed to build upstream request: {:?}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut upstream_resp = match Fetch::Request(upstream_req).send().await {
        Ok(r) => r,
        Err(e) => {
            worker::console_log!("upstream fetch failed: {:?}", e);
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let upstream_status = upstream_resp.status_code();

    if upstream_status == 429 || upstream_status == 401 {
        s.rotator.mark_cooldown(&key);
        worker::console_log!(
            "key {} marked for cooldown (status {})",
            mask_key(&key),
            upstream_status
        );
    }

    let content_type = upstream_resp
        .headers()
        .get("content-type")
        .ok()
        .flatten()
        .unwrap_or_else(|| "application/json".to_string());

    let resp_bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            worker::console_log!("failed to read upstream body: {:?}", e);
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    // Apply key redaction for /me
    let resp_bytes = if s.config.redact_keys && is_me {
        redact_token_field(resp_bytes)
    } else {
        resp_bytes
    };

    // --- Workers Cache API write ---
    if !is_post_batch
        && !is_me
        && !no_cache
        && upstream_status == 200
        && resp_bytes.len() <= s.config.cache_max_body_bytes
    {
        let mut cache_headers = Headers::new();
        let _ = cache_headers.set("Content-Type", &content_type);
        let _ = cache_headers.set(
            "Cache-Control",
            &format!("max-age={}", s.config.cache_ttl_seconds),
        );
        if let (Ok(cache_req), Ok(cache_resp)) = (
            WRequest::new(&upstream_url, Method::Get),
            WResponse::from_bytes(resp_bytes.clone()),
        ) {
            let cache_resp = cache_resp.with_headers(cache_headers);
            let _ = Cache::default().put(cache_req, cache_resp).await;
        }
    }

    build_response(upstream_status, &content_type, resp_bytes.into())
}

// ---------------------------------------------------------------------------
// Helpers — same logic as server/src/proxy.rs
// ---------------------------------------------------------------------------

/// Redact the "token" field in a /me JSON response, keeping only the first 3 chars.
fn redact_token_field(body: Vec<u8>) -> Vec<u8> {
    let Ok(mut map) = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&body)
    else {
        return body;
    };
    if let Some(serde_json::Value::String(token)) = map.get("token") {
        let redacted = if token.len() > 3 {
            format!("{}...", &token[..3])
        } else {
            "...".to_string()
        };
        map.insert("token".to_string(), serde_json::Value::String(redacted));
    }
    match serde_json::to_vec(&map) {
        Ok(bytes) => bytes,
        Err(_) => body,
    }
}

fn build_response(status: u16, content_type: &str, body: axum::body::Bytes) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        headers.insert(axum::http::header::CONTENT_TYPE, v);
    }
    (status_code, headers, Body::from(body)).into_response()
}
