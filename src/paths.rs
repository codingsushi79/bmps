//! Where things live on disk. `BEAMHOST_HOME` relocates everything at once,
//! which is also what the tests use to stay out of the real home directory.

use std::path::PathBuf;

fn home_override() -> Option<PathBuf> {
    std::env::var_os("BEAMHOST_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn config_dir() -> PathBuf {
    home_override().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("beamhost")
    })
}

pub fn data_dir() -> PathBuf {
    home_override().map(|h| h.join("data")).unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("share")
            .join("beamhost")
    })
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// Unix socket paths are capped at ~104 bytes on macOS (108 on Linux). A
/// long BEAMHOST_HOME would overflow that, so fall back to a short path in
/// /tmp, unique per user and data directory.
pub fn socket() -> PathBuf {
    let preferred = data_dir().join("beamhost.sock");
    if preferred.as_os_str().len() < 100 {
        return preferred;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data_dir().hash(&mut hasher);
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!(
        "/tmp/beamhost-{uid}-{:08x}.sock",
        hasher.finish() as u32
    ))
}

pub fn pid_file() -> PathBuf {
    data_dir().join("daemon.pid")
}

pub fn daemon_log() -> PathBuf {
    data_dir().join("daemon.log")
}

/// Downloaded server binaries: `bin/<tag>/<flavor>/BeamMP-Server`.
pub fn bin_dir() -> PathBuf {
    data_dir().join("bin")
}

/// One working directory per server: config, Resources, logs.
pub fn servers_dir() -> PathBuf {
    data_dir().join("servers")
}

pub fn server_dir(name: &str) -> PathBuf {
    servers_dir().join(name)
}

pub fn ensure_dirs() -> std::io::Result<()> {
    for dir in [config_dir(), data_dir(), bin_dir(), servers_dir()] {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn socket_paths_always_fit_in_sun_path() {
        // SAFETY: only this test reads BEAMHOST_HOME in this process' tests
        // that care; it is restored right after.
        let long = format!("/tmp/{}", "x".repeat(150));
        unsafe { std::env::set_var("BEAMHOST_HOME", &long) };
        let socket = super::socket();
        unsafe { std::env::remove_var("BEAMHOST_HOME") };
        assert!(socket.as_os_str().len() < 100, "{}", socket.display());
        assert!(socket.starts_with("/tmp"));
    }
}
