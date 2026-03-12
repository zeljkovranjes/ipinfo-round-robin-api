use worker::Env;

#[derive(Debug, Clone)]
pub struct Config {
    pub keys: Vec<String>,
    pub cooldown_seconds: u64,
    pub cache_ttl_seconds: u64,
    /// Responses larger than this (bytes) are not cached. Default 32 KiB.
    pub cache_max_body_bytes: usize,
    pub ipinfo_base_url: String,
    /// If true, redact API keys in /me responses (show only first 3 chars).
    pub redact_keys: bool,
}

impl Config {
    pub fn from_env(env: &Env) -> worker::Result<Self> {
        // IPINFO_KEYS should be stored as a secret; fall back to var for local dev
        let keys_raw = env
            .secret("IPINFO_KEYS")
            .map(|v| v.to_string())
            .or_else(|_| env.var("IPINFO_KEYS").map(|v| v.to_string()))
            .map_err(|_| {
                worker::Error::RustError("IPINFO_KEYS is required but not set".to_string())
            })?;

        let keys: Vec<String> = keys_raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if keys.is_empty() {
            return Err(worker::Error::RustError(
                "IPINFO_KEYS must contain at least one key".to_string(),
            ));
        }

        Ok(Config {
            keys,
            cooldown_seconds: get_var_parsed(env, "COOLDOWN_SECONDS", 60),
            cache_ttl_seconds: get_var_parsed(env, "CACHE_TTL_SECONDS", 300),
            cache_max_body_bytes: get_var_parsed(env, "CACHE_MAX_BODY_BYTES", 32 * 1024),
            ipinfo_base_url: env
                .var("IPINFO_BASE_URL")
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "https://ipinfo.io".to_string()),
            redact_keys: env
                .var("REDACT_KEYS")
                .map(|v| v.to_string().eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        })
    }
}

fn get_var_parsed<T: std::str::FromStr>(env: &Env, key: &str, default: T) -> T {
    env.var(key)
        .ok()
        .and_then(|v| v.to_string().parse().ok())
        .unwrap_or(default)
}

/// Mask an API key for safe logging: show last 3 chars only.
pub fn mask_key(key: &str) -> String {
    if key.len() <= 3 {
        return "***".to_string();
    }
    format!("***{}", &key[key.len() - 3..])
}
