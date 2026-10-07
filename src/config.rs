//! The one config file. The daemon owns it at runtime; the CLI and the TUI
//! edit it through the daemon when one is running, so there is never a second
//! writer racing the first.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::paths;

/// Maps that ship with BeamNG.drive. Anything else (a modded map) is accepted
/// as-is; this list only powers hints and short names.
pub const MAPS: &[&str] = &[
    "gridmap_v2",
    "west_coast_usa",
    "east_coast_usa",
    "utah",
    "italy",
    "jungle_rock_island",
    "industrial",
    "small_island",
    "hirochi_raceway",
    "automation_test_track",
    "johnson_valley",
    "derby",
    "driver_training",
    "cliff",
    "smallgrid",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    /// Native on Linux, Docker everywhere else.
    #[default]
    Auto,
    Native,
    Docker,
}

impl Runtime {
    /// What `Auto` means on this machine.
    pub fn resolve(self) -> Runtime {
        match self {
            Runtime::Auto if cfg!(target_os = "linux") => Runtime::Native,
            Runtime::Auto => Runtime::Docker,
            other => other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Runtime::Auto => "auto",
            Runtime::Native => "native",
            Runtime::Docker => "docker",
        }
    }

    pub fn parse(value: &str) -> Result<Runtime> {
        Ok(match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Runtime::Auto,
            "native" => Runtime::Native,
            "docker" => Runtime::Docker,
            other => bail!("unknown runtime `{other}` (auto, native or docker)"),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Server release used when a server does not pin one. `latest` means the
    /// newest stable release that is installed (or gets installed).
    pub default_version: String,
    pub runtime: Runtime,
    /// Container image for the docker runtime. Built locally on first use.
    pub docker_image: String,
    /// Poll the public BeamMP server list to confirm public servers show up.
    pub check_listing: bool,
    pub sample_interval_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_version: "latest".into(),
            runtime: Runtime::Auto,
            docker_image: "beamhost-runtime:debian12".into(),
            check_listing: true,
            sample_interval_ms: 1000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ServerSpec {
    /// Identifier: directory, container and CLI name.
    pub name: String,
    /// Shown in the server browser; may carry `^1`-style colour codes.
    /// Empty = `name`.
    pub title: String,
    pub port: u16,
    /// Short map name (`gridmap_v2`) or a full `/levels/x/info.json` path.
    pub map: String,
    pub max_players: u32,
    pub max_cars: u32,
    pub private: bool,
    /// From https://keymaster.beammp.com. Required by the server to start.
    pub auth_key: String,
    pub description: String,
    pub tags: String,
    pub allow_guests: bool,
    pub log_chat: bool,
    pub debug: bool,
    pub ip: String,
    /// Empty = `settings.default_version`.
    pub version: String,
    /// Start with the daemon.
    pub autostart: bool,
    /// Bring the server back after a crash, with backoff.
    pub restart_on_crash: bool,
    pub runtime: Option<Runtime>,
}

impl Default for ServerSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            title: String::new(),
            port: 30814,
            map: "gridmap_v2".into(),
            max_players: 8,
            max_cars: 1,
            private: true,
            auth_key: String::new(),
            description: "Hosted with beamhost".into(),
            tags: "Freeroam".into(),
            allow_guests: true,
            log_chat: true,
            debug: false,
            ip: "::".into(),
            version: String::new(),
            autostart: false,
            restart_on_crash: true,
            runtime: None,
        }
    }
}

impl ServerSpec {
    /// The name shown in the server browser.
    pub fn name_or_title(&self) -> String {
        if self.title.trim().is_empty() {
            self.name.clone()
        } else {
            self.title.clone()
        }
    }

    /// The `Map` value the server expects.
    pub fn map_path(&self) -> String {
        map_path(&self.map)
    }

    /// Short map name for display.
    pub fn map_short(&self) -> String {
        let map = self.map.trim();
        map.strip_prefix("/levels/")
            .and_then(|m| m.split('/').next())
            .unwrap_or(map)
            .to_string()
    }

    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        if self.port < 1024 {
            bail!("port {} is privileged; pick 1024 or higher", self.port);
        }
        if self.max_players == 0 || self.max_players > 256 {
            bail!("max players must be between 1 and 256");
        }
        if self.max_cars == 0 || self.max_cars > 50 {
            bail!("max cars must be between 1 and 50");
        }
        if self.map.trim().is_empty() {
            bail!("map is required");
        }
        let key = self.auth_key.trim();
        if !key.is_empty() && !is_plausible_key(key) {
            bail!("auth key looks wrong: expected a 36-char UUID from keymaster.beammp.com");
        }
        Ok(())
    }
}

pub fn map_path(map: &str) -> String {
    let map = map.trim();
    if map.starts_with('/') {
        map.to_string()
    } else {
        format!("/levels/{map}/info.json")
    }
}

/// Keymaster keys are UUIDs. Checking the shape catches the classic mistake of
/// pasting something else without pretending to validate the key itself.
pub fn is_plausible_key(key: &str) -> bool {
    key.len() == 36
        && key.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Names become directory names and container names, so keep them boring.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 32 {
        bail!("name must be 1-32 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("name may only use letters, digits, `-` and `_`");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, rename = "server")]
    pub servers: Vec<ServerSpec>,
}

impl Config {
    pub fn load() -> Result<Config> {
        Self::load_from(&paths::config_file())
    }

    pub fn load_from(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let config: Config =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                config.validate()?;
                Ok(config)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Atomic write, mode 0600: the file holds auth keys.
    pub fn save(&self) -> Result<()> {
        self.save_to(&paths::config_file())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        write_private(path, text.as_bytes())
    }

    pub fn validate(&self) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        let mut ports = std::collections::HashMap::new();
        for server in &self.servers {
            server
                .validate()
                .with_context(|| format!("server `{}`", server.name))?;
            if !names.insert(server.name.as_str()) {
                bail!("two servers are called `{}`", server.name);
            }
            if let Some(other) = ports.insert(server.port, server.name.as_str()) {
                bail!(
                    "servers `{other}` and `{}` both use port {}",
                    server.name,
                    server.port
                );
            }
        }
        Ok(())
    }

    pub fn server(&self, name: &str) -> Option<&ServerSpec> {
        self.servers.iter().find(|s| s.name == name)
    }

    pub fn server_mut(&mut self, name: &str) -> Option<&mut ServerSpec> {
        self.servers.iter_mut().find(|s| s.name == name)
    }

    /// Add or replace (by name), then validate the whole config so a port
    /// clash is caught before anything is written.
    pub fn upsert(&mut self, spec: ServerSpec, replace: bool) -> Result<()> {
        let mut next = self.clone();
        match next.servers.iter_mut().find(|s| s.name == spec.name) {
            Some(_) if !replace => bail!("a server called `{}` already exists", spec.name),
            Some(existing) => *existing = spec,
            None if replace => bail!("no server called `{}`", spec.name),
            None => next.servers.push(spec),
        }
        next.validate()?;
        *self = next;
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<ServerSpec> {
        let index = self
            .servers
            .iter()
            .position(|s| s.name == name)
            .with_context(|| format!("no server called `{name}`"))?;
        Ok(self.servers.remove(index))
    }

    /// The lowest free port at or above 30814, for new-server defaults.
    pub fn free_port(&self) -> u16 {
        let mut port = 30814;
        while self.servers.iter().any(|s| s.port == port) {
            port += 1;
        }
        port
    }

    pub fn version_for(&self, spec: &ServerSpec) -> String {
        if spec.version.trim().is_empty() {
            self.settings.default_version.clone()
        } else {
            spec.version.trim().to_string()
        }
    }

    pub fn runtime_for(&self, spec: &ServerSpec) -> Runtime {
        spec.runtime.unwrap_or(self.settings.runtime).resolve()
    }
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str, port: u16) -> ServerSpec {
        ServerSpec {
            name: name.into(),
            port,
            ..Default::default()
        }
    }

    #[test]
    fn short_map_names_expand_and_full_paths_pass_through() {
        assert_eq!(map_path("utah"), "/levels/utah/info.json");
        assert_eq!(map_path("/levels/x/info.json"), "/levels/x/info.json");
        let spec = ServerSpec {
            map: "/levels/italy/info.json".into(),
            ..Default::default()
        };
        assert_eq!(spec.map_short(), "italy");
    }

    #[test]
    fn port_clashes_and_duplicate_names_are_rejected() {
        let mut config = Config::default();
        config.upsert(server("a", 30814), false).unwrap();
        assert!(config.upsert(server("b", 30814), false).is_err());
        assert!(config.upsert(server("a", 30815), false).is_err());
        config.upsert(server("a", 30815), true).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.free_port(), 30814);
    }

    #[test]
    fn auth_keys_are_shape_checked() {
        assert!(is_plausible_key("0f8fad5b-d9cb-469f-a165-70867728950e"));
        assert!(!is_plausible_key("not-a-key"));
        let spec = ServerSpec {
            name: "x".into(),
            auth_key: "nope".into(),
            ..Default::default()
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn names_must_be_filesystem_safe() {
        assert!(validate_name("west-coast_1").is_ok());
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn config_round_trips_through_toml() {
        let mut config = Config::default();
        config.upsert(server("main", 30814), false).unwrap();
        let text = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.servers, config.servers);
    }
}
