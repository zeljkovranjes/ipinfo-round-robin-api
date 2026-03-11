mod cache;
mod config;
mod rotator;

fn main() {
    match config::Config::from_env() {
        Ok(cfg) => println!("Config loaded. Keys: {}", cfg.keys.len()),
        Err(e) => eprintln!("Config error: {e}"),
    }
}
