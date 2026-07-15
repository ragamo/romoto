use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use rand::Rng;
use russh::server::{Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, CryptoVec, Pty};
use tokio::sync::{Mutex, broadcast, Notify};

struct SharedState {
    pty_master: Box<dyn portable_pty::MasterPty + Send>,
    pty_writer: Box<dyn Write + Send>,
    buffer: Vec<u8>,
    last_size: PtySize,
}

pub fn run(cmd_name: &str, port: u16, relay_host: Option<&str>, relay_pass: Option<&str>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cmd_name, port, relay_host, relay_pass))
}

fn spawn_pty(cmd_name: &str, cwd: &std::path::Path) -> Result<(Box<dyn portable_pty::MasterPty + Send>, Box<dyn Write + Send>, Box<dyn Read + Send>)> {
    let pty_system = NativePtySystem::default();
    let pty_pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(cmd_name);
    cmd.cwd(cwd);

    let _child = pty_pair.slave.spawn_command(cmd)?;
    drop(pty_pair.slave);

    let writer = pty_pair.master.take_writer()?;
    let reader = pty_pair.master.try_clone_reader()?;

    Ok((pty_pair.master, writer, reader))
}

async fn run_async(cmd_name: &str, port: u16, relay_host: Option<&str>, relay_pass: Option<&str>) -> Result<()> {
    let session_id = generate_session_id();
    let cwd = std::env::current_dir()?;

    let (master, writer, reader) = spawn_pty(cmd_name, &cwd)?;

    let state = Arc::new(Mutex::new(SharedState {
        pty_master: master,
        pty_writer: writer,
        buffer: Vec::new(),
        last_size: PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 },
    }));

    let (tx, _) = broadcast::channel::<Vec<u8>>(256);
    let respawn_notify = Arc::new(Notify::new());

    // Start the PTY reader loop
    start_reader(reader, state.clone(), tx.clone(), respawn_notify.clone());

    // Respawn loop: when the process exits, restart it
    let state_clone = state.clone();
    let tx_clone = tx.clone();
    let respawn_notify_clone = respawn_notify.clone();
    let cmd_owned = cmd_name.to_string();
    let cwd_clone = cwd.clone();
    tokio::spawn(async move {
        loop {
            respawn_notify_clone.notified().await;
            eprintln!("[romoto] process exited, restarting...");
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            match spawn_pty(&cmd_owned, &cwd_clone) {
                Ok((master, writer, reader)) => {
                    let size;
                    {
                        let mut s = state_clone.lock().await;
                        size = s.last_size;
                        s.pty_master = master;
                        s.pty_writer = writer;
                        s.buffer.clear();
                    }
                    // Resize new PTY to match last known client size
                    {
                        let s = state_clone.lock().await;
                        let _ = s.pty_master.resize(size);
                    }
                    // Clear connected clients' screens
                    let _ = tx_clone.send(b"\x1b[2J\x1b[H".to_vec());
                    start_reader(reader, state_clone.clone(), tx_clone.clone(), respawn_notify_clone.clone());
                    eprintln!("[romoto] process restarted");
                }
                Err(e) => {
                    eprintln!("[romoto] failed to restart process: {e}");
                }
            }
        }
    });

    let key = russh::keys::key::KeyPair::generate_ed25519();
    let config = russh::server::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
        auth_rejection_time: std::time::Duration::from_secs(3),
        auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
        keys: vec![key],
        ..Default::default()
    };

    println!("\x1b[1mromoto\x1b[0m session started");
    println!("Command: {cmd_name}");
    if let Some(relay) = relay_host {
        println!("Relay: {relay}");
        println!("Connect with: ssh {session_id}@{relay}");
    } else {
        println!("Connect with: ssh {session_id}@localhost -p {port}");
    }
    println!("Working directory: {}", cwd.display());

    // Connect to relay if specified
    if let Some(relay) = relay_host {
        let relay_addr = if relay.contains(':') {
            relay.to_string()
        } else {
            format!("{relay}:22")
        };
        let session_id_clone = session_id.clone();
        let state_clone = state.clone();
        let tx_clone = tx.clone();
        let pass_owned = relay_pass.map(|s| s.to_string());
        if pass_owned.is_some() {
            eprintln!("[romoto] relay password set");
        } else {
            eprintln!("[romoto] no relay password");
        }
        tokio::spawn(async move {
            if let Err(e) = connect_to_relay(&relay_addr, &session_id_clone, pass_owned.as_deref(), state_clone, tx_clone).await {
                eprintln!("[romoto] relay connection failed: {e}");
            }
        });
    }

    let mut server = AppServer {
        state: state.clone(),
        broadcast_tx: tx,
        session_id: session_id.clone(),
    };

    server
        .run_on_address(Arc::new(config), ("0.0.0.0", port))
        .await?;

    Ok(())
}

fn start_reader(
    mut reader: Box<dyn Read + Send>,
    state: Arc<Mutex<SharedState>>,
    tx: broadcast::Sender<Vec<u8>>,
    respawn_notify: Arc<Notify>,
) {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let data = buf[..n].to_vec();

            if let Ok(mut s) = state.try_lock() {
                s.buffer.extend_from_slice(&data);
                if s.buffer.len() > 65536 {
                    let drain = s.buffer.len() - 65536;
                    s.buffer.drain(..drain);
                }
            }

            let _ = tx.send(data);
        }
        respawn_notify.notify_one();
    });
}

fn generate_session_id() -> String {
    let mut rng = rand::thread_rng();
    let chars: Vec<char> = "abcdefghijklmnopqrstuvwxyz0123456789".chars().collect();
    (0..8).map(|_| chars[rng.gen_range(0..chars.len())]).collect()
}

// --- SSH Server ---

#[derive(Clone)]
struct AppServer {
    state: Arc<Mutex<SharedState>>,
    broadcast_tx: broadcast::Sender<Vec<u8>>,
    session_id: String,
}

impl Server for AppServer {
    type Handler = ClientHandler;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> ClientHandler {
        let peer_addr = peer.map(|p| p.to_string()).unwrap_or_else(|| "unknown".into());
        eprintln!("[romoto] new connection from {peer_addr}");
        ClientHandler {
            peer_addr,
            state: self.state.clone(),
            broadcast_tx: self.broadcast_tx.clone(),
            session_id: self.session_id.clone(),
        }
    }
}

struct ClientHandler {
    peer_addr: String,
    state: Arc<Mutex<SharedState>>,
    broadcast_tx: broadcast::Sender<Vec<u8>>,
    session_id: String,
}

#[async_trait]
impl Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        if user == self.session_id {
            eprintln!("[romoto] auth accepted (none) for user={user} from {}", self.peer_addr);
            Ok(Auth::Accept)
        } else {
            eprintln!("[romoto] auth rejected (none) for user={user} from {}", self.peer_addr);
            Ok(Auth::Reject { proceed_with_methods: None })
        }
    }

    async fn auth_password(&mut self, user: &str, _password: &str) -> Result<Auth, Self::Error> {
        if user == self.session_id {
            eprintln!("[romoto] auth accepted (password) for user={user} from {}", self.peer_addr);
            Ok(Auth::Accept)
        } else {
            eprintln!("[romoto] auth rejected (password) for user={user} from {}", self.peer_addr);
            Ok(Auth::Reject { proceed_with_methods: None })
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        _key: &russh::keys::key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        if user == self.session_id {
            eprintln!("[romoto] auth accepted (publickey) for user={user} from {}", self.peer_addr);
            Ok(Auth::Accept)
        } else {
            eprintln!("[romoto] auth rejected (publickey) for user={user} from {}", self.peer_addr);
            Ok(Auth::Reject { proceed_with_methods: None })
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let size = PtySize {
            rows: row_height as u16,
            cols: col_width as u16,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = self.state.lock().await;
        state.pty_master.resize(size)?;
        state.last_size = size;
        session.channel_success(channel);
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let handle = session.handle();

        // Replay buffer so new client sees current state
        {
            let state = self.state.lock().await;
            if !state.buffer.is_empty() {
                let _ = handle.data(channel, CryptoVec::from(state.buffer.clone())).await;
            }
        }

        // Spawn a task that forwards broadcast output to this client
        let mut rx = self.broadcast_tx.subscribe();
        let client_handle = handle.clone();
        let client_channel = channel;
        let peer = self.peer_addr.clone();
        eprintln!("[romoto] client joined session from {peer}");
        tokio::spawn(async move {
            while let Ok(data) = rx.recv().await {
                if client_handle.data(client_channel, CryptoVec::from(data)).await.is_err() {
                    break;
                }
            }
            eprintln!("[romoto] client disconnected from {peer}");
        });

        session.channel_success(channel);
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let size = PtySize {
            rows: row_height as u16,
            cols: col_width as u16,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut state = self.state.lock().await;
        state.pty_master.resize(size)?;
        state.last_size = size;
        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().await;
        let _ = state.pty_writer.write_all(data);
        let _ = state.pty_writer.flush();
        Ok(())
    }
}

// --- Relay client ---

async fn connect_to_relay(
    relay_addr: &str,
    session_id: &str,
    pass: Option<&str>,
    state: Arc<Mutex<SharedState>>,
    broadcast_tx: broadcast::Sender<Vec<u8>>,
) -> Result<()> {
    let config = Arc::new(russh::client::Config::default());
    let handler = RelayClientHandler {
        state,
        broadcast_tx,
    };

    let mut session = russh::client::connect(config, relay_addr, handler).await?;
    let user = format!("host:{session_id}");
    let auth_result = if let Some(password) = pass {
        session.authenticate_password(&user, password).await?
    } else {
        session.authenticate_none(&user).await?
    };
    if !auth_result {
        anyhow::bail!("relay auth rejected — check --pass");
    }

    eprintln!("[romoto] connected to relay");

    // Open a session channel to register with the relay
    let channel = session.channel_open_session().await?;
    channel.request_shell(false).await?;

    // Keep the connection alive — the relay will open forwarded channels for guests
    // The client handler's server_channel_open_forwarded_tcpip handles incoming guests
    tokio::signal::ctrl_c().await?;
    Ok(())
}

struct RelayClientHandler {
    state: Arc<Mutex<SharedState>>,
    broadcast_tx: broadcast::Sender<Vec<u8>>,
}

#[async_trait]
impl russh::client::Handler for RelayClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<russh::client::Msg>,
        _connected_address: &str,
        _connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut russh::client::Session,
    ) -> Result<(), Self::Error> {
        eprintln!("[romoto] guest connected via relay");

        let stream = channel.into_stream();
        let (mut read_half, mut write_half) = tokio::io::split(stream);
        let state = self.state.clone();
        let mut rx = self.broadcast_tx.subscribe();

        // Forward PTY broadcast → guest (via relay channel)
        let state_clone = state.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            // Send current buffer first
            {
                let s = state_clone.lock().await;
                if !s.buffer.is_empty() {
                    if write_half.write_all(&s.buffer).await.is_err() {
                        return;
                    }
                }
            }
            // Then forward broadcast
            while let Ok(data) = rx.recv().await {
                if write_half.write_all(&data).await.is_err() {
                    break;
                }
            }
        });

        // Forward guest input → PTY
        let state2 = self.state.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 8192];
            loop {
                match read_half.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = &buf[..n];
                        if let Some(resize) = parse_resize_escape(data) {
                            let mut s = state2.lock().await;
                            let size = PtySize {
                                rows: resize.1,
                                cols: resize.0,
                                pixel_width: 0,
                                pixel_height: 0,
                            };
                            let _ = s.pty_master.resize(size);
                            s.last_size = size;
                        } else {
                            let mut s = state2.lock().await;
                            let _ = s.pty_writer.write_all(data);
                            let _ = s.pty_writer.flush();
                        }
                    }
                    Err(_) => break,
                }
            }
            eprintln!("[romoto] guest disconnected from relay");
        });

        Ok(())
    }
}

fn parse_resize_escape(data: &[u8]) -> Option<(u16, u16)> {
    let s = std::str::from_utf8(data).ok()?;
    let s = s.strip_prefix("\x1b]romoto;resize;")?;
    let s = s.strip_suffix('\x07')?;
    let mut parts = s.split(';');
    let cols: u16 = parts.next()?.parse().ok()?;
    let rows: u16 = parts.next()?.parse().ok()?;
    Some((cols, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_session_id_length() {
        let id = generate_session_id();
        assert_eq!(id.len(), 8);
    }

    #[test]
    fn test_generate_session_id_charset() {
        let id = generate_session_id();
        assert!(id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn test_generate_session_id_unique() {
        let a = generate_session_id();
        let b = generate_session_id();
        assert_ne!(a, b);
    }

    #[test]
    fn test_parse_resize_escape_valid() {
        let data = b"\x1b]romoto;resize;120;40\x07";
        assert_eq!(parse_resize_escape(data), Some((120, 40)));
    }

    #[test]
    fn test_parse_resize_escape_small() {
        let data = b"\x1b]romoto;resize;80;24\x07";
        assert_eq!(parse_resize_escape(data), Some((80, 24)));
    }

    #[test]
    fn test_parse_resize_escape_invalid_prefix() {
        let data = b"\x1b[romoto;resize;80;24\x07";
        assert_eq!(parse_resize_escape(data), None);
    }

    #[test]
    fn test_parse_resize_escape_no_suffix() {
        let data = b"\x1b]romoto;resize;80;24";
        assert_eq!(parse_resize_escape(data), None);
    }

    #[test]
    fn test_parse_resize_escape_not_numbers() {
        let data = b"\x1b]romoto;resize;abc;def\x07";
        assert_eq!(parse_resize_escape(data), None);
    }

    #[test]
    fn test_parse_resize_escape_random_data() {
        let data = b"hello world";
        assert_eq!(parse_resize_escape(data), None);
    }
}
