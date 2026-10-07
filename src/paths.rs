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

pub fn socket() -> PathBuf {
    data_dir().join("beamhost.sock")
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
