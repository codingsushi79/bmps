//! Finding a Docker engine that actually answers.
//!
//! `docker` on a Mac can point at any of OrbStack, Docker Desktop, colima or
//! Rancher Desktop, and the *current context* is often a leftover from a
//! different one (classic: Docker Desktop's `desktop-linux` after switching
//! to OrbStack). So instead of trusting the default, try in order:
//!
//! 1. the docker CLI as configured (current context / DOCKER_HOST),
//! 2. every configured context, known engines first,
//! 3. the well-known socket files directly,
//!
//! and remember the first engine that answers as a `DOCKER_HOST`, which every
//! later docker call then uses. When nothing answers, the error says what was
//! tried and what each attempt said.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Engine {
    pub bin: PathBuf,
    /// `unix://...` (or tcp) to pin every call to; `None` = the CLI default.
    pub host: Option<String>,
    /// Where it was found, for logs and the dashboard.
    pub via: String,
}

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

/// The docker CLI: PATH first, then where the common engines install it,
/// since a daemon started from a non-login shell may lack their PATH entry.
fn find_cli() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("docker");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = home();
    [
        home.join(".orbstack/bin/docker"),
        PathBuf::from("/Applications/OrbStack.app/Contents/MacOS/xbin/docker"),
        PathBuf::from("/usr/local/bin/docker"),
        PathBuf::from("/opt/homebrew/bin/docker"),
        home.join(".docker/bin/docker"),
        PathBuf::from("/Applications/Docker.app/Contents/Resources/bin/docker"),
        home.join(".rd/bin/docker"),
        PathBuf::from("/usr/bin/docker"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// Run a short docker command with a hard timeout (a wedged engine can make
/// `docker info` hang). Returns stdout on success, a one-line reason on
/// failure.
fn probe(bin: &Path, host: Option<&str>, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(bin);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(host) = host {
        command
            .env("DOCKER_HOST", host)
            .env_remove("DOCKER_CONTEXT");
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("failed")
            .to_string())
    }
}

fn answers(bin: &Path, host: Option<&str>) -> Result<(), String> {
    probe(bin, host, &["info", "--format", "{{.ServerVersion}}"])
        .map(|_| ())
        .map_err(|e| summarize(&e))
}

/// Docker's connection errors run to three lines of Go; keep the part that
/// says where it looked.
fn summarize(error: &str) -> String {
    let socket = error
        .split_whitespace()
        .find(|w| w.starts_with("unix://") || w.starts_with("tcp://") || w.starts_with("npipe://"))
        .map(|w| w.trim_end_matches([';', '.', ',', ':']).to_string());
    let lower = error.to_ascii_lowercase();
    match socket {
        Some(socket) if lower.contains("no such file") || lower.contains("connect") => {
            format!("nothing listening at {socket}")
        }
        _ => error.chars().take(120).collect(),
    }
}

/// Context names, with the engines people actually run first.
fn contexts(bin: &Path) -> Vec<String> {
    let mut names: Vec<String> = probe(bin, None, &["context", "ls", "--format", "{{.Name}}"])
        .map(|out| {
            out.lines()
                .map(|l| l.trim().trim_end_matches(" *").to_string())
                .collect()
        })
        .unwrap_or_default();
    const PREFERRED: [&str; 5] = [
        "orbstack",
        "colima",
        "desktop-linux",
        "rancher-desktop",
        "default",
    ];
    names.sort_by_key(|n| {
        PREFERRED
            .iter()
            .position(|p| p == n)
            .unwrap_or(PREFERRED.len())
    });
    names.dedup();
    names
}

fn context_host(bin: &Path, name: &str) -> Option<String> {
    probe(
        bin,
        None,
        &[
            "context",
            "inspect",
            name,
            "--format",
            "{{.Endpoints.docker.Host}}",
        ],
    )
    .ok()
    .filter(|h| !h.is_empty())
}

fn known_sockets() -> Vec<PathBuf> {
    let home = home();
    vec![
        home.join(".orbstack/run/docker.sock"),
        home.join(".colima/default/docker.sock"),
        home.join(".colima/docker.sock"),
        home.join(".docker/run/docker.sock"),
        home.join(".rd/docker.sock"),
        PathBuf::from("/var/run/docker.sock"),
    ]
}

/// Find an engine that answers. `fresh` skips the cache (the engine may have
/// been started or stopped since).
pub fn detect(fresh: bool) -> Result<Engine, String> {
    if !fresh && let Some(engine) = ENGINE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Ok(engine);
    }
    let found = search();
    let mut cache = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    *cache = found.as_ref().ok().cloned();
    found
}

fn search() -> Result<Engine, String> {
    let Some(bin) = find_cli() else {
        return Err("the `docker` command isn't installed. Install OrbStack (or Docker Desktop, or colima), \
                    which provides it, then try again"
            .into());
    };
    let mut tried = Vec::new();
    match answers(&bin, None) {
        Ok(()) => {
            return Ok(Engine {
                bin,
                host: None,
                via: "default context".into(),
            });
        }
        Err(e) => tried.push(format!("default context: {e}")),
    }
    for name in contexts(&bin) {
        let Some(host) = context_host(&bin, &name) else {
            continue;
        };
        match answers(&bin, Some(&host)) {
            Ok(()) => {
                return Ok(Engine {
                    bin,
                    host: Some(host),
                    via: format!("context `{name}`"),
                });
            }
            Err(e) => tried.push(format!("context `{name}`: {e}")),
        }
    }
    for socket in known_sockets().into_iter().filter(|s| s.exists()) {
        let host = format!("unix://{}", socket.display());
        match answers(&bin, Some(&host)) {
            Ok(()) => {
                return Ok(Engine {
                    bin,
                    host: Some(host),
                    via: socket.display().to_string(),
                });
            }
            Err(e) => tried.push(format!("{}: {e}", socket.display())),
        }
    }
    Err(format!(
        "no Docker engine answered. Start OrbStack, Docker Desktop or colima. Tried {}",
        tried.join("; ")
    ))
}

pub fn available() -> bool {
    detect(true).is_ok()
}

fn engine_or_default() -> Engine {
    detect(false).unwrap_or(Engine {
        bin: PathBuf::from("docker"),
        host: None,
        via: "PATH".into(),
    })
}

/// A docker command aimed at the engine that answered.
pub fn command() -> Command {
    let engine = engine_or_default();
    let mut command = Command::new(&engine.bin);
    if let Some(host) = &engine.host {
        command
            .env("DOCKER_HOST", host)
            .env_remove("DOCKER_CONTEXT");
    }
    command
}

pub fn async_command() -> tokio::process::Command {
    let engine = engine_or_default();
    let mut command = tokio::process::Command::new(&engine.bin);
    if let Some(host) = &engine.host {
        command
            .env("DOCKER_HOST", host)
            .env_remove("DOCKER_CONTEXT");
    }
    command
}

/// After switching from Docker Desktop, `~/.docker/config.json` often still
/// names Desktop's credential helper (`credsStore: desktop`), which no longer
/// exists, and every image pull fails. A clean, empty config sidesteps it;
/// anonymous pulls of public images need no credentials.
pub fn clean_config_dir() -> std::io::Result<PathBuf> {
    let dir = crate::paths::data_dir().join("docker-config");
    std::fs::create_dir_all(&dir)?;
    let config = dir.join("config.json");
    if !config.exists() {
        std::fs::write(&config, "{}")?;
    }
    Ok(dir)
}

pub fn is_credential_error(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("docker-credential") || s.contains("error getting credentials")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_helper_failures_are_recognised() {
        assert!(is_credential_error(
            "error getting credentials - err: exec: \"docker-credential-desktop\": executable file not found in $PATH"
        ));
        assert!(!is_credential_error("no space left on device"));
    }

    #[test]
    fn a_missing_engine_is_reported_with_what_was_tried() {
        // `true` exits 0 for anything, so stand up a fake CLI that always
        // fails the way docker does when its engine is down.
        let dir = std::env::temp_dir().join(format!("beamhost-fake-docker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("docker");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'failed to connect to the docker API at unix:///nope.sock; check if the path is correct and if the daemon is running: dial unix /nope.sock: connect: no such file or directory' >&2\nexit 1\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = answers(&fake, None).unwrap_err();
        assert_eq!(err, "nothing listening at unix:///nope.sock");
        let _ = std::fs::remove_dir_all(dir);
    }
}
