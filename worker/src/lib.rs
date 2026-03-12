use worker::*;

mod config;
mod proxy;
mod rotator;

#[event(fetch)]
async fn main(req: HttpRequest, env: Env, _ctx: Context) -> Result<axum::response::Response> {
    let config = config::Config::from_env(&env)?;
    let state = proxy::AppState::new(config);
    let router = proxy::build_router(state);

    use tower::ServiceExt;
    // Router<()> implements Service with Error = Infallible, so unwrap is safe
    Ok(router.oneshot(req).await.unwrap_or_else(|i| match i {}))
}
