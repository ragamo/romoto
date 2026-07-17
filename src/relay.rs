use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use russh::server::{Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, CryptoVec, Pty};
use tokio::sync::Mutex;

struct HostEntry {
    handle: russh::server::Handle,
}

struct RelayState {
    hosts: HashMap<String, HostEntry>,
}

pub fn run(port: u16, pass: Option<&str>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let pass_owned = pass.map(|s| s.to_string());
    rt.block_on(run_async(port, pass_owned))
}

async fn run_async(port: u16, pass: Option<String>) -> Result<()> {
    let state = Arc::new(Mutex::new(RelayState {
        hosts: HashMap::new(),
    }));

    let key = russh::keys::key::KeyPair::generate_ed25519();
    let config = russh::server::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
        auth_rejection_time: std::time::Duration::from_secs(3),
        auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
        keys: vec![key],
        ..Default::default()
    };

    println!("\x1b[1mromoto relay\x1b[0m listening on port {port}");
    if pass.is_some() {
        eprintln!("[relay] password protection enabled");
    } else {
        eprintln!("[relay] no password set — any host can register (use --pass to secure)");
    }

    let pass = pass.map(Arc::new);

    let mut server = RelayServer {
        state,
        pass,
    };

    tokio::select! {
        result = server.run_on_address(Arc::new(config), ("0.0.0.0", port)) => {
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\n[relay] shutting down");
            std::process::exit(0);
        }
    }

    Ok(())
}

// --- SSH Server ---

#[derive(Clone)]
struct RelayServer {
    state: Arc<Mutex<RelayState>>,
    pass: Option<Arc<String>>,
}

impl Server for RelayServer {
    type Handler = RelayHandler;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> RelayHandler {
        let peer_addr = peer.map(|p| p.to_string()).unwrap_or_else(|| "unknown".into());
        eprintln!("[relay] new connection from {peer_addr}");
        RelayHandler {
            peer_addr,
            state: self.state.clone(),
            pass: self.pass.clone(),
            role: ConnectionRole::Unknown,
            session_id: None,
            host_channel_info: None,
        }
    }
}

#[derive(Clone, Debug)]
enum ConnectionRole {
    Unknown,
    Host,
    Guest,
}

struct RelayHandler {
    peer_addr: String,
    state: Arc<Mutex<RelayState>>,
    pass: Option<Arc<String>>,
    role: ConnectionRole,
    session_id: Option<String>,
    host_channel_info: Option<(russh::server::Handle, ChannelId)>,
}

#[async_trait]
impl Handler for RelayHandler {
    type Error = anyhow::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        self.classify_user(user);
        match self.role {
            ConnectionRole::Host => {
                if self.pass.is_some() {
                    // Require password auth for hosts when pass is set
                    eprintln!("[relay] host requires password: session={} from {}", self.session_id.as_deref().unwrap_or("?"), self.peer_addr);
                    Ok(Auth::Reject { proceed_with_methods: None })
                } else {
                    eprintln!("[relay] host registered: session={} from {}", self.session_id.as_deref().unwrap_or("?"), self.peer_addr);
                    Ok(Auth::Accept)
                }
            }
            ConnectionRole::Guest => {
                let sid = self.session_id.as_deref().unwrap_or("");
                let state = self.state.lock().await;
                if state.hosts.contains_key(sid) {
                    eprintln!("[relay] guest accepted: session={sid} from {}", self.peer_addr);
                    Ok(Auth::Accept)
                } else {
                    eprintln!("[relay] guest rejected (no host): session={sid} from {}", self.peer_addr);
                    Ok(Auth::Reject { proceed_with_methods: None })
                }
            }
            _ => Ok(Auth::Reject { proceed_with_methods: None }),
        }
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.classify_user(user);
        match self.role {
            ConnectionRole::Host => {
                if let Some(ref expected) = self.pass {
                    if password == expected.as_str() {
                        eprintln!("[relay] host registered: session={} from {}", self.session_id.as_deref().unwrap_or("?"), self.peer_addr);
                        Ok(Auth::Accept)
                    } else {
                        eprintln!("[relay] host rejected (bad password): from {}", self.peer_addr);
                        Ok(Auth::Reject { proceed_with_methods: None })
                    }
                } else {
                    eprintln!("[relay] host registered: session={} from {}", self.session_id.as_deref().unwrap_or("?"), self.peer_addr);
                    Ok(Auth::Accept)
                }
            }
            ConnectionRole::Guest => {
                let sid = self.session_id.as_deref().unwrap_or("");
                let state = self.state.lock().await;
                if state.hosts.contains_key(sid) {
                    eprintln!("[relay] guest accepted: session={sid} from {}", self.peer_addr);
                    Ok(Auth::Accept)
                } else {
                    eprintln!("[relay] guest rejected (no host): session={sid} from {}", self.peer_addr);
                    Ok(Auth::Reject { proceed_with_methods: None })
                }
            }
            _ => Ok(Auth::Reject { proceed_with_methods: None }),
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        _key: &russh::keys::key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.classify_user(user);
        // Guests can auth with publickey (no password needed)
        if matches!(self.role, ConnectionRole::Guest) {
            let sid = self.session_id.as_deref().unwrap_or("");
            let state = self.state.lock().await;
            if state.hosts.contains_key(sid) {
                eprintln!("[relay] guest accepted (publickey): session={sid} from {}", self.peer_addr);
                return Ok(Auth::Accept);
            }
        }
        Ok(Auth::Reject { proceed_with_methods: None })
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        // Host registers itself when opening a session channel
        if matches!(self.role, ConnectionRole::Host) {
            if let Some(sid) = &self.session_id {
                let handle = session.handle();
                let mut state = self.state.lock().await;
                state.hosts.insert(sid.clone(), HostEntry { handle });
            }
        }
        Ok(true)
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel);
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if matches!(self.role, ConnectionRole::Host) {
            // Host's shell request — just acknowledge, keep channel open
            session.channel_success(channel);
            return Ok(());
        }

        // Guest: open a forwarded channel on the host and pipe data
        let sid = self.session_id.clone().unwrap_or_default();
        let host_handle = {
            let state = self.state.lock().await;
            state.hosts.get(&sid).map(|h| h.handle.clone())
        };

        let Some(host_handle) = host_handle else {
            eprintln!("[relay] no host found for session={sid}");
            session.channel_failure(channel);
            return Ok(());
        };

        // Open a forwarded-tcpip channel to the host
        let host_channel = host_handle
            .channel_open_forwarded_tcpip("relay", 0, &self.peer_addr, 0)
            .await;

        let Ok(host_channel) = host_channel else {
            eprintln!("[relay] failed to open channel to host for session={sid}");
            session.channel_failure(channel);
            return Ok(());
        };

        // Pipe: host channel output → guest
        let guest_handle = session.handle();
        let guest_channel_id = channel;
        let host_channel_id = host_channel.id();

        let peer = self.peer_addr.clone();
        let sid_clone = sid.clone();
        let mut host_stream = host_channel.into_stream();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 8192];
            loop {
                match host_stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if guest_handle.data(guest_channel_id, CryptoVec::from(buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            eprintln!("[relay] guest disconnected: session={sid_clone} from {peer}");
        });

        // Store host info for forwarding guest input in data()
        self.host_channel_info = Some((host_handle.clone(), host_channel_id));

        session.channel_success(channel);
        eprintln!("[relay] guest connected to session={sid} from {}", self.peer_addr);
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
        // Forward resize to host as a special escape sequence
        if let Some((ref host_handle, host_channel_id)) = self.host_channel_info {
            // Send a resize notification as raw bytes — the host will parse it
            let resize_msg = format!("\x1b]romoto;resize;{col_width};{row_height}\x07");
            let _ = host_handle.data(host_channel_id, CryptoVec::from(resize_msg.into_bytes())).await;
        }
        Ok(())
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.role {
            ConnectionRole::Guest => {
                // Forward guest input to host channel
                if let Some((ref host_handle, host_channel_id)) = self.host_channel_info {
                    let _ = host_handle.data(host_channel_id, CryptoVec::from(data.to_vec())).await;
                }
            }
            ConnectionRole::Host => {
                // Host data goes nowhere on the relay (host uses forwarded channels)
            }
            _ => {}
        }
        Ok(())
    }
}

impl RelayHandler {
    fn classify_user(&mut self, user: &str) {
        if let Some(sid) = user.strip_prefix("host:") {
            self.role = ConnectionRole::Host;
            self.session_id = Some(sid.to_string());
        } else {
            self.role = ConnectionRole::Guest;
            self.session_id = Some(user.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handler(pass: Option<&str>) -> RelayHandler {
        RelayHandler {
            peer_addr: "127.0.0.1:1234".into(),
            state: Arc::new(Mutex::new(RelayState { hosts: HashMap::new() })),
            pass: pass.map(|s| Arc::new(s.to_string())),
            role: ConnectionRole::Unknown,
            session_id: None,
            host_channel_info: None,
        }
    }

    #[test]
    fn test_classify_user_host() {
        let mut h = make_handler(None);
        h.classify_user("host:abc123");
        assert!(matches!(h.role, ConnectionRole::Host));
        assert_eq!(h.session_id.as_deref(), Some("abc123"));
    }

    #[test]
    fn test_classify_user_guest() {
        let mut h = make_handler(None);
        h.classify_user("abc123");
        assert!(matches!(h.role, ConnectionRole::Guest));
        assert_eq!(h.session_id.as_deref(), Some("abc123"));
    }

    #[test]
    fn test_classify_user_host_empty_sid() {
        let mut h = make_handler(None);
        h.classify_user("host:");
        assert!(matches!(h.role, ConnectionRole::Host));
        assert_eq!(h.session_id.as_deref(), Some(""));
    }

    #[test]
    fn test_classify_user_not_host_prefix() {
        let mut h = make_handler(None);
        h.classify_user("hosting");
        assert!(matches!(h.role, ConnectionRole::Guest));
        assert_eq!(h.session_id.as_deref(), Some("hosting"));
    }

    #[tokio::test]
    async fn test_auth_none_host_no_pass() {
        let mut h = make_handler(None);
        let result = h.auth_none("host:session1").await.unwrap();
        assert!(matches!(result, Auth::Accept));
    }

    #[tokio::test]
    async fn test_auth_none_host_with_pass_rejects() {
        let mut h = make_handler(Some("secret"));
        let result = h.auth_none("host:session1").await.unwrap();
        assert!(matches!(result, Auth::Reject { .. }));
    }

    #[tokio::test]
    async fn test_auth_password_host_correct() {
        let mut h = make_handler(Some("secret"));
        let result = h.auth_password("host:session1", "secret").await.unwrap();
        assert!(matches!(result, Auth::Accept));
    }

    #[tokio::test]
    async fn test_auth_password_host_wrong() {
        let mut h = make_handler(Some("secret"));
        let result = h.auth_password("host:session1", "wrong").await.unwrap();
        assert!(matches!(result, Auth::Reject { .. }));
    }

    #[tokio::test]
    async fn test_auth_guest_no_host_registered() {
        let mut h = make_handler(None);
        let result = h.auth_none("session1").await.unwrap();
        assert!(matches!(result, Auth::Reject { .. }));
    }
}
