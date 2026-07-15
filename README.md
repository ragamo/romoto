# `romoto`

Share a terminal session over SSH. Run any CLI tool and let others join via SSH — multiplayer, real-time.

## Install

### Homebrew (macOS/Linux)

```bash
brew install ragamo/tap/romoto
```

### From source

```bash
cargo install --path .
```

## Usage

```bash
romoto <command> [options]
```

### Examples

```bash
# Share a Claude Code session
romoto claude

# Share opencode on a custom port
romoto opencode -p 3000

# Share any interactive CLI
romoto vim
```

On startup, `romoto` prints a connection string:

```
romoto session started
Command: claude
Connect with: ssh abc12345@localhost -p 2222
Working directory: /home/user/project
```

### Connect

From another machine (or terminal):

```bash
ssh abc12345@localhost -p 2222
```

The session ID (`abc12345`) acts as both the username and access token. Multiple users can connect simultaneously and share the same session.

### Options

| Flag | Description |
|------|-------------|
| `-p, --port <n>` | SSH port (default: 2222) |
| `--relay <host>` | Connect to a relay server |
| `-v, --version` | Show version |
| `-h, --help` | Show help |

## Features

- **Multiplayer** — multiple SSH clients see and interact with the same session
- **Auto-restart** — if the command exits, it restarts automatically
- **Buffer replay** — new clients see the current terminal state on connect
- **Zero config** — no SSH keys to manage, no accounts to create
- **Relay** — share sessions remotely without exposing your IP

## Relay

`romoto relay` runs on a VPS and routes guests to hosts. Hosts connect outbound to the relay, so no port forwarding or public IP is needed on the host side.

### Setup

```bash
# On your VPS (e.g. romoto.yourdomain.com)
romoto relay -p 2222
```

### Host (your machine)

```bash
romoto claude --relay romoto.yourdomain.com:2222
```

`romoto` connects to the relay, registers its session, and prints:

```
romoto session started
Command: claude
Relay: romoto.yourdomain.com:2222
Connect with: ssh abc12345@romoto.yourdomain.com -p 2222
```

### Guest

```bash
ssh abc12345@romoto.yourdomain.com -p 2222
```

The relay looks up the session ID and routes the connection to the correct host. Multiple hosts can share a single relay — each gets its own session ID.

## Sharing remotely (without relay)

`romoto` listens on localhost by default. To share with others over the internet without a relay:

### Tailscale

If both machines are on the same Tailnet, just connect using the Tailscale IP or hostname:

```bash
# Host
romoto claude

# Guest
ssh abc12345@my-machine -p 2222
```

No extra config needed — Tailscale handles the networking.

### ngrok

```bash
# Host
romoto claude
ngrok tcp 2222

# Guest (use the ngrok-provided host:port)
ssh abc12345@0.tcp.ngrok.io -p 12345
```

## How it works

1. Spawns the command in a PTY
2. Starts an embedded SSH server (no system sshd needed)
3. Generates a random session ID for authentication
4. Broadcasts PTY output to all connected clients
5. Forwards input from any client to the PTY

With `--relay`, `romoto` also connects as an SSH client to the relay server. The relay opens forwarded channels for each guest and pipes data bidirectionally.

## License

MIT
