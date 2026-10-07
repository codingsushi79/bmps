//! Everything on disk that belongs to one server: its `ServerConfig.toml`,
//! the bridge plugin, client mods and server plugins, and the files the
//! bridge uses to talk to the daemon.
//!
//! ```text
//! servers/<name>/
//!   ServerConfig.toml          regenerated from beamhost's config on start
//!   Resources/Client/*.zip     mods players download on join
//!   Resources/Client.disabled/ mods kept but not served
//!   Resources/Server/<plugin>/ Lua server plugins (beamhost's bridge included)
//!   beamhost/status.json       written by the bridge every second
//!   beamhost/cmd/*.cmd         commands for the bridge, one per file
//! ```

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::{ServerSpec, write_private};
use crate::model::{ModFile, Player};
use crate::paths;

pub const BRIDGE: &str = include_str!("../assets/beamhost.lua");
pub const BRIDGE_NAME: &str = "beamhost";

pub struct Instance {
    pub dir: PathBuf,
}

impl Instance {
    pub fn new(name: &str) -> Self {
        Self {
            dir: paths::server_dir(name),
        }
    }

    pub fn client_dir(&self) -> PathBuf {
        self.dir.join("Resources").join("Client")
    }

    pub fn disabled_dir(&self) -> PathBuf {
        self.dir.join("Resources").join("Client.disabled")
    }

    pub fn server_plugins_dir(&self) -> PathBuf {
        self.dir.join("Resources").join("Server")
    }

    pub fn status_file(&self) -> PathBuf {
        self.dir.join("beamhost").join("status.json")
    }

    pub fn cmd_dir(&self) -> PathBuf {
        self.dir.join("beamhost").join("cmd")
    }

    /// Lay out the directory, write the server config and the bridge. Called
    /// on every start so the files always match beamhost's config.
    pub fn prepare(&self, spec: &ServerSpec) -> Result<()> {
        for dir in [
            self.client_dir(),
            self.disabled_dir(),
            self.server_plugins_dir().join(BRIDGE_NAME),
            self.cmd_dir(),
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        write_private(
            &self.dir.join("ServerConfig.toml"),
            render_server_config(spec).as_bytes(),
        )?;
        std::fs::write(
            self.server_plugins_dir().join(BRIDGE_NAME).join("main.lua"),
            BRIDGE,
        )?;
        // A stale status from the last run would show players who are gone.
        let _ = std::fs::remove_file(self.status_file());
        Ok(())
    }

    /// Hand a command to the bridge. Written under a temp name and renamed
    /// so the plugin never picks up a half-written file; names sort in
    /// submission order.
    pub fn queue_command(&self, line: &str) -> Result<()> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        if line.contains(['\n', '\r']) {
            bail!("commands are a single line");
        }
        let dir = self.cmd_dir();
        std::fs::create_dir_all(&dir)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("{stamp:020}-{seq:06}");
        let tmp = dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, line)?;
        std::fs::rename(&tmp, dir.join(format!("{name}.cmd")))?;
        Ok(())
    }

    pub fn read_status(&self) -> Option<BridgeStatus> {
        let text = std::fs::read(self.status_file()).ok()?;
        serde_json::from_slice(&text).ok()
    }

    pub fn mods(&self) -> Vec<ModFile> {
        let mut out = Vec::new();
        for (dir, enabled) in [(self.client_dir(), true), (self.disabled_dir(), false)] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.to_ascii_lowercase().ends_with(".zip") {
                    continue;
                }
                let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(ModFile {
                    name,
                    bytes,
                    enabled,
                });
            }
        }
        out.sort_by_key(|a| a.name.to_lowercase());
        out
    }

    pub fn plugins(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(self.server_plugins_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// Copy a mod zip (or every zip in a folder) into Resources/Client.
    pub fn add_mod(&self, source: &Path) -> Result<Vec<String>> {
        let source = expand_home(source);
        let files: Vec<PathBuf> = if source.is_dir() {
            std::fs::read_dir(&source)?
                .flatten()
                .map(|e| e.path())
                .filter(|p| is_zip(p))
                .collect()
        } else if is_zip(&source) {
            vec![source.clone()]
        } else if source.exists() {
            bail!(
                "{} is not a .zip — BeamNG mods are zip files",
                source.display()
            );
        } else {
            bail!("{} does not exist", source.display());
        };
        if files.is_empty() {
            bail!("no .zip files in {}", source.display());
        }
        std::fs::create_dir_all(self.client_dir())?;
        let mut added = Vec::new();
        for file in files {
            check_zip(&file)?;
            let name = file
                .file_name()
                .context("mod path has no file name")?
                .to_string_lossy()
                .into_owned();
            let dest = self.client_dir().join(&name);
            let tmp = self.client_dir().join(format!(".{name}.part"));
            std::fs::copy(&file, &tmp).with_context(|| format!("copying {}", file.display()))?;
            std::fs::rename(&tmp, &dest)?;
            let _ = std::fs::remove_file(self.disabled_dir().join(&name));
            added.push(name);
        }
        Ok(added)
    }

    pub fn remove_mod(&self, file: &str) -> Result<()> {
        let name = safe_file_name(file)?;
        for dir in [self.client_dir(), self.disabled_dir()] {
            let path = dir.join(name);
            if path.exists() {
                std::fs::remove_file(&path)?;
                return Ok(());
            }
        }
        bail!("no mod called `{file}`")
    }

    /// Move a mod between served and kept-but-not-served. Returns whether it
    /// is now enabled.
    pub fn toggle_mod(&self, file: &str) -> Result<bool> {
        let name = safe_file_name(file)?;
        let (on, off) = (self.client_dir().join(name), self.disabled_dir().join(name));
        if on.exists() {
            std::fs::create_dir_all(self.disabled_dir())?;
            std::fs::rename(on, off)?;
            Ok(false)
        } else if off.exists() {
            std::fs::create_dir_all(self.client_dir())?;
            std::fs::rename(off, on)?;
            Ok(true)
        } else {
            bail!("no mod called `{file}`")
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BridgeStatus {
    pub time: i64,
    #[serde(default)]
    pub players: Vec<Player>,
}

fn is_zip(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// Catch a renamed .rar or a truncated download before players do.
fn check_zip(path: &Path) -> Result<()> {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(path)?
        .read_exact(&mut magic)
        .with_context(|| format!("{} is empty", path.display()))?;
    if &magic != b"PK\x03\x04" && &magic != b"PK\x05\x06" {
        bail!("{} is not a valid zip archive", path.display());
    }
    Ok(())
}

/// Mod names come from the client; never let one walk out of the directory.
fn safe_file_name(file: &str) -> Result<&str> {
    if file.is_empty() || file.contains(['/', '\\']) || file.starts_with('.') {
        bail!("invalid mod name `{file}`");
    }
    Ok(file)
}

fn expand_home(path: &Path) -> PathBuf {
    match path.to_str().and_then(|p| p.strip_prefix("~/")) {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => path.to_path_buf(),
    }
}

fn toml_string(value: &str) -> String {
    // A TOML basic string is a JSON string minus a few escapes we never emit.
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

pub fn render_server_config(spec: &ServerSpec) -> String {
    format!(
        "# Generated by beamhost from ~/.config/beamhost/config.toml.\n\
         # Edit the server there (or in the TUI); this file is rewritten on start.\n\
         \n\
         [General]\n\
         Name = {name}\n\
         Port = {port}\n\
         AuthKey = {key}\n\
         AllowGuests = {guests}\n\
         LogChat = {log_chat}\n\
         Debug = {debug}\n\
         IP = {ip}\n\
         Private = {private}\n\
         InformationPacket = true\n\
         Map = {map}\n\
         MaxCars = {max_cars}\n\
         MaxPlayers = {max_players}\n\
         Description = {description}\n\
         Tags = {tags}\n\
         ResourceFolder = \"Resources\"\n\
         \n\
         [Misc]\n\
         ImScaredOfUpdates = true\n\
         UpdateReminderTime = \"0s\"\n\
         SendErrorsShowMessage = false\n\
         SendErrors = true\n",
        name = toml_string(&spec.name_or_title()),
        port = spec.port,
        key = toml_string(spec.auth_key.trim()),
        guests = spec.allow_guests,
        log_chat = spec.log_chat,
        debug = spec.debug,
        ip = toml_string(&spec.ip),
        private = spec.private,
        map = toml_string(&spec.map_path()),
        max_cars = spec.max_cars,
        max_players = spec.max_players,
        description = toml_string(&spec.description),
        tags = toml_string(&spec.tags),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_instance() -> Instance {
        let dir = std::env::temp_dir().join(format!(
            "beamhost-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Instance { dir }
    }

    #[test]
    fn server_config_is_valid_toml_with_every_field() {
        let spec = ServerSpec {
            name: "main".into(),
            title: "My \"quoted\" server".into(),
            auth_key: "0f8fad5b-d9cb-469f-a165-70867728950e".into(),
            map: "utah".into(),
            ..Default::default()
        };
        let text = render_server_config(&spec);
        let parsed: toml::Table = toml::from_str(&text).unwrap();
        let general = parsed["General"].as_table().unwrap();
        assert_eq!(general["Name"].as_str(), Some("My \"quoted\" server"));
        assert_eq!(general["Map"].as_str(), Some("/levels/utah/info.json"));
        assert_eq!(general["Port"].as_integer(), Some(30814));
        assert_eq!(general["Private"].as_bool(), Some(true));
    }

    #[test]
    fn prepare_lays_out_resources_and_the_bridge() {
        let instance = temp_instance();
        instance
            .prepare(&ServerSpec {
                name: "t".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(instance.client_dir().is_dir());
        assert!(instance.cmd_dir().is_dir());
        assert_eq!(instance.plugins(), vec!["beamhost".to_string()]);
        let _ = std::fs::remove_dir_all(&instance.dir);
    }

    #[test]
    fn mods_are_added_toggled_and_removed() {
        let instance = temp_instance();
        std::fs::create_dir_all(&instance.dir).unwrap();
        let source = instance.dir.join("cool_car.zip");
        std::fs::write(&source, b"PK\x03\x04rest-of-zip").unwrap();
        let bogus = instance.dir.join("fake.zip");
        std::fs::write(&bogus, b"Rar!").unwrap();

        assert_eq!(instance.add_mod(&source).unwrap(), vec!["cool_car.zip"]);
        assert!(instance.add_mod(&bogus).is_err());
        assert!(instance.mods()[0].enabled);
        assert!(!instance.toggle_mod("cool_car.zip").unwrap());
        assert!(!instance.mods()[0].enabled);
        assert!(instance.toggle_mod("cool_car.zip").unwrap());
        assert!(instance.remove_mod("../../etc/passwd").is_err());
        instance.remove_mod("cool_car.zip").unwrap();
        assert!(instance.mods().is_empty());
        let _ = std::fs::remove_dir_all(&instance.dir);
    }

    #[test]
    fn commands_are_queued_in_order_one_per_file() {
        let instance = temp_instance();
        instance.queue_command("say hi").unwrap();
        instance.queue_command("kick 2 bye").unwrap();
        assert!(instance.queue_command("say a\nkick 1").is_err());
        let mut names: Vec<_> = std::fs::read_dir(instance.cmd_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2);
        assert!(names.iter().all(|n| n.ends_with(".cmd")));
        let first = std::fs::read_to_string(instance.cmd_dir().join(&names[0])).unwrap();
        assert_eq!(first, "say hi");
        let _ = std::fs::remove_dir_all(&instance.dir);
    }

    #[test]
    fn bridge_status_parses() {
        let status: BridgeStatus = serde_json::from_str(
            r#"{"time":1,"started":0,"players":[{"id":0,"name":"a","vehicles":2,"guest":false}]}"#,
        )
        .unwrap();
        assert_eq!(status.players[0].vehicles, 2);
    }
}
