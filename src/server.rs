use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use rand::Rng;
use russh::server::{Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, CryptoVec, Pty};
use tokio::sync::{Mutex, broadcast};

struct SharedState {
    pty_master: Box<dyn portable_pty::MasterPty + Send>,
    buffer: Vec<u8>,
}

pub fn run(cmd_name: &str) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(cmd_name))
}

async fn run_async(cmd_name: &str) -> Result<()> {
    let session_id = generate_session_id();
    let cwd = std::env::current_dir()?;

    let pty_system = NativePtySystem::default();
    let pty_pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(cmd_name);
    cmd.cwd(&cwd);

    let _child = pty_pair.slave.spawn_command(cmd)?;
    drop(pty_pair.slave);

    let pty_writer = Arc::new(std::sync::Mutex::new(pty_pair.master.take_writer()?));
    let mut reader = pty_pair.master.try_clone_reader()?;

    let state = Arc::new(Mutex::new(SharedState {
        pty_master: pty_pair.master,
        buffer: Vec::new(),
    }));

    // Broadcast channel for PTY output
    let (tx, _) = broadcast::channel::<Vec<u8>>(256);
    let tx_clone = tx.clone();

    // Read PTY in a blocking thread, send to broadcast channel
    let state_clone = state.clone();
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let data = buf[..n].to_vec();

            // Update replay buffer (best-effort, non-blocking)
            if let Ok(mut s) = state_clone.try_lock() {
                s.buffer.extend_from_slice(&data);
                if s.buffer.len() > 65536 {
                    let drain = s.buffer.len() - 65536;
                    s.buffer.drain(..drain);
                }
            }

            let _ = tx_clone.send(data);
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

    let port = 2222;
    println!("\x1b[1mromoto\x1b[0m session started");
    println!("Command: {cmd_name}");
    println!("Connect with: ssh {session_id}@localhost -p {port}");
    println!("Working directory: {}", cwd.display());

    let mut server = AppServer {
        state: state.clone(),
        pty_writer: pty_writer.clone(),
        broadcast_tx: tx,
        session_id: session_id.clone(),
    };

    server
        .run_on_address(Arc::new(config), ("0.0.0.0", port))
        .await?;

    Ok(())
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
    pty_writer: Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
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
            pty_writer: self.pty_writer.clone(),
            broadcast_tx: self.broadcast_tx.clone(),
            session_id: self.session_id.clone(),
            user: None,
        }
    }
}

struct ClientHandler {
    peer_addr: String,
    state: Arc<Mutex<SharedState>>,
    pty_writer: Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    broadcast_tx: broadcast::Sender<Vec<u8>>,
    session_id: String,
    user: Option<String>,
}

#[async_trait]
impl Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        if user == self.session_id {
            self.user = Some(user.to_string());
            eprintln!("[romoto] auth accepted (none) for user={user} from {}", self.peer_addr);
            Ok(Auth::Accept)
        } else {
            eprintln!("[romoto] auth rejected (none) for user={user} from {}", self.peer_addr);
            Ok(Auth::Reject { proceed_with_methods: None })
        }
    }

    async fn auth_password(&mut self, user: &str, _password: &str) -> Result<Auth, Self::Error> {
        if user == self.session_id {
            self.user = Some(user.to_string());
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
            self.user = Some(user.to_string());
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
        let state = self.state.lock().await;
        state.pty_master.resize(PtySize {
            rows: row_height as u16,
            cols: col_width as u16,
            pixel_width: 0,
            pixel_height: 0,
        })?;
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
        let state = self.state.lock().await;
        state.pty_master.resize(PtySize {
            rows: row_height as u16,
            cols: col_width as u16,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Ok(mut writer) = self.pty_writer.lock() {
            let _ = writer.write_all(data);
            let _ = writer.flush();
        }
        Ok(())
    }
}
