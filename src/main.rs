mod config;
mod server;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--config") {
        config::run()
    } else {
        server::run()
    }
}
