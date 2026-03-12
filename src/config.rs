use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub keys: Vec<String>,
    pub port: u16,
    pub host: String,
    pub cooldown_seconds: u64,
    pub request_timeout_ms: u64,
    pub cache_ttl_seconds: u64,
    pub cache_max_entries: usize,
    /// Responses larger than this (bytes) are not cached. Default 32 KiB.
    pub cache_max_body_bytes: usize,
    pub log_level: String,
    pub log_format: String,
    pub ipinfo_base_url: String,
    /// If true, redact API keys in /me responses (show only first 3 chars).
    pub redact_keys: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        // Load .env file if present (ignore error if missing)
        let _ = dotenvy::dotenv();

        let keys_raw = env::var("IPINFO_KEYS")
            .map_err(|_| "IPINFO_KEYS is required but not set".to_string())?;

        let keys: Vec<String> = keys_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if keys.is_empty() {
            return Err("IPINFO_KEYS must contain at least one key".to_string());
        }

        Ok(Config {
            keys,
            port: parse_env("PORT", 8080)?,
            host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            cooldown_seconds: parse_env("COOLDOWN_SECONDS", 60)?,
            request_timeout_ms: parse_env("REQUEST_TIMEOUT_MS", 5000)?,
            cache_ttl_seconds: parse_env("CACHE_TTL_SECONDS", 300)?,
            cache_max_entries: parse_env("CACHE_MAX_ENTRIES", 10000)?,
            cache_max_body_bytes: parse_env("CACHE_MAX_BODY_BYTES", 32 * 1024)?,
            log_level: env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
            log_format: env::var("LOG_FORMAT").unwrap_or_else(|_| "text".to_string()),
            ipinfo_base_url: env::var("IPINFO_BASE_URL")
                .unwrap_or_else(|_| "https://ipinfo.io".to_string()),
            redact_keys: env::var("REDACT_KEYS")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        })
    }
}

fn parse_env<T>(key: &str, default: T) -> Result<T, String>
where
    T: std::str::FromStr + std::fmt::Display,
    T::Err: std::fmt::Display,
{
    match env::var(key) {
        Ok(val) => val
            .parse::<T>()
            .map_err(|e| format!("Invalid value for {key}: {e}")),
        Err(_) => Ok(default),
    }
}

/// Mask an API key for safe logging: show last 3 chars only.
pub fn mask_key(key: &str) -> String {
    if key.len() <= 3 {
        return "***".to_string();
    }
    format!("***{}", &key[key.len() - 3..])
}
