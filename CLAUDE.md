# CLAUDE.md

## Project

romoto is a Rust CLI that shares a terminal session over SSH. It spawns any command (claude, opencode, codex, etc.) in a PTY and serves it to multiple SSH clients simultaneously.

## Build & Run

```bash
cargo build
cargo run -- claude
cargo run -- opencode -p 3000
```

## Architecture

- `src/main.rs` — CLI argument parsing, dispatches to server
- `src/server.rs` — SSH server (russh), PTY management (portable-pty), broadcast to clients

Key design:
- One PTY per session, shared by all connected clients (multiplayer)
- `tokio::task::spawn_blocking` for PTY reads, `broadcast` channel for fan-out
- PTY writer behind `std::sync::Mutex` (separate from async state) to avoid contention
- Auto-respawn on process exit with correct terminal size

## Dependencies

- `russh` — embedded SSH server
- `portable-pty` — cross-platform PTY
- `tokio` — async runtime
- `anyhow` — error handling
- `async-trait` — async trait support for russh Handler
- `rand` — session ID generation

## Conventions

- Pure PTY forwarding
- Logs go to stderr with `[romoto]` prefix
- Auth: only the generated session-id username is accepted
- Manual arg parsing (no clap) — keep dependencies minimal
