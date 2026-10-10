//! What the daemon reports. Everything here is plain data so it crosses the
//! socket as JSON and the TUI can render it without knowing how it was made.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    Stopped,
    /// Fetching the server binary or building the container image.
    Preparing,
    Starting,
    Running,
    Stopping,
    /// Exited on its own; waiting out a backoff before restarting.
    Restarting,
    /// Exited on its own and will not be restarted.
    Crashed,
}

impl ServerState {
    pub fn label(self) -> &'static str {
        match self {
            ServerState::Stopped => "stopped",
            ServerState::Preparing => "preparing",
            ServerState::Starting => "starting",
            ServerState::Running => "running",
            ServerState::Stopping => "stopping",
            ServerState::Restarting => "restarting",
            ServerState::Crashed => "crashed",
        }
    }

    /// A process exists, or is about to.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            ServerState::Preparing
                | ServerState::Starting
                | ServerState::Running
                | ServerState::Stopping
                | ServerState::Restarting
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Player {
    pub id: i64,
    pub name: String,
    pub vehicles: u32,
    #[serde(default)]
    pub guest: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModFile {
    pub name: String,
    pub bytes: u64,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub name: String,
    pub state: ServerState,
    pub port: u16,
    pub map: String,
    pub version: String,
    pub runtime: String,
    pub private: bool,
    pub auth_key_set: bool,
    pub pid: Option<u32>,
    pub uptime_secs: u64,
    pub players: Vec<Player>,
    pub max_players: u32,
    pub max_cars: u32,
    /// Telemetry from the in-server plugin is fresh.
    pub telemetry: bool,
    pub cpu: f32,
    pub mem: u64,
    pub restarts: u32,
    pub last_exit: Option<String>,
    /// `Some(true)` once the public list shows this server.
    pub listed: Option<bool>,
    pub mods: Vec<ModFile>,
    pub plugins: Vec<String>,
    /// Players online over time, one sample per interval.
    pub history: Vec<u64>,
    pub autostart: bool,
    pub restart_on_crash: bool,
    pub description: String,
    pub tags: String,
    /// Newest console line number, so clients can ask for what is new.
    pub console_seq: u64,
    /// What a start is doing right now, while preparing.
    #[serde(default)]
    pub phase: Option<String>,
    /// The last start failed before the server ran (see `last_exit`).
    #[serde(default)]
    pub failed: bool,
}

impl ServerStatus {
    pub fn vehicles(&self) -> u32 {
        self.players.iter().map(|p| p.vehicles).sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Hardware {
    pub cpu_brand: String,
    pub cores: usize,
    pub cpu_usage: f32,
    pub per_core: Vec<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub load_avg: [f64; 3],
    pub os: String,
    pub host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Totals {
    pub servers_total: usize,
    pub servers_running: usize,
    pub players: usize,
    pub slots: u32,
    pub vehicles: u32,
    pub history: Vec<u64>,
    pub peak_players: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installed {
    pub tag: String,
    pub flavor: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Releases {
    pub installed: Vec<Installed>,
    pub latest: Option<String>,
    pub checked_secs_ago: Option<u64>,
    /// An install in progress: (tag, done bytes, total bytes).
    pub installing: Option<(String, u64, u64)>,
    pub error: Option<String>,
    pub host_flavor: String,
    pub docker: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub at: String,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleLine {
    pub seq: u64,
    pub at: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonInfo {
    pub pid: u32,
    pub uptime_secs: u64,
    pub version: String,
    pub config_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub daemon: DaemonInfo,
    pub servers: Vec<ServerStatus>,
    pub totals: Totals,
    pub hardware: Hardware,
    pub releases: Releases,
    pub logs: Vec<LogEntry>,
}

// ----------------------------------------------------------- formatting ---

pub fn fmt_duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d}d {h:02}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Truncate to `max` characters, marking the cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut out: String = text.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

/// BeamMP names and descriptions carry `^1`-style colour codes; strip them for
/// display and matching.
pub fn strip_beam_codes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '^' && chars.peek().is_some_and(|n| n.is_ascii_alphanumeric()) {
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_naturally() {
        assert_eq!(fmt_duration(5), "5s");
        assert_eq!(fmt_duration(65), "1m 05s");
        assert_eq!(fmt_duration(3 * 3600 + 120), "3h 02m");
        assert_eq!(fmt_duration(2 * 86400 + 3600), "2d 01h");
    }

    #[test]
    fn bytes_scale() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(12 * 1024 * 1024), "12.0 MiB");
    }

    #[test]
    fn truncation_marks_the_cut() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
    }

    #[test]
    fn colour_codes_are_stripped() {
        assert_eq!(strip_beam_codes("^1Red ^lBold^r server"), "Red Bold server");
        assert_eq!(strip_beam_codes("50^ off"), "50^ off");
    }
}
