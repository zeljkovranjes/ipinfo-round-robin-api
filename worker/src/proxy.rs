// Same logic as server/src/proxy.rs.
// Differences:
//   - No axum/tower — workers-rs native Request/Response/Fetch/Cache
//   - Manual routing (no Router) — avoids !Send future issues with axum Handler trait
//   - worker::Fetch replaces reqwest
//   - Workers Cache API (worker::Cache) replaces moka
//   - No singleflight — Workers handles request coalescing at infra level
//   - worker::console_log! replaces tracing

use std::sync::Arc;

use worker::{AnalyticsEngineDataPointBuilder, Cache, Env, Fetch, Headers, Method, Request, RequestInit, Response, Result};

use crate::{
    config::{mask_key, Config},
    rotator::Rotator,
};

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

/// Main entry point — routes to the right handler.
pub async fn handle(req: Request, env: Env, state: AppState) -> Result<Response> {
    let path = req.path();
    let trimmed = path.trim_end_matches('/');

    match (req.method(), trimmed) {
        (Method::Get, "/health") => health_handler(&state).await,
        (Method::Get, "/stats") => stats_handler(&state).await,
        (Method::Delete, "/cache") => cache_delete_handler().await,
        _ => proxy_handler(req, &state, &env).await,
    }
}

// ---------------------------------------------------------------------------
// Analytics helpers
// ---------------------------------------------------------------------------

fn request_path_type(path: &str, is_post_batch: bool, is_me: bool) -> &'static str {
    if is_post_batch {
        return "batch";
    }
    if is_me {
        return "me";
    }
    if path == "/" || path.is_empty() {
        return "root";
    }
    let segments = path.trim_start_matches('/').split('/').filter(|s| !s.is_empty()).count();
    if segments >= 2 { "ip_field" } else { "ip" }
}

fn write_analytics(
    env: &Env,
    kind: &str,
    status: u16,
    cache_hit: bool,
    upstream: bool,
    cooldown: bool,
    bytes: usize,
) {
    if let Ok(ds) = env.analytics_engine("ANALYTICS") {
        let point = AnalyticsEngineDataPointBuilder::new()
            .indexes([kind])
            .doubles(vec![
                f64::from(status),
                if cache_hit { 1.0 } else { 0.0 },
                if upstream { 1.0 } else { 0.0 },
                if cooldown { 1.0 } else { 0.0 },
                bytes as f64,
            ])
            .build();
        let _ = ds.write_data_point(&point);
    }
}

// ---------------------------------------------------------------------------
// Internal handlers
// ---------------------------------------------------------------------------

async fn health_handler(state: &AppState) -> Result<Response> {
    let key_stats = state.rotator.stats();
    let status = if key_stats.active > 0 { "ok" } else { "degraded" };
    Response::from_json(&serde_json::json!({ "status": status, "keys": key_stats }))
}

async fn stats_handler(state: &AppState) -> Result<Response> {
    Response::from_json(&serde_json::json!({
        "keys": state.rotator.stats(),
        "note": "per-isolate counters; resets when isolate restarts"
    }))
}

async fn cache_delete_handler() -> Result<Response> {
    // Workers Cache API has no bulk-invalidation. Entries expire via Cache-Control max-age.
    Ok(Response::from_json(&serde_json::json!({
        "error": "Bulk cache invalidation is not supported in the Workers runtime. Entries expire via Cache-Control max-age."
    }))?
    .with_status(501))
}

// ---------------------------------------------------------------------------
// Proxy handler
// ---------------------------------------------------------------------------

async fn proxy_handler(mut req: Request, state: &AppState, env: &Env) -> Result<Response> {
    let url = req.url()?;
    let path = url.path().to_string();
    let is_post_batch = req.method() == Method::Post;
    let is_me = path == "/me";
    let no_cache = req
        .headers()
        .get("cache-control")?
        .map(|v| v.contains("no-cache"))
        .unwrap_or(false);

    let kind = request_path_type(&path, is_post_batch, is_me);

    // Build the upstream URL, preserving query params
    let path_with_query = match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    };
    let upstream_url = format!("{}{}", state.config.ipinfo_base_url, path_with_query);

    // --- Workers Cache API read (skip for POST /batch, no-cache, and /me) ---
    if !is_post_batch && !no_cache && !is_me {
        if let Some(cached) = Cache::default().get(upstream_url.as_str(), false).await? {
            let status = cached.status_code();
            write_analytics(env, kind, status, true, false, false, 0);
            return Ok(cached);
        }
    }

    // --- Read request body (needed for POST /batch) ---
    let body_bytes: Vec<u8> = if is_post_batch { req.bytes().await? } else { Vec::new() };

    // --- Get API key ---
    let key = match state.rotator.next_key() {
        Some(k) => k,
        None => {
            write_analytics(env, kind, 503, false, false, false, 0);
            let mut resp =
                Response::error("All API keys are rate limited. Try again later.", 503)?;
            resp.headers_mut()
                .set("retry-after", &state.config.cooldown_seconds.to_string())?;
            return Ok(resp);
        }
    };

    // --- Build upstream request ---
    let mut headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {key}"))?;
    headers.set("Accept", "application/json")?;

    let w_method = if is_post_batch { Method::Post } else { Method::Get };
    let mut init = RequestInit::new();
    init.with_method(w_method).with_headers(headers);

    if is_post_batch && !body_bytes.is_empty() {
        // POST /batch body is JSON — safe to treat as a UTF-8 string
        let body_str = String::from_utf8_lossy(&body_bytes).to_string();
        init.with_body(Some(worker::wasm_bindgen::JsValue::from_str(&body_str)));
    }

    let upstream_req = Request::new_with_init(&upstream_url, &init)?;
    let mut upstream_resp = Fetch::Request(upstream_req).send().await?;

    let upstream_status = upstream_resp.status_code();
    let cooldown = upstream_status == 429 || upstream_status == 401;

    if cooldown {
        state.rotator.mark_cooldown(&key);
        worker::console_log!(
            "key {} marked for cooldown (status {})",
            mask_key(&key),
            upstream_status
        );
    }

    let content_type = upstream_resp
        .headers()
        .get("content-type")?
        .unwrap_or_else(|| "application/json".to_string());

    let resp_bytes = upstream_resp.bytes().await?;

    // Apply key redaction for /me
    let resp_bytes = if state.config.redact_keys && is_me {
        redact_token_field(resp_bytes)
    } else {
        resp_bytes
    };

    // --- Workers Cache API write ---
    if !is_post_batch
        && !is_me
        && !no_cache
        && upstream_status == 200
        && resp_bytes.len() <= state.config.cache_max_body_bytes
    {
        let mut cache_headers = Headers::new();
        cache_headers.set("Content-Type", &content_type)?;
        cache_headers.set(
            "Cache-Control",
            &format!("max-age={}", state.config.cache_ttl_seconds),
        )?;
        if let Ok(cache_resp) = Response::from_bytes(resp_bytes.clone()) {
            let cache_resp = cache_resp.with_headers(cache_headers);
            let _ = Cache::default().put(upstream_url.as_str(), cache_resp).await;
        }
    }

    write_analytics(env, kind, upstream_status, false, true, cooldown, resp_bytes.len());

    // --- Build final response ---
    let mut resp_headers = Headers::new();
    resp_headers.set("Content-Type", &content_type)?;
    Ok(Response::from_bytes(resp_bytes)?.with_status(upstream_status).with_headers(resp_headers))
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
