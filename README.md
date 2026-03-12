# ipinfo-round-robin-api

A lightweight HTTP proxy written in Rust that mirrors all [IPInfo](https://ipinfo.io) endpoints, rotates through multiple API keys using a round-robin strategy, and caches responses to minimise upstream calls and bypass rate limits.

Two deployment targets are available:

| | `server/` | `worker/` |
|---|---|---|
| Runtime | Tokio / Axum | Cloudflare Workers |
| Cache | In-memory LRU (moka) | Workers Cache API (edge) |
| State | Process-lifetime | Per-isolate (resets on cold start) |
| Deploy | Docker / bare metal | `wrangler deploy` |

---

## Features

- Transparent proxy for all IPInfo endpoints (`/`, `/me`, `/:ip`, `/:ip/:field`, `POST /batch`)
- Round-robin rotation across N API keys
- Automatic cooldown when a key hits `429` or `401`, with recovery after a configurable window
- `Cache-Control: no-cache` bypass support
- `POST /batch` and `GET /me` are never cached
- Optional key redaction in `/me` responses (`REDACT_KEYS=true`)
- `/health`, `/stats`, and `DELETE /cache` internal endpoints

---

## server/

Self-hosted binary. Runs anywhere Docker runs.

**Additional features over the worker:**
- In-memory LRU cache (TinyLFU) with per-entry TTL and max body size limit
- Singleflight coalescing — concurrent cache-miss requests for the same key collapse into one upstream call
- Persistent round-robin state and stats for the lifetime of the process
- Structured logging via `tracing` (text or JSON)

### Quick Start

```bash
cd server

# 1. Copy and fill in your keys
cp ../.env.example ../.env

# 2. Run
cargo run

# 3. Query
curl http://localhost:8080/8.8.8.8
curl http://localhost:8080/8.8.8.8/country
```

### Configuration

All configuration is via environment variables (or a `.env` file):

| Variable | Default | Description |
|---|---|---|
| `IPINFO_KEYS` | **required** | Comma-separated API keys |
| `PORT` | `8080` | HTTP listen port |
| `HOST` | `0.0.0.0` | Bind address |
| `COOLDOWN_SECONDS` | `60` | Seconds before retrying a rate-limited key |
| `REQUEST_TIMEOUT_MS` | `5000` | Timeout for upstream requests |
| `CACHE_TTL_SECONDS` | `300` | Cache entry time-to-live |
| `CACHE_MAX_ENTRIES` | `10000` | Max cache entries before LRU eviction |
| `CACHE_MAX_BODY_BYTES` | `32768` | Responses larger than this are not cached |
| `LOG_LEVEL` | `info` | `debug` / `info` / `warn` / `error` |
| `LOG_FORMAT` | `text` | `text` or `json` |
| `IPINFO_BASE_URL` | `https://ipinfo.io` | Override upstream (useful for testing) |
| `REDACT_KEYS` | `false` | Mask token field in `/me` responses |

### Docker

```bash
cd server
docker compose up
```

Or build manually:

```bash
docker build -t ipinfo-round-robin-api server/
docker run -e IPINFO_KEYS=your_key -p 8080:8080 ipinfo-round-robin-api
```

### Tests

```bash
cargo test -p ipinfo-round-robin-api
```

---

## worker/

Runs on [Cloudflare Workers](https://workers.cloudflare.com). No server required.

**Differences from the server:**
- Cache is backed by the [Workers Cache API](https://developers.cloudflare.com/workers/runtime-apis/cache/) — persists across isolate restarts, shared across edge locations in the same region
- Round-robin state and stats are per-isolate and reset on cold start
- `DELETE /cache` returns `501` — Workers Cache has no bulk-invalidation; entries expire via `Cache-Control max-age`
- No singleflight (Workers handles request coalescing at the infrastructure level)
- Optional [Analytics Engine](#analytics-engine) integration for per-request metrics

### Deploy

```bash
cd worker

# First time: authenticate
wrangler login

# Set your API keys as a secret (never hardcode them)
wrangler secret put IPINFO_KEYS
# Enter: key1,key2,key3

# Deploy
wrangler deploy
```

### Analytics Engine

Optional. Requires a [Workers paid plan](https://developers.cloudflare.com/workers/platform/pricing/).

Uncomment the binding in `wrangler.toml`:

```toml
[[analytics_engine_datasets]]
binding = "ANALYTICS"
dataset  = "ipinfo-round-robin_requests"
```

Each proxy request writes a data point you can query via [Workers Analytics Engine SQL](https://developers.cloudflare.com/analytics/analytics-engine/sql-api/):

| Field | Value |
|---|---|
| `index1` | Path type: `root`, `me`, `ip`, `ip_field`, `batch` |
| `double1` | HTTP status code |
| `double2` | `1` if served from cache, `0` if not |
| `double3` | `1` if an upstream call was made, `0` if not |
| `double4` | `1` if a key was put into cooldown (429/401), `0` if not |
| `double5` | Response size in bytes |
| `double6` | Key slot index (0-based); `-1` for cache hits and exhausted-key 503s |
| `double7` | Total request latency in milliseconds |

Example queries:

```sql
-- Cache hit rate by path type (last 24 h)
SELECT index1 AS path_type,
  SUM(_sample_interval * double2) / SUM(_sample_interval) AS cache_hit_rate,
  COUNT() AS requests
FROM `ipinfo-round-robin_requests`
WHERE timestamp > NOW() - INTERVAL '1' DAY
GROUP BY index1

-- Key cooldown events over time (5-minute buckets)
SELECT toStartOfInterval(timestamp, INTERVAL '5' MINUTE) AS t,
  SUM(_sample_interval * double4) AS cooldowns
FROM `ipinfo-round-robin_requests`
GROUP BY t ORDER BY t

-- Requests per key slot (rotation distribution)
SELECT double6 AS key_slot, COUNT() AS requests
FROM `ipinfo-round-robin_requests`
WHERE timestamp > NOW() - INTERVAL '1' HOUR
  AND double6 >= 0
GROUP BY key_slot ORDER BY key_slot

-- p50/p99 upstream latency vs cache latency
SELECT
  CASE WHEN double2 = 1 THEN 'cache' ELSE 'upstream' END AS source,
  quantileWeighted(0.5)(double7, _sample_interval)  AS p50_ms,
  quantileWeighted(0.99)(double7, _sample_interval) AS p99_ms
FROM `ipinfo-round-robin_requests`
WHERE timestamp > NOW() - INTERVAL '1' HOUR
GROUP BY source
```

If the binding is absent (free plan / not configured), writes are silently skipped — no errors, no impact on requests.

### Configuration

Secrets (sensitive — set via `wrangler secret put`):

| Secret | Description |
|---|---|
| `IPINFO_KEYS` | Comma-separated API keys |

Vars (non-sensitive — set in `wrangler.toml` under `[vars]`):

| Variable | Default | Description |
|---|---|---|
| `COOLDOWN_SECONDS` | `60` | Seconds before retrying a rate-limited key |
| `CACHE_TTL_SECONDS` | `300` | Cache entry time-to-live |
| `CACHE_MAX_BODY_BYTES` | `32768` | Responses larger than this are not cached |
| `IPINFO_BASE_URL` | `https://ipinfo.io` | Override upstream |
| `REDACT_KEYS` | `false` | Mask token field in `/me` responses |

---

## Endpoints

Both versions expose the same endpoints:

### Proxy (mirrors IPInfo)

| Method | Path | Description |
|---|---|---|
| `GET` | `/` | Caller's own IP info |
| `GET` | `/me` | Returns API key and usage (not cached) |
| `GET` | `/:ip` | Full info for an IP |
| `GET` | `/:ip/:field` | Single field (e.g. `/8.8.8.8/country`) |
| `POST` | `/batch` | Batch IP lookups (not cached) |

### Internal

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Key status (total / active / cooling) |
| `GET` | `/stats` | Request counts and key stats |
| `DELETE` | `/cache` | Flush cache (server only; 501 on worker) |

---

## License

MIT
