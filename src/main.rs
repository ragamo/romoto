mod server;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "-v" || a == "--version") {
        println!("romoto {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("romoto - Share a terminal session over SSH");
        println!();
        println!("Usage: romoto <command> [options]");
        println!();
        println!("Arguments:");
        println!("  <command>        Command to run (e.g. claude, opencode, codex)");
        println!();
        println!("Options:");
        println!("  -p, --port <n>   SSH port to listen on (default: 2222)");
        println!("  -v, --version    Show version");
        println!("  -h, --help       Show this help");
        return Ok(());
    }

    let port_pos = args.iter().position(|a| a == "-p" || a == "--port");
    let port: u16 = port_pos
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(2222);

    let cmd = args.iter()
        .enumerate()
        .skip(1)
        .find(|(i, a)| {
            !a.starts_with('-')
                && port_pos.map_or(true, |p| *i != p + 1)
        })
        .map(|(_, s)| s.as_str());

    let Some(cmd) = cmd else {
        eprintln!("Error: command is required");
        eprintln!();
        eprintln!("Example: romoto claude");
        std::process::exit(1);
    };

    server::run(cmd, port)
}
