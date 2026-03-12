use worker::*;

mod config;
mod proxy;
mod rotator;

#[event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let config = config::Config::from_env(&env)?;
    let state = proxy::AppState::new(config);
    proxy::handle(req, env, state).await
}
