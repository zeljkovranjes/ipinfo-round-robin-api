mod cache;
mod config;
mod proxy;
mod rotator;
mod stats;
#[cfg(test)]
mod tests;

use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() {
    let config = match config::Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Configuration error: {e}");
            std::process::exit(1);
        }
    };

    // Initialise tracing
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&config.log_level));

    if config.log_format == "json" {
        fmt().json().with_env_filter(filter).init();
    } else {
        fmt().with_env_filter(filter).init();
    }

    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!(
        keys = config.keys.len(),
        cache_ttl = config.cache_ttl_seconds,
        cache_max = config.cache_max_entries,
        "starting ipinfo-round-robin-api on {addr}"
    );

    let state = proxy::AppState::new(config);
    let router = proxy::build_router(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("failed to bind {addr}: {e}");
            std::process::exit(1);
        });

    tracing::info!("listening on {addr}");

    axum::serve(listener, router)
        .await
        .expect("server error");
}
