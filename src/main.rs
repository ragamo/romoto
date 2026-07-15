use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use russh::server::{Auth, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, CryptoVec, Pty};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::Mutex;

// --- Mouse parsing ---

#[derive(Debug)]
enum MouseEvent {
    Press { button: u8, col: u16, row: u16 },
    #[allow(dead_code)]
    Release { col: u16, row: u16 },
}

fn parse_mouse_events(data: &[u8]) -> (Vec<MouseEvent>, Vec<u8>) {
    let mut events = Vec::new();
    let mut remaining = Vec::new();
    let mut i = 0;

    while i < data.len() {
        // SGR mouse: \x1b[<Cb;Cx;CyM (press) or \x1b[<Cb;Cx;Cym (release)
        if i + 2 < data.len() && data[i] == 0x1b && data[i + 1] == b'[' && data[i + 2] == b'<' {
            let start = i + 3;
            let mut end = start;
            while end < data.len() && data[end] != b'M' && data[end] != b'm' {
                end += 1;
            }
            if end < data.len() {
                let params: Vec<&str> = std::str::from_utf8(&data[start..end])
                    .unwrap_or("")
                    .split(';')
                    .collect();
                if params.len() == 3 {
                    let button: u8 = params[0].parse().unwrap_or(0);
                    let col: u16 = params[1].parse::<u16>().unwrap_or(1).saturating_sub(1);
                    let row: u16 = params[2].parse::<u16>().unwrap_or(1).saturating_sub(1);
                    if data[end] == b'M' {
                        events.push(MouseEvent::Press { button, col, row });
                    } else {
                        events.push(MouseEvent::Release { col, row });
                    }
                }
                i = end + 1;
                continue;
            }
        }
        remaining.push(data[i]);
        i += 1;
    }

    (events, remaining)
}

// --- App state per client ---

struct App {
    active_tab: usize,
    tab_titles: Vec<&'static str>,
}

impl App {
    fn new() -> Self {
        Self {
            active_tab: 0,
            tab_titles: vec!["Tab 1", "Tab 2"],
        }
    }

    fn handle_click(&mut self, col: u16, row: u16) {
        // Tab bar is row 1 (inside the border of the top block at row 0)
        if row != 1 {
            return;
        }
        // Tabs rendered with " Title1 │ Title2 " pattern inside the border
        // Border starts at col 0, content at col 1
        // ratatui Tabs uses: " {title} " separated by "│"
        let mut x: u16 = 1; // start after left border
        for (i, title) in self.tab_titles.iter().enumerate() {
            // Each tab takes: space + title + space = len + 2, then separator "│" (except last)
            let tab_width = title.len() as u16 + 2;
            if col >= x && col < x + tab_width {
                self.active_tab = i;
                return;
            }
            x += tab_width + 1; // +1 for the "│" separator
        }
    }
}

// --- Terminal handle: bridges ratatui writes to the SSH channel ---

struct TerminalHandle {
    sender: UnboundedSender<Vec<u8>>,
    sink: Vec<u8>,
}

impl TerminalHandle {
    fn new(handle: russh::server::Handle, channel_id: ChannelId) -> Self {
        let (sender, mut receiver) = unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(data) = receiver.recv().await {
                let _ = handle.data(channel_id, CryptoVec::from(data)).await;
            }
        });
        Self {
            sender,
            sink: Vec::new(),
        }
    }
}

impl Write for TerminalHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.sink.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sender
            .send(self.sink.clone())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
        self.sink.clear();
        Ok(())
    }
}

type SshTerminal = Terminal<CrosstermBackend<TerminalHandle>>;

// Enable SGR mouse tracking
const ENABLE_MOUSE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const DISABLE_MOUSE: &[u8] = b"\x1b[?1006l\x1b[?1002l\x1b[?1000l";

// --- SSH Server ---

#[derive(Clone)]
struct AppServer {
    clients: Arc<Mutex<HashMap<usize, (SshTerminal, App)>>>,
    next_id: usize,
}

impl Server for AppServer {
    type Handler = ClientHandler;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> ClientHandler {
        let id = self.next_id;
        self.next_id += 1;
        ClientHandler {
            id,
            clients: self.clients.clone(),
        }
    }
}

struct ClientHandler {
    id: usize,
    clients: Arc<Mutex<HashMap<usize, (SshTerminal, App)>>>,
}

#[async_trait]
impl Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _key: &russh::keys::key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let handle = session.handle();
        let terminal_handle = TerminalHandle::new(handle, channel.id());
        let backend = CrosstermBackend::new(terminal_handle);
        let options = TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        };
        let terminal = Terminal::with_options(backend, options)?;
        let app = App::new();
        self.clients.lock().await.insert(self.id, (terminal, app));
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
        let rect = Rect::new(0, 0, col_width as u16, row_height as u16);
        if let Some((terminal, _)) = self.clients.lock().await.get_mut(&self.id) {
            terminal.resize(rect)?;
        }
        session.channel_success(channel);
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Enable mouse tracking on the client
        session.data(channel, CryptoVec::from(ENABLE_MOUSE.to_vec()));

        if let Some((terminal, app)) = self.clients.lock().await.get_mut(&self.id) {
            render(terminal, app)?;
        }
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
        let rect = Rect::new(0, 0, col_width as u16, row_height as u16);
        if let Some((terminal, app)) = self.clients.lock().await.get_mut(&self.id) {
            terminal.resize(rect)?;
            render(terminal, app)?;
        }
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let mut should_close = false;

        {
            let mut clients = self.clients.lock().await;
            if let Some((terminal, app)) = clients.get_mut(&self.id) {
                let (mouse_events, key_bytes) = parse_mouse_events(data);

                // Handle mouse events
                for event in mouse_events {
                    if let MouseEvent::Press { button: 0, col, row } = event {
                        app.handle_click(col, row);
                    }
                }

                // Handle keyboard input
                for byte in &key_bytes {
                    match byte {
                        b'q' => {
                            should_close = true;
                            break;
                        }
                        b'1' => app.active_tab = 0,
                        b'2' => app.active_tab = 1,
                        b'\t' => {
                            app.active_tab = (app.active_tab + 1) % app.tab_titles.len();
                        }
                        _ => {}
                    }
                }

                if !should_close {
                    render(terminal, app)?;
                }
            }
        }

        if should_close {
            let mut clients = self.clients.lock().await;
            clients.remove(&self.id);
            session.data(channel, CryptoVec::from(DISABLE_MOUSE.to_vec()));
            session.close(channel);
        }

        Ok(())
    }
}

// --- Rendering ---

fn render(terminal: &mut SshTerminal, app: &App) -> Result<()> {
    terminal.draw(|frame| draw(frame, app))?;
    Ok(())
}

fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(0)])
        .split(frame.area());

    let titles: Vec<Line> = app.tab_titles.iter().map(|t| Line::from(*t)).collect();
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title("romoto"))
        .select(app.active_tab)
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, chunks[0]);

    let content = match app.active_tab {
        0 => "Hola Mundo desde Tab 1",
        1 => "Hola Mundo desde Tab 2",
        _ => "",
    };
    let paragraph = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title(app.tab_titles[app.active_tab]));
    frame.render_widget(paragraph, chunks[1]);
}

// --- Main ---

#[tokio::main]
async fn main() -> Result<()> {
    let key = russh::keys::key::KeyPair::generate_ed25519();
    let config = russh::server::Config {
        inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
        auth_rejection_time: std::time::Duration::from_secs(3),
        auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
        keys: vec![key],
        ..Default::default()
    };

    let mut server = AppServer {
        clients: Arc::new(Mutex::new(HashMap::new())),
        next_id: 0,
    };

    println!("romoto SSH server listening on 0.0.0.0:2222");
    println!("Connect with: ssh localhost -p 2222");

    server
        .run_on_address(Arc::new(config), ("0.0.0.0", 2222))
        .await?;

    Ok(())
}
