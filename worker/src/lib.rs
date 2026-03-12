use worker::*;

#[event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let _ = (req, env);
    Response::ok("ipinfo-round-robin-worker")
}
