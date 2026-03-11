# ipinfo-round-robin-api

A lightweight HTTP proxy written in Rust that mirrors all [IPInfo](https://ipinfo.io) endpoints, rotates through multiple API keys using a round-robin strategy, and caches responses in-memory to minimise upstream calls and bypass rate limits.

## Features

- Transparent proxy for all IPInfo endpoints
- Round-robin rotation across N API keys
- Automatic cooldown when a key hits `429` or `401`, with recovery after a configurable window
- In-memory LRU cache (TinyLFU) with per-entry TTL and max body size limit
- Singleflight coalescing — concurrent cache-miss requests for the same key are collapsed into one upstream call
- `Cache-Control: no-cache` bypass support
- `POST /batch` and `GET /me` never cached (not cacheable)
- `/health`, `/stats`, and `DELETE /cache` internal endpoints
- Structured logging via `tracing` (text or JSON)
- Single static binary, minimal allocations

## Quick Start

```bash
# 1. Copy and fill in your keys
cp .env.example .env

# 2. Run
cargo run

# 3. Query
curl http://localhost:8080/8.8.8.8
curl http://localhost:8080/8.8.8.8/country
```

## Configuration

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
| `LOG_LEVEL` | `info` | `debug` / `info` / `warn` / `error` |
| `LOG_FORMAT` | `text` | `text` or `json` |
| `IPINFO_BASE_URL` | `https://ipinfo.io` | Override upstream (useful for testing) |

## Endpoints

### Proxy (mirrors IPInfo)

| Method | Path | Description |
|---|---|---|
| `GET` | `/` | Caller's own IP info |
| `GET` | `/me` | Returns API key and usage (misleading path ik) |
| `GET` | `/:ip` | Full info for an IP |
| `GET` | `/:ip/:field` | Single field (e.g. `/8.8.8.8/country`) |
| `POST` | `/batch` | Batch IP lookups (not cached) |

### Internal

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Key status (total / active / cooling) |
| `GET` | `/stats` | Request counts, cache stats, key stats |
| `DELETE` | `/cache` | Flush entire cache |

## Docker

```bash
docker compose up
```

Or build manually:

```bash
docker build -t ipinfo-round-robin-api .
docker run -e IPINFO_KEYS=your_key -p 8080:8080 ipinfo-round-robin-api
```

## Running Tests

```bash
cargo test
```

## License

MIT
