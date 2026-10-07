//! The dashboard.
//!
//! A thin client: it holds no server state of its own, it polls the daemon
//! for a snapshot and renders it. Quitting — `q`, `Esc` or Ctrl-C — only ends
//! this process; the daemon and every server carry on.

mod form;
mod theme;
mod views;
mod widgets;

use anyhow::Result;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};
use ratatui::{DefaultTerminal, Frame};

use crate::config::Config;
use crate::ipc::{Client, Request};
use crate::model::{ConsoleLine, ServerStatus, Snapshot, fmt_duration};
use form::Form;

/// How often to ask the daemon for a fresh snapshot.
const REFRESH: Duration = Duration::from_millis(500);
/// Input poll timeout; also bounds the animation tick.
const POLL: Duration = Duration::from_millis(100);
const TOAST_TTL: Duration = Duration::from_secs(4);
/// Console lines kept client-side for the selected server.
const CONSOLE_KEEP: usize = 5000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Dashboard,
    Servers,
    Players,
    Mods,
    Console,
    System,
    Logs,
}

impl Tab {
    const ALL: [Tab; 7] = [
        Tab::Dashboard,
        Tab::Servers,
        Tab::Players,
        Tab::Mods,
        Tab::Console,
        Tab::System,
        Tab::Logs,
    ];

    fn title(&self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Servers => "Servers",
            Tab::Players => "Players",
            Tab::Mods => "Mods",
            Tab::Console => "Console",
            Tab::System => "System",
            Tab::Logs => "Logs",
        }
    }

    fn index(&self) -> usize {
        Tab::ALL.iter().position(|t| t == self).unwrap_or(0)
    }
}

struct Confirm {
    prompt: String,
    request: Request,
}

pub struct App {
    client: Option<Client>,
    snapshot: Option<Snapshot>,
    connection_error: Option<String>,
    tab: Tab,
    pub server_selected: usize,
    pub player_selected: usize,
    pub mod_selected: usize,
    /// Lines scrolled up from the bottom, in Logs and Console.
    pub log_scroll: usize,
    pub console_scroll: usize,
    /// Console lines for `console_server`, newest last.
    pub console: Vec<ConsoleLine>,
    console_server: Option<String>,
    console_after: u64,
    /// This machine's LAN address, for the "how do people connect" hint.
    pub lan_ip: Option<String>,
    toast: Option<(String, bool, Instant)>,
    confirm: Option<Confirm>,
    /// Open form, if any. Swallows all input while present.
    form: Option<Form>,
    help: bool,
    pub frozen: bool,
    pub tick: usize,
    last_refresh: Instant,
    quit: bool,
    /// True when this process started the daemon just to show the dashboard.
    we_started_daemon: bool,
}

impl App {
    fn new() -> Self {
        Self {
            client: None,
            snapshot: None,
            connection_error: None,
            tab: Tab::Dashboard,
            server_selected: 0,
            player_selected: 0,
            mod_selected: 0,
            log_scroll: 0,
            console_scroll: 0,
            console: Vec::new(),
            console_server: None,
            console_after: 0,
            lan_ip: lan_ip(),
            toast: None,
            confirm: None,
            form: None,
            help: false,
            frozen: false,
            tick: 0,
            last_refresh: Instant::now() - REFRESH,
            quit: false,
            we_started_daemon: false,
        }
    }

    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    fn connect(&mut self) -> Result<&mut Client, String> {
        if self.client.is_none() {
            self.client = Some(Client::connect().map_err(|e| format!("{e:#}"))?);
        }
        Ok(self.client.as_mut().expect("just connected"))
    }

    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        let snapshot = match self.connect() {
            Ok(client) => client.snapshot().map_err(|e| format!("{e:#}")),
            Err(err) => Err(err),
        };
        match snapshot {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.connection_error = None;
                self.clamp_selection();
            }
            Err(err) => {
                self.connection_error = Some(err);
                self.client = None;
                return;
            }
        }
        if self.tab == Tab::Console {
            self.refresh_console();
        }
    }

    /// Fetch only the console lines that are new since the last call.
    fn refresh_console(&mut self) {
        let Some(name) = self.selected_server().map(|s| s.name.clone()) else {
            self.console.clear();
            self.console_server = None;
            return;
        };
        if self.console_server.as_deref() != Some(&name) {
            self.console.clear();
            self.console_after = 0;
            self.console_scroll = 0;
            self.console_server = Some(name.clone());
        }
        let after = self.console_after;
        let lines = match self.connect() {
            Ok(client) => client.console(&name, after, 1000),
            Err(_) => return,
        };
        match lines {
            Ok(lines) => {
                if let Some(last) = lines.last() {
                    self.console_after = last.seq;
                }
                // Hold the view still while scrolled back through history.
                if self.console_scroll > 0 {
                    self.console_scroll += lines.len();
                }
                self.console.extend(lines);
                if self.console.len() > CONSOLE_KEEP {
                    let excess = self.console.len() - CONSOLE_KEEP;
                    self.console.drain(..excess);
                }
            }
            Err(_) => self.client = None,
        }
    }

    fn clamp_selection(&mut self) {
        let (servers, players, mods) = match &self.snapshot {
            Some(s) => (
                s.servers.len(),
                s.servers.iter().map(|s| s.players.len()).sum::<usize>(),
                s.servers
                    .get(self.server_selected)
                    .map(|s| s.mods.len())
                    .unwrap_or(0),
            ),
            None => return,
        };
        let clamp = |index: &mut usize, len: usize| {
            *index = if len == 0 { 0 } else { (*index).min(len - 1) };
        };
        clamp(&mut self.server_selected, servers);
        clamp(&mut self.player_selected, players);
        clamp(&mut self.mod_selected, mods);
    }

    /// Run one request against the daemon and hand back what it said.
    fn request(&mut self, request: Request) -> Result<String, String> {
        let result = self
            .connect()
            .and_then(|client| client.command(&request).map_err(|e| format!("{e:#}")));
        if result.is_err() {
            // A failed request may have left an unread reply in the pipe,
            // which would be mistaken for the answer to the next one.
            self.client = None;
        }
        // Reflect the change immediately rather than waiting for the tick.
        self.last_refresh = Instant::now() - REFRESH;
        result
    }

    fn send(&mut self, request: Request) {
        match self.request(request) {
            Ok(message) => self.toast(message, false),
            Err(err) => self.toast(err, true),
        }
    }

    /// On quit, stop a daemon we started if it ended up hosting nothing.
    fn tidy_up(&mut self) {
        if !self.we_started_daemon {
            return;
        }
        let idle = self
            .snapshot
            .as_ref()
            .is_some_and(|s| s.servers.iter().all(|s| !s.state.is_live()));
        if idle && let Some(client) = self.client.as_mut() {
            let _ = client.command(&Request::Shutdown);
        }
    }

    fn toast(&mut self, message: String, is_error: bool) {
        self.toast = Some((message, is_error, Instant::now()));
    }

    pub fn selected_server(&self) -> Option<&ServerStatus> {
        self.snapshot
            .as_ref()
            .and_then(|s| s.servers.get(self.server_selected))
    }

    fn selected_name(&self) -> Option<String> {
        self.selected_server().map(|s| s.name.clone())
    }

    /// Every player on every server, in table order.
    pub fn all_players(&self) -> Vec<(String, crate::model::Player)> {
        self.snapshot
            .as_ref()
            .map(|s| {
                s.servers
                    .iter()
                    .flat_map(|server| {
                        server
                            .players
                            .iter()
                            .map(move |p| (server.name.clone(), p.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn list_len(&self) -> usize {
        let Some(snapshot) = &self.snapshot else {
            return 0;
        };
        match self.tab {
            Tab::Players => self.all_players().len(),
            Tab::Mods => self.selected_server().map(|s| s.mods.len()).unwrap_or(0),
            Tab::Logs => snapshot.logs.len(),
            Tab::Console => self.console.len(),
            _ => snapshot.servers.len(),
        }
    }

    fn cursor(&mut self) -> &mut usize {
        match self.tab {
            Tab::Players => &mut self.player_selected,
            Tab::Mods => &mut self.mod_selected,
            Tab::Logs => &mut self.log_scroll,
            Tab::Console => &mut self.console_scroll,
            _ => &mut self.server_selected,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.list_len();
        if matches!(self.tab, Tab::Logs | Tab::Console) {
            // Scrolling up means moving back through history.
            let cursor = self.cursor();
            let next = *cursor as isize - delta;
            *cursor = next.clamp(0, len.saturating_sub(1) as isize) as usize;
            return;
        }
        if len == 0 {
            return;
        }
        let cursor = self.cursor();
        let next = *cursor as isize + delta;
        *cursor = next.clamp(0, len as isize - 1) as usize;
    }

    fn cycle_server(&mut self, delta: isize) {
        let len = self.snapshot.as_ref().map(|s| s.servers.len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        self.server_selected =
            (self.server_selected as isize + delta).rem_euclid(len as isize) as usize;
        self.mod_selected = 0;
        if self.tab == Tab::Console {
            self.refresh_console();
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // An open form takes all input, so a message can contain any
        // character without triggering a shortcut.
        if self.form.is_some() {
            self.form_key(key);
            return;
        }
        if let Some(confirm) = &self.confirm {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let request = confirm.request.clone();
                    self.confirm = None;
                    self.send(request);
                }
                _ => {
                    self.confirm = None;
                    self.toast("cancelled".into(), false);
                }
            }
            return;
        }
        if self.help {
            self.help = false;
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // Leaving the dashboard never stops a server.
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.help = true,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.set_tab(Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()]);
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.set_tab(Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]);
            }
            KeyCode::Char(c @ '1'..='7') => self.set_tab(Tab::ALL[c as usize - '1' as usize]),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home => {
                let top = match self.tab {
                    Tab::Logs | Tab::Console => self.list_len().saturating_sub(1),
                    _ => 0,
                };
                *self.cursor() = top;
            }
            KeyCode::End => {
                let bottom = match self.tab {
                    Tab::Logs | Tab::Console => 0,
                    _ => self.list_len().saturating_sub(1),
                };
                *self.cursor() = bottom;
            }
            KeyCode::Char('[') => self.cycle_server(-1),
            KeyCode::Char(']') => self.cycle_server(1),
            KeyCode::Char(':') | KeyCode::Enter if self.tab == Tab::Console => {
                if let Some(name) = self.selected_name() {
                    self.form = Some(Form::command(&name));
                }
            }
            KeyCode::Char('s') => {
                if let Some(name) = self.selected_name() {
                    self.send(Request::Start { name });
                }
            }
            KeyCode::Char('x') => {
                if let Some(name) = self.selected_name() {
                    self.send(Request::Stop { name });
                }
            }
            KeyCode::Char('R') => {
                if let Some(name) = self.selected_name() {
                    self.send(Request::Restart { name });
                }
            }
            KeyCode::Char('S') => self.send(Request::StartAll),
            KeyCode::Char('X') => {
                self.confirm = Some(Confirm {
                    prompt: "Stop every server? Players will be disconnected.".into(),
                    request: Request::StopAll,
                });
            }
            KeyCode::Char('K') if self.tab == Tab::Players => {
                let players = self.all_players();
                if let Some((server, player)) = players.get(self.player_selected) {
                    self.form = Some(Form::kick(server, player.id, &player.name));
                }
            }
            KeyCode::Char('K') => {
                if let Some(name) = self.selected_name() {
                    self.form = Some(Form::auth_key(&name));
                }
            }
            KeyCode::Char('m') => {
                let server = if self.tab == Tab::Players {
                    self.all_players()
                        .get(self.player_selected)
                        .map(|(s, _)| s.clone())
                        .or_else(|| self.selected_name())
                } else {
                    self.selected_name()
                };
                if let Some(name) = server {
                    self.form = Some(Form::say(&name));
                }
            }
            KeyCode::Char('c') => {
                if self.selected_server().is_some() {
                    self.set_tab(Tab::Console);
                }
            }
            KeyCode::Char('i') => self.form = Some(Form::install()),
            KeyCode::Char('a') => self.open_add_form(),
            KeyCode::Char('e') | KeyCode::Char(' ') if self.tab == Tab::Mods => {
                if let Some((server, file)) = self.selected_mod() {
                    self.send(Request::ToggleMod { server, file });
                }
            }
            KeyCode::Char('e') => self.open_edit_form(),
            KeyCode::Char('d') | KeyCode::Delete => self.delete_selected(),
            KeyCode::Char('r') => self.send(Request::Reload),
            KeyCode::Char('f') => {
                self.frozen = !self.frozen;
                let message = if self.frozen {
                    "display frozen (servers keep running)"
                } else {
                    "display live"
                };
                self.toast(message.into(), false);
            }
            KeyCode::Char('Q') => {
                self.confirm = Some(Confirm {
                    prompt: "Shut the daemon down and stop every server?".into(),
                    request: Request::Shutdown,
                });
            }
            _ => {}
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        if tab == Tab::Console {
            self.refresh_console();
        }
    }

    fn selected_mod(&self) -> Option<(String, String)> {
        let server = self.selected_server()?;
        let file = server.mods.get(self.mod_selected)?;
        Some((server.name.clone(), file.name.clone()))
    }

    fn form_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.form = None;
                self.toast("cancelled".into(), false);
            }
            KeyCode::Char('u') if ctrl => form.clear_field(),
            KeyCode::Char('c') if ctrl => self.form = None,
            KeyCode::Tab | KeyCode::Down => form.next(),
            KeyCode::BackTab | KeyCode::Up => form.previous(),
            KeyCode::Left => form.move_left(),
            KeyCode::Right => form.move_right(),
            KeyCode::Home => form.move_to_start(),
            KeyCode::End => form.move_to_end(),
            KeyCode::Char('a') if ctrl => form.move_to_start(),
            KeyCode::Char('e') if ctrl => form.move_to_end(),
            KeyCode::Backspace => form.backspace(),
            KeyCode::Delete => form.delete(),
            KeyCode::Char(c) => form.insert(c),
            KeyCode::Enter => {
                let keep_open = form.kind == form::FormKind::Command;
                match form.build() {
                    Ok(request) => match self.request(request) {
                        Ok(message) => {
                            if keep_open {
                                // A console prompt stays up for the next
                                // command, like a real shell.
                                if let Some(form) = self.form.as_mut() {
                                    form.clear_field();
                                }
                                self.console_scroll = 0;
                            } else {
                                self.form = None;
                                self.toast(message, false);
                            }
                        }
                        // Keep the form up when the daemon says no, so
                        // nothing typed has to be typed again.
                        Err(message) => {
                            if let Some(form) = self.form.as_mut() {
                                form.error = Some(message);
                            }
                        }
                    },
                    Err(message) => form.error = Some(message),
                }
            }
            _ => {}
        }
    }

    /// `a` adds whatever the current view is about.
    fn open_add_form(&mut self) {
        if self.tab == Tab::Mods {
            if let Some(name) = self.selected_name() {
                self.form = Some(Form::add_mod(&name));
            } else {
                self.toast("add a server first (a on the Servers tab)".into(), true);
            }
            return;
        }
        let port = Config::load().map(|c| c.free_port()).unwrap_or(30814);
        self.form = Some(Form::add_server(port));
    }

    fn open_edit_form(&mut self) {
        let Some(name) = self.selected_name() else {
            return;
        };
        // The snapshot leaves out the auth key on purpose; edit from the
        // config so nothing the form does not show is lost.
        match Config::load() {
            Ok(config) => match config.server(&name) {
                Some(spec) => self.form = Some(Form::edit_server(spec)),
                None => self.toast(format!("`{name}` is not in the config file"), true),
            },
            Err(err) => self.toast(format!("{err:#}"), true),
        }
    }

    fn delete_selected(&mut self) {
        let (prompt, request) = if self.tab == Tab::Mods {
            let Some((server, file)) = self.selected_mod() else {
                return;
            };
            (
                format!("Delete mod `{file}` from `{server}`?"),
                Request::RemoveMod { server, file },
            )
        } else {
            let Some(name) = self.selected_name() else {
                return;
            };
            (
                format!("Stop and remove server `{name}`? Its files and mods are kept."),
                Request::RemoveServer { name, purge: false },
            )
        };
        self.confirm = Some(Confirm { prompt, request });
    }
}

/// The address other machines on the LAN would use. Connecting a UDP socket
/// sends nothing; it just asks the OS which interface it would route through.
fn lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

/// `we_started_daemon` is true when opening the dashboard is what brought the
/// daemon up; if nothing is hosting when the user quits, it is stopped again.
pub fn run(we_started_daemon: bool) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, we_started_daemon);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, we_started_daemon: bool) -> Result<()> {
    let mut app = App::new();
    app.we_started_daemon = we_started_daemon;
    app.refresh();
    let mut last_tick = Instant::now();
    let mut dirty = true;

    loop {
        // Redraw only when something could have changed: input, a refresh,
        // or the spinner tick. Idle, the dashboard costs next to nothing.
        if dirty {
            terminal.draw(|frame| draw(frame, &mut app))?;
            dirty = false;
        }

        if event::poll(POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key),
                _ => {}
            }
            dirty = true;
        }
        if last_tick.elapsed() >= Duration::from_millis(250) {
            app.tick = app.tick.wrapping_add(1);
            last_tick = Instant::now();
            dirty = true;
        }
        if !app.frozen && app.last_refresh.elapsed() >= REFRESH {
            app.refresh();
            dirty = true;
        }
        if let Some((_, _, at)) = &app.toast
            && at.elapsed() > TOAST_TTL
        {
            app.toast = None;
            dirty = true;
        }
        if app.quit {
            app.tidy_up();
            return Ok(());
        }
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);

    match app.snapshot.as_ref() {
        Some(_) => match app.tab {
            Tab::Dashboard => views::dashboard(frame, app, body),
            Tab::Servers => views::servers(frame, app, body),
            Tab::Players => views::players(frame, app, body),
            Tab::Mods => views::mods(frame, app, body),
            Tab::Console => views::console(frame, app, body),
            Tab::System => views::system(frame, app, body),
            Tab::Logs => views::logs(frame, app, body),
        },
        None => draw_disconnected(frame, app, body),
    }

    draw_footer(frame, app, footer);

    if app.help {
        widgets::help_overlay(frame, frame.area());
    }
    if let Some(confirm) = &app.confirm {
        widgets::confirm_overlay(frame, frame.area(), &confirm.prompt);
    }
    if let Some(form) = &app.form {
        form.render(frame, frame.area());
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [tabs_area, status_area] =
        Layout::horizontal([Constraint::Min(30), Constraint::Length(52)]).areas(inner);

    let titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| {
            Line::from(vec![
                Span::styled(format!("{} ", i + 1), theme::muted()),
                Span::raw(t.title()),
            ])
        })
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .style(theme::base())
        .highlight_style(theme::accent())
        .divider(Span::styled("│", theme::border()));
    frame.render_widget(tabs, tabs_area);

    let status = match (&app.snapshot, &app.connection_error) {
        (Some(snapshot), _) => {
            let totals = &snapshot.totals;
            let live = totals.servers_running > 0;
            let spinner = if live && !app.frozen {
                widgets::SPINNER[app.tick % widgets::SPINNER.len()]
            } else {
                "•"
            };
            Line::from(vec![
                Span::styled(
                    format!("{spinner} "),
                    Style::default().fg(if live { theme::GOOD } else { theme::MUTED }),
                ),
                Span::styled(format!("{} online", totals.players), theme::accent()),
                Span::styled("  servers ", theme::label()),
                Span::styled(
                    format!("{}/{}", totals.servers_running, totals.servers_total),
                    theme::value(),
                ),
                Span::styled("  cars ", theme::label()),
                Span::styled(totals.vehicles.to_string(), theme::value()),
                Span::styled("  up ", theme::label()),
                Span::styled(fmt_duration(snapshot.daemon.uptime_secs), theme::value()),
            ])
        }
        (None, Some(_)) => Line::from(Span::styled("daemon unreachable", theme::bad())),
        (None, None) => Line::from(Span::styled("connecting...", theme::muted())),
    };
    frame.render_widget(Paragraph::new(status).right_aligned(), status_area);
}

fn draw_disconnected(frame: &mut Frame, app: &App, area: Rect) {
    let message = app
        .connection_error
        .clone()
        .unwrap_or_else(|| "connecting to the daemon...".into());
    let text = vec![
        Line::from(""),
        Line::from(Span::styled("  no daemon", theme::title())),
        Line::from(""),
        Line::from(Span::styled(format!("  {message}"), theme::bad())),
        Line::from(""),
        Line::from(Span::styled(
            "  start one with `beamhost daemon start`; this screen reconnects on its own",
            theme::muted(),
        )),
    ];
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::border()),
        ),
        area,
    );
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if let Some((message, is_error, _)) = &app.toast {
        let style = if *is_error {
            theme::bad()
        } else {
            theme::good()
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if *is_error { " ✗ " } else { " ✓ " }, style),
                Span::styled(message.clone(), style),
            ])),
            area,
        );
        return;
    }

    let keys: &[(&str, &str)] = match app.tab {
        Tab::Players => &[
            ("↑↓", "select"),
            ("K", "kick"),
            ("m", "message"),
            ("?", "help"),
            ("q", "quit (keeps hosting)"),
        ],
        Tab::Mods => &[
            ("[ ]", "server"),
            ("a", "add mod"),
            ("e", "enable/disable"),
            ("d", "delete"),
            ("R", "restart to apply"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Console => &[
            ("[ ]", "server"),
            (":", "command"),
            ("↑↓", "scroll"),
            ("End", "latest"),
            ("m", "say"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Logs => &[
            ("↑↓", "scroll"),
            ("End", "latest"),
            ("f", "freeze"),
            ("?", "help"),
            ("q", "quit (keeps hosting)"),
        ],
        _ => &[
            ("↑↓", "select"),
            ("s/x/R", "start/stop/restart"),
            ("a", "add"),
            ("e", "edit"),
            ("K", "auth key"),
            ("c", "console"),
            ("i", "install"),
            ("?", "help"),
            ("q", "quit (keeps hosting)"),
        ],
    };
    let mut spans = vec![Span::raw(" ")];
    for (key, description) in keys {
        spans.push(Span::styled(*key, theme::accent()));
        spans.push(Span::styled(format!(" {description}   "), theme::muted()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests;
