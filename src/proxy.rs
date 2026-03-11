use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use tracing::{info, warn};

use crate::{
    cache::Cache,
    config::{mask_key, Config},
    rotator::Rotator,
    stats::Stats,
};

/// Shared application state, cheaply cloned via Arc.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub rotator: Arc<Rotator>,
    pub cache: Arc<Cache>,
    pub stats: Arc<Stats>,
    pub client: reqwest::Client,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(config.request_timeout_ms))
            .build()
            .expect("failed to build reqwest client");

        let rotator = Arc::new(Rotator::new(
            config.keys.clone(),
            config.cooldown_seconds,
        ));
        let cache = Arc::new(Cache::new(
            config.cache_ttl_seconds,
            config.cache_max_entries,
        ));

        AppState {
            rotator,
            cache,
            stats: Arc::new(Stats::new()),
            client,
            config: Arc::new(config),
        }
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
    Json(crate::stats::HealthResponse { status, keys: key_stats })
}

async fn stats_handler(State(s): State<AppState>) -> impl IntoResponse {
    let st = &s.stats;
    Json(crate::stats::StatsResponse {
        requests_total: st.requests_total.load(std::sync::atomic::Ordering::Relaxed),
        requests_proxied: st.requests_proxied.load(std::sync::atomic::Ordering::Relaxed),
        requests_cached: st.requests_cached.load(std::sync::atomic::Ordering::Relaxed),
        upstream_errors: st.upstream_errors.load(std::sync::atomic::Ordering::Relaxed),
        keys_exhausted: st.keys_exhausted.load(std::sync::atomic::Ordering::Relaxed),
        cache: s.cache.stats(),
        keys: s.rotator.stats(),
    })
}

async fn cache_delete_handler(State(s): State<AppState>) -> impl IntoResponse {
    s.cache.clear();
    StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------------------
// Proxy handler
// ---------------------------------------------------------------------------

async fn proxy_handler(State(s): State<AppState>, req: Request) -> Response {
    let start = Instant::now();
    s.stats.inc_total();

    let method = req.method().clone();
    let uri = req.uri().clone();

    // Cache key = method + path + query string
    let cache_key = format!(
        "{}:{}",
        method,
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/")
    );

    let is_post_batch = method == axum::http::Method::POST;
    let no_cache = req
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("no-cache"))
        .unwrap_or(false);

    // --- Cache read (skip for POST /batch and no-cache requests) ---
    if !is_post_batch && !no_cache {
        if let Some(entry) = s.cache.get(&cache_key) {
            s.stats.inc_cached();
            let elapsed = start.elapsed().as_micros();
            info!(
                method = %method,
                path = %uri.path(),
                cache = "hit",
                status = entry.status,
                latency_us = elapsed,
            );
            return build_response(entry.status, &entry.content_type, entry.body);
        }
    }

    // --- Round-robin key selection ---
    let key = match s.rotator.next_key() {
        Some(k) => k,
        None => {
            s.stats.inc_keys_exhausted();
            warn!(path = %uri.path(), "all keys exhausted");
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

    // --- Build upstream URL ---
    let upstream_url = format!(
        "{}{}",
        s.config.ipinfo_base_url,
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/")
    );

    // --- Forward request body (needed for POST /batch) ---
    let req_content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

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

    // --- Make upstream request ---
    let upstream_req = s
        .client
        .request(method.clone(), &upstream_url)
        .header("Authorization", format!("Bearer {key}"))
        .header("Accept", "application/json");

    let upstream_req = if is_post_batch {
        upstream_req
            .header("Content-Type", req_content_type)
            .body(body_bytes.clone())
    } else {
        upstream_req
    };

    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(e) => {
            s.stats.inc_upstream_errors();
            warn!(key = %mask_key(&key), error = %e, "upstream request failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let upstream_status = upstream_resp.status();

    // --- Handle rate limit / invalid key ---
    if upstream_status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || upstream_status == reqwest::StatusCode::UNAUTHORIZED
    {
        s.rotator.mark_cooldown(&key);
        warn!(
            key = %mask_key(&key),
            status = upstream_status.as_u16(),
            "key marked for cooldown"
        );
    }

    // Capture content-type before consuming the response
    let content_type = upstream_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    // --- Read upstream body ---
    let resp_bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            s.stats.inc_upstream_errors();
            warn!(error = %e, "failed to read upstream body");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status_u16 = upstream_status.as_u16();

    s.stats.inc_proxied();
    let elapsed = start.elapsed().as_micros();
    info!(
        method = %method,
        path = %uri.path(),
        cache = "miss",
        key = %mask_key(&key),
        status = status_u16,
        latency_us = elapsed,
    );

    // --- Cache store (only 2xx, not POST /batch, not no-cache) ---
    if !is_post_batch && !no_cache && upstream_status.is_success() {
        s.cache.insert(
            cache_key,
            axum::body::Bytes::copy_from_slice(&resp_bytes),
            content_type.clone(),
            status_u16,
        );
    }

    build_response(status_u16, &content_type, axum::body::Bytes::copy_from_slice(&resp_bytes))
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn build_response(status: u16, content_type: &str, body: axum::body::Bytes) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(content_type) {
        headers.insert(axum::http::header::CONTENT_TYPE, v);
    }
    (status_code, headers, Body::from(body)).into_response()
}
