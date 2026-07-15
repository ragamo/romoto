use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};
use ratatui::Frame;

struct App {
    active_tab: usize,
    tab_titles: Vec<&'static str>,
}

impl App {
    fn new() -> Self {
        Self {
            active_tab: 0,
            tab_titles: vec!["General", "Auth"],
        }
    }
}

pub fn run() -> Result<()> {
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

    let terminal = ratatui::init();
    let result = run_loop(terminal);

    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    ratatui::restore();

    result
}

fn run_loop(mut terminal: ratatui::DefaultTerminal) -> Result<()> {
    let mut app = App::new();

    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        match event::read()? {
            Event::Key(key) => {
                if key.code == event::KeyCode::Char('q') || key.code == event::KeyCode::Esc {
                    break;
                }
                match key.code {
                    event::KeyCode::Tab => {
                        app.active_tab = (app.active_tab + 1) % app.tab_titles.len();
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse) => {
                if mouse.kind == MouseEventKind::Down(event::MouseButton::Left) {
                    handle_click(&mut app, mouse.column, mouse.row);
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn handle_click(app: &mut App, col: u16, row: u16) {
    if row != 1 {
        return;
    }
    let mut x: u16 = 1;
    for (i, title) in app.tab_titles.iter().enumerate() {
        let tab_width = title.len() as u16 + 2;
        if col >= x && col < x + tab_width {
            app.active_tab = i;
            return;
        }
        x += tab_width + 1;
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(0)])
        .split(frame.area());

    let titles: Vec<Line> = app.tab_titles.iter().map(|t| Line::from(*t)).collect();
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title("romoto config"))
        .select(app.active_tab)
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, chunks[0]);

    let content = match app.active_tab {
        0 => "General settings (TODO)",
        1 => "Authentication settings (TODO)",
        _ => "",
    };
    let paragraph = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title(app.tab_titles[app.active_tab]));
    frame.render_widget(paragraph, chunks[1]);
}
