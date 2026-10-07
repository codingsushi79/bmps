//! Modal forms, so servers, mods, auth keys and chat messages can be handled
//! from inside the dashboard rather than from a shell.
//!
//! The form only collects text; validation lives in the daemon, the single
//! place that knows whether a config is coherent. Whatever it rejects comes
//! straight back into the form so it can be fixed in place.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::theme;
use super::widgets::centered;
use crate::config::{Runtime, ServerSpec};
use crate::ipc::Request;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FormKind {
    AddServer,
    /// Edit an existing server; holds the spec being edited so fields the
    /// form does not show (and the auth key) survive the round trip.
    EditServer,
    AuthKey,
    AddMod,
    Say,
    Kick {
        player: i64,
    },
    Command,
    Install,
}

pub struct Field {
    pub label: &'static str,
    pub value: String,
    /// Insertion point, as a byte offset into `value`. Kept on a char
    /// boundary by every method that moves it, so slicing is always safe.
    cursor: usize,
    pub hint: &'static str,
    pub required: bool,
    /// Rendered as dots. Nothing here is a private key, but shoulder-surfing a
    /// pool password is still rude.
    pub secret: bool,
}

impl Field {
    fn new(label: &'static str, hint: &'static str, required: bool) -> Self {
        Self {
            label,
            value: String::new(),
            cursor: 0,
            hint,
            required,
            secret: false,
        }
    }

    fn secret(label: &'static str, hint: &'static str, required: bool) -> Self {
        Self {
            label,
            value: String::new(),
            cursor: 0,
            hint,
            required,
            secret: true,
        }
    }

    /// The value as it should be shown: masked for secrets.
    fn display(&self) -> String {
        if self.secret {
            "•".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        }
    }

    /// The displayed value either side of the insertion point.
    fn split_at_cursor(&self) -> (String, String) {
        let before = self.value[..self.cursor].chars().count();
        let shown = self.display();
        let split = shown
            .char_indices()
            .nth(before)
            .map(|(i, _)| i)
            .unwrap_or(shown.len());
        (shown[..split].to_string(), shown[split..].to_string())
    }

    /// Byte offset of the character before the cursor, if there is one.
    fn previous_boundary(&self) -> Option<usize> {
        self.value[..self.cursor]
            .chars()
            .next_back()
            .map(|c| self.cursor - c.len_utf8())
    }

    /// Byte offset one character to the right, clamped to the end.
    fn next_boundary(&self) -> usize {
        match self.value[self.cursor..].chars().next() {
            Some(c) => self.cursor + c.len_utf8(),
            None => self.cursor,
        }
    }

    fn with(label: &'static str, hint: &'static str, default: &str) -> Self {
        Self {
            label,
            value: default.to_string(),
            cursor: default.len(),
            hint,
            required: false,
            secret: false,
        }
    }
}
pub struct Form {
    pub kind: FormKind,
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: Option<String>,
    /// The server the form acts on.
    pub subject: Option<String>,
    /// The spec being edited, for `EditServer`.
    base: Option<ServerSpec>,
}

impl Form {
    fn new(kind: FormKind, title: impl Into<String>, fields: Vec<Field>) -> Self {
        Self {
            kind,
            title: title.into(),
            fields,
            focus: 0,
            error: None,
            subject: None,
            base: None,
        }
    }

    fn on(mut self, server: &str) -> Self {
        self.subject = Some(server.to_string());
        self
    }

    pub fn add_server(port: u16) -> Self {
        Self::new(
            FormKind::AddServer,
            "New BeamMP server",
            server_fields(
                &ServerSpec {
                    port,
                    ..Default::default()
                },
                true,
            ),
        )
    }

    pub fn edit_server(spec: &ServerSpec) -> Self {
        let mut form = Self::new(
            FormKind::EditServer,
            format!("Edit `{}`", spec.name),
            server_fields(spec, false),
        )
        .on(&spec.name);
        form.base = Some(spec.clone());
        form.move_to_end();
        form
    }

    pub fn auth_key(server: &str) -> Self {
        Self::new(
            FormKind::AuthKey,
            format!("Auth key for `{server}`"),
            vec![Field::secret(
                "auth key",
                "keymaster.beammp.com → Keys → New (blank clears it)",
                false,
            )],
        )
        .on(server)
    }

    pub fn add_mod(server: &str) -> Self {
        Self::new(
            FormKind::AddMod,
            format!("Add a mod to `{server}`"),
            vec![Field::new(
                "path",
                "a .zip mod, or a folder of them (~ works)",
                true,
            )],
        )
        .on(server)
    }

    pub fn say(server: &str) -> Self {
        Self::new(
            FormKind::Say,
            format!("Message everyone on `{server}`"),
            vec![Field::new("message", "shown in chat to every player", true)],
        )
        .on(server)
    }

    pub fn kick(server: &str, player: i64, name: &str) -> Self {
        Self::new(
            FormKind::Kick { player },
            format!("Kick {name} from `{server}`"),
            vec![Field::with(
                "reason",
                "shown to the player",
                "Kicked by the host",
            )],
        )
        .on(server)
    }

    pub fn command(server: &str) -> Self {
        Self::new(
            FormKind::Command,
            format!("Console · `{server}`"),
            vec![Field::new(
                "command",
                "status, list, say <msg>, kick <name>, help",
                true,
            )],
        )
        .on(server)
    }

    pub fn install() -> Self {
        Self::new(
            FormKind::Install,
            "Install BeamMP-Server",
            vec![Field::with(
                "version",
                "a release tag like v3.9.3, or latest",
                "latest",
            )],
        )
    }

    fn value(&self, index: usize) -> String {
        self.fields
            .get(index)
            .map(|f| f.value.trim().to_string())
            .unwrap_or_default()
    }

    fn optional(&self, index: usize) -> Option<String> {
        let value = self.value(index);
        if value.is_empty() { None } else { Some(value) }
    }

    // ------------------------------------------------------------- input ---

    pub fn insert(&mut self, c: char) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.value.insert(field.cursor, c);
            field.cursor += c.len_utf8();
            self.error = None;
        }
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            if let Some(previous) = field.previous_boundary() {
                field.value.remove(previous);
                field.cursor = previous;
            }
            self.error = None;
        }
    }

    /// Delete the character under the cursor.
    pub fn delete(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            if field.cursor < field.value.len() {
                field.value.remove(field.cursor);
            }
            self.error = None;
        }
    }

    pub fn clear_field(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.value.clear();
            field.cursor = 0;
            self.error = None;
        }
    }

    pub fn move_left(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus)
            && let Some(previous) = field.previous_boundary()
        {
            field.cursor = previous;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.cursor = field.next_boundary();
        }
    }

    pub fn move_to_start(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.cursor = 0;
        }
    }

    pub fn move_to_end(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.cursor = field.value.len();
        }
    }

    pub fn next(&mut self) {
        self.focus = (self.focus + 1) % self.fields.len();
        self.move_to_end();
    }

    pub fn previous(&mut self) {
        self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
        self.move_to_end();
    }
    /// Turn the filled-in form into a request, or explain what is missing.
    pub fn build(&self) -> Result<Request, String> {
        for field in &self.fields {
            if field.required && field.value.trim().is_empty() {
                return Err(format!("{} is required", field.label));
            }
        }
        let server = || self.subject.clone().unwrap_or_default();
        Ok(match &self.kind {
            FormKind::AddServer | FormKind::EditServer => {
                let mut spec = self.base.clone().unwrap_or_default();
                let mut i = 0;
                if self.kind == FormKind::AddServer {
                    spec.name = self.value(0);
                    i = 1;
                }
                spec.title = self.value(i);
                spec.port = parse(&self.value(i + 1), "port")?;
                spec.map = self.value(i + 2);
                spec.max_players = parse(&self.value(i + 3), "max players")?;
                spec.max_cars = parse(&self.value(i + 4), "max cars")?;
                spec.private = !yes(&self.value(i + 5), "public")?;
                spec.description = self.value(i + 6);
                spec.tags = self.value(i + 7);
                spec.version = self.value(i + 8);
                spec.runtime =
                    match Runtime::parse(&self.value(i + 9)).map_err(|e| e.to_string())? {
                        Runtime::Auto => None,
                        other => Some(other),
                    };
                spec.autostart = yes(&self.value(i + 10), "autostart")?;
                spec.restart_on_crash = yes(&self.value(i + 11), "restart on crash")?;
                if self.kind == FormKind::AddServer {
                    spec.auth_key = self.value(i + 12);
                }
                spec.validate().map_err(|e| e.to_string())?;
                if self.kind == FormKind::AddServer {
                    Request::AddServer { spec }
                } else {
                    Request::UpdateServer { spec }
                }
            }
            FormKind::AuthKey => Request::SetAuthKey {
                name: server(),
                key: self.value(0),
            },
            FormKind::AddMod => Request::AddMod {
                server: server(),
                path: self.value(0),
            },
            FormKind::Say => Request::Say {
                server: server(),
                message: self.value(0),
            },
            FormKind::Kick { player } => Request::Kick {
                server: server(),
                player: *player,
                reason: self.value(0),
            },
            FormKind::Command => Request::Command {
                server: server(),
                line: self.value(0),
            },
            FormKind::Install => Request::Install {
                version: self.optional(0),
            },
        })
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let height = self.fields.len() as u16 + 7;
        let target = centered(area, 74, height);
        frame.render_widget(Clear, target);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::ACCENT))
            .title(Span::styled(format!(" {} ", self.title), theme::title()));
        let inner = block.inner(target);
        frame.render_widget(block, target);

        let [rows, footer] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(inner);

        let lines: Vec<Line> = self
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let focused = i == self.focus;
                let marker = if focused { "▸" } else { " " };
                let label = format!("{marker} {:<12}", field.label);
                let mut spans = vec![Span::styled(
                    label,
                    if focused {
                        theme::accent()
                    } else {
                        theme::label()
                    },
                )];
                let entry = Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD);
                if field.value.is_empty() {
                    if focused {
                        // The block cursor comes first on an empty field, so
                        // the hint is not pushed off its own line.
                        spans.push(Span::styled("█", theme::accent()));
                    }
                    spans.push(Span::styled(field.hint, theme::muted()));
                } else if focused {
                    // Split the value at the insertion point and draw the
                    // block between the halves, so the cursor is where the
                    // next character will actually land.
                    let (before, after) = field.split_at_cursor();
                    spans.push(Span::styled(before, entry));
                    spans.push(Span::styled("█", theme::accent()));
                    spans.push(Span::styled(after, entry));
                } else {
                    spans.push(Span::styled(field.display(), entry));
                }
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rows);

        let mut footer_lines = Vec::new();
        if let Some(error) = &self.error {
            footer_lines.push(Line::from(Span::styled(format!("✗ {error}"), theme::bad())));
        } else {
            footer_lines.push(Line::from(Span::styled(
                self.fields
                    .get(self.focus)
                    .map(|f| {
                        if f.required {
                            format!("{} — required", f.hint)
                        } else {
                            format!("{} — optional", f.hint)
                        }
                    })
                    .unwrap_or_default(),
                theme::muted(),
            )));
        }
        footer_lines.push(Line::from(vec![
            Span::styled("Tab/↑↓", theme::accent()),
            Span::styled(" move   ", theme::muted()),
            Span::styled("Enter", theme::accent()),
            Span::styled(" save   ", theme::muted()),
            Span::styled("←→", theme::accent()),
            Span::styled(" edit   ", theme::muted()),
            Span::styled("Ctrl-U", theme::accent()),
            Span::styled(" clear   ", theme::muted()),
            Span::styled("Esc", theme::accent()),
            Span::styled(" cancel", theme::muted()),
        ]));
        frame.render_widget(
            Paragraph::new(footer_lines).wrap(Wrap { trim: false }),
            footer,
        );
    }
}
fn server_fields(spec: &ServerSpec, new: bool) -> Vec<Field> {
    let flag = |b: bool| if b { "yes" } else { "no" };
    let mut fields = Vec::new();
    if new {
        fields.push(Field::new(
            "name",
            "id for the CLI and folders, e.g. freeroam",
            true,
        ));
    }
    fields.extend([
        Field::with(
            "title",
            "shown in the server browser; ^1 colour codes ok",
            &spec.title,
        ),
        Field::with(
            "port",
            "TCP+UDP; forward it on your router to go public",
            &spec.port.to_string(),
        ),
        Field::with(
            "map",
            "gridmap_v2, west_coast_usa, utah, italy, ...",
            &spec.map_short(),
        ),
        Field::with("max players", "1-256", &spec.max_players.to_string()),
        Field::with("max cars", "per player, 1-50", &spec.max_cars.to_string()),
        Field::with(
            "public",
            "yes = listed in the BeamMP server browser",
            flag(!spec.private),
        ),
        Field::with("description", "browser description", &spec.description),
        Field::with("tags", "comma separated, e.g. Freeroam,Drift", &spec.tags),
        Field::with(
            "version",
            "blank = default (latest), or a tag like v3.9.3",
            &spec.version,
        ),
        Field::with(
            "runtime",
            "auto, native (Linux) or docker (macOS)",
            spec.runtime.map(|r| r.label()).unwrap_or("auto"),
        ),
        Field::with(
            "autostart",
            "start whenever the daemon starts",
            flag(spec.autostart),
        ),
        Field::with(
            "restart",
            "bring it back after a crash (yes/no)",
            flag(spec.restart_on_crash),
        ),
    ]);
    if new {
        fields.push(Field::secret(
            "auth key",
            "required to start: keymaster.beammp.com → Keys",
            false,
        ));
    }
    fields
}

fn parse<T: std::str::FromStr>(value: &str, label: &str) -> Result<T, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("{label}: `{value}` is not a number"))
}

fn yes(value: &str, label: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "on" | "1" => Ok(true),
        "n" | "no" | "false" | "off" | "0" | "" => Ok(false),
        other => Err(format!("{label}: answer yes or no, not `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(form: &mut Form, label: &str, value: &str) {
        let field = form
            .fields
            .iter_mut()
            .find(|f| f.label == label)
            .unwrap_or_else(|| panic!("no field {label}"));
        field.value = value.to_string();
        field.cursor = value.len();
    }

    #[test]
    fn a_new_server_form_builds_an_add_request() {
        let mut form = Form::add_server(30820);
        fill(&mut form, "name", "drift");
        fill(&mut form, "public", "yes");
        fill(&mut form, "map", "utah");
        match form.build().unwrap() {
            Request::AddServer { spec } => {
                assert_eq!(spec.name, "drift");
                assert_eq!(spec.port, 30820);
                assert!(!spec.private);
                assert_eq!(spec.map_path(), "/levels/utah/info.json");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn editing_keeps_the_auth_key_and_name() {
        let spec = ServerSpec {
            name: "main".into(),
            auth_key: "0f8fad5b-d9cb-469f-a165-70867728950e".into(),
            ..Default::default()
        };
        let mut form = Form::edit_server(&spec);
        fill(&mut form, "max players", "16");
        match form.build().unwrap() {
            Request::UpdateServer { spec: next } => {
                assert_eq!(next.name, "main");
                assert_eq!(next.auth_key, spec.auth_key);
                assert_eq!(next.max_players, 16);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn bad_input_is_reported_not_guessed() {
        let mut form = Form::add_server(30814);
        fill(&mut form, "name", "x");
        fill(&mut form, "port", "lots");
        assert!(form.build().unwrap_err().contains("port"));
        fill(&mut form, "port", "30814");
        fill(&mut form, "public", "maybe");
        assert!(form.build().unwrap_err().contains("yes or no"));
        let empty = Form::add_server(30814);
        assert!(empty.build().unwrap_err().contains("name"));
    }

    #[test]
    fn the_auth_key_is_masked() {
        let form = Form::auth_key("main");
        assert!(form.fields[0].secret);
    }
}
