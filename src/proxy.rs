use std::sync::Arc;
use std::time::Instant;

use async_singleflight::Group;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use tracing::{debug, info, warn};

use crate::{
    cache::{Cache, CacheEntry},
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
    pub inflight: Arc<Group<CacheEntry, u16>>,
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
            std::time::Duration::from_secs(config.cache_ttl_seconds),
            config.cache_max_entries,
        ));

        AppState {
            rotator,
            cache,
            stats: Arc::new(Stats::new()),
            client,
            inflight: Arc::new(Group::new()),
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
    let is_me_or_root = matches!(uri.path(), "/" | "/me");
    let no_cache = req
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("no-cache"))
        .unwrap_or(false);

    // --- Cache read (skip for POST /batch, no-cache, and /me|/ which are key-specific) ---
    if !is_post_batch && !no_cache && !is_me_or_root {
        if let Some(entry) = s.cache.get(&cache_key).await {
            s.stats.inc_cached();
            info!(
                method = %method,
                path = %uri.path(),
                cache = "hit",
                status = entry.status,
                latency_us = start.elapsed().as_micros(),
            );
            return build_response(entry.status, &entry.content_type, entry.body);
        }
    }

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

    // --- Build upstream URL ---
    let upstream_url = format!(
        "{}{}",
        s.config.ipinfo_base_url,
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/")
    );

    // POST /batch, no-cache, and /me|/ bypass singleflight — always go direct
    if is_post_batch || no_cache || is_me_or_root {
        return direct_proxy(&s, &method, &upstream_url, body_bytes, req_content_type, is_post_batch, start, &uri).await;
    }

    // --- Singleflight: coalesce concurrent cache-miss requests for the same key ---
    // Returns (Option<CacheEntry>, Option<err_status>, bool is_owner)
    let (ok, err, _) = {
        let s2 = s.clone();
        let upstream_url2 = upstream_url.clone();
        let cache_key2 = cache_key.clone();
        let path = uri.path().to_string();
        s.inflight
            .work(&cache_key, async move {
                fetch_and_cache(s2, upstream_url2, cache_key2, path).await
            })
            .await
    };

    if let Some(entry) = ok {
        s.stats.inc_proxied();
        info!(
            method = %method,
            path = %uri.path(),
            cache = "miss",
            status = entry.status,
            latency_us = start.elapsed().as_micros(),
        );
        return build_response(entry.status, &entry.content_type, entry.body);
    }

    match err {
        Some(503) => {
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
            resp
        }
        _ => {
            s.stats.inc_upstream_errors();
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Upstream helpers
// ---------------------------------------------------------------------------

/// Direct proxy for POST /batch and no-cache GET requests (no singleflight, no caching).
async fn direct_proxy(
    s: &AppState,
    method: &axum::http::Method,
    upstream_url: &str,
    body_bytes: axum::body::Bytes,
    req_content_type: String,
    send_body: bool,
    start: Instant,
    uri: &axum::http::Uri,
) -> Response {
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

    let upstream_req = s
        .client
        .request(method.clone(), upstream_url)
        .header("Authorization", format!("Bearer {key}"))
        .header("Accept", "application/json");

    let upstream_req = if send_body {
        upstream_req.header("Content-Type", req_content_type).body(body_bytes)
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
    if upstream_status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || upstream_status == reqwest::StatusCode::UNAUTHORIZED
    {
        s.rotator.mark_cooldown(&key);
        warn!(key = %mask_key(&key), status = upstream_status.as_u16(), "key marked for cooldown");
    }

    let content_type = upstream_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

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
    info!(
        method = %method,
        path = %uri.path(),
        cache = "miss",
        key = %mask_key(&key),
        status = status_u16,
        latency_us = start.elapsed().as_micros(),
    );

    build_response(status_u16, &content_type, resp_bytes)
}

/// Upstream fetch for cacheable GET requests, called inside the singleflight group.
/// Caches successful responses; returns Ok(entry) for any upstream response,
/// Err(503) if all keys exhausted, Err(502) on network failure.
async fn fetch_and_cache(
    s: AppState,
    upstream_url: String,
    cache_key: String,
    path: String,
) -> Result<CacheEntry, u16> {
    let key = match s.rotator.next_key() {
        Some(k) => k,
        None => {
            warn!(path = %path, "all keys exhausted");
            return Err(503);
        }
    };

    let upstream_resp = match s
        .client
        .get(&upstream_url)
        .header("Authorization", format!("Bearer {key}"))
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(key = %mask_key(&key), error = %e, "upstream request failed");
            return Err(502);
        }
    };

    let upstream_status = upstream_resp.status();
    if upstream_status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || upstream_status == reqwest::StatusCode::UNAUTHORIZED
    {
        s.rotator.mark_cooldown(&key);
        warn!(key = %mask_key(&key), status = upstream_status.as_u16(), "key marked for cooldown");
    }

    let content_type = upstream_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let resp_bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "failed to read upstream body");
            return Err(502);
        }
    };

    let entry = CacheEntry {
        body: resp_bytes,
        content_type,
        status: upstream_status.as_u16(),
    };

    if upstream_status.is_success() {
        if entry.body.len() > s.config.cache_max_body_bytes {
            debug!(
                path = %path,
                body_bytes = entry.body.len(),
                limit_bytes = s.config.cache_max_body_bytes,
                "skipping cache: response body exceeds size limit"
            );
        } else {
            s.cache
                .insert(cache_key, entry.body.clone(), entry.content_type.clone(), entry.status)
                .await;
        }
    }

    Ok(entry)
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
