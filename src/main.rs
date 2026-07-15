mod config;
mod server;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--config") {
        config::run()
    } else {
        let cmd = args.iter()
            .position(|a| a == "--cmd")
            .and_then(|i| args.get(i + 1))
            .map(|s| s.as_str())
            .unwrap_or("claude");
        server::run(cmd)
    }
}
