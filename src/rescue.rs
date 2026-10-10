//! Getting out of a bad state without a reboot: a daemon that stopped
//! answering, or servers left running by a daemon that was killed.
//!
//! Every launched server records `servers/<name>/beamhost/server.pid`
//! ("<pid> <runtime>"). A pid is only ever signalled after checking that the
//! process behind it is still what we started (BeamMP-Server, or the docker
//! CLI running it), so a pid reused by the OS after a reboot is left alone.

use anyhow::{Result, bail};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config::Runtime;
use crate::paths;

fn pid_file(server: &str) -> PathBuf {
    paths::server_dir(server)
        .join("beamhost")
        .join("server.pid")
}

pub fn record(server: &str, pid: u32, runtime: Runtime) {
    let path = pid_file(server);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{pid} {}", runtime.label()));
}

pub fn clear(server: &str) {
    let _ = std::fs::remove_file(pid_file(server));
}

fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// The executable name behind a pid, if the process exists.
fn process_name(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system
        .process(pid)
        .map(|p| p.name().to_string_lossy().into_owned())
}

/// SIGTERM, wait up to `grace`, then SIGKILL. `target` may be a negative
/// process-group id. Returns true if something had to be killed outright.
fn terminate(target: i32, check: i32, grace: Duration) -> bool {
    unsafe {
        libc::kill(target, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !alive(check) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        libc::kill(target, libc::SIGKILL);
    }
    true
}

/// Stop servers whose daemon is gone. Safe to call any time no daemon is
/// running: the daemon calls it on startup, `daemon stop` after a forced stop.
pub fn reap_orphans() -> Vec<String> {
    let mut report = Vec::new();
    let Ok(entries) = std::fs::read_dir(paths::servers_dir()) else {
        return report;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(text) = std::fs::read_to_string(pid_file(&name)) else {
            continue;
        };
        clear(&name);
        let mut parts = text.split_whitespace();
        let pid: u32 = match parts.next().and_then(|p| p.parse().ok()) {
            Some(pid) => pid,
            None => continue,
        };
        let docker = parts.next() == Some("docker");
        if docker {
            // The container outlives its `docker run` client; remove it by
            // name, which is ours by construction.
            crate::runtime::remove_stale_container(&name);
        }
        let Some(process) = process_name(pid) else {
            if docker {
                report.push(format!("{name}: removed its container"));
            }
            continue;
        };
        let ours = if docker {
            process.starts_with("docker") || process.starts_with("com.docker")
        } else {
            process.starts_with("BeamMP-Server")
        };
        if !ours {
            continue;
        }
        let pid = pid as i32;
        // Native servers lead their own process group.
        let target = if docker { pid } else { -pid };
        let killed = terminate(target, pid, Duration::from_secs(5));
        report.push(format!(
            "{name}: stopped leftover server (pid {pid}{})",
            if killed { ", had to kill it" } else { "" }
        ));
    }
    report
}

/// Stop a daemon that no longer answers on its socket.
pub fn force_stop_daemon() -> Result<Vec<String>> {
    let mut report = Vec::new();
    let pid: Option<u32> = std::fs::read_to_string(paths::pid_file())
        .ok()
        .and_then(|t| t.trim().parse().ok());
    match pid {
        Some(pid) if process_name(pid).is_some_and(|n| n.starts_with("beamhost")) => {
            let killed = terminate(pid as i32, pid as i32, Duration::from_secs(3));
            report.push(format!(
                "daemon (pid {pid}) {}",
                if killed { "killed" } else { "stopped" }
            ));
        }
        Some(pid) if alive(pid as i32) => {
            bail!(
                "pid {pid} from {} is not a beamhost process; not touching it",
                paths::pid_file().display()
            );
        }
        _ => report.push("no daemon process was running".into()),
    }
    let _ = std::fs::remove_file(paths::socket());
    let _ = std::fs::remove_file(paths::pid_file());
    report.extend(reap_orphans());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_process_is_found_and_dead_pids_are_not() {
        let name = process_name(std::process::id()).expect("this test process exists");
        assert!(!name.is_empty());
        // Spawn and reap a child so its pid is certainly dead.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!alive(pid as i32));
    }

    #[test]
    fn terminate_stops_a_process_that_honours_sigterm() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        // Reap it as soon as it exits, as init would for a real orphan;
        // otherwise the zombie still answers kill(pid, 0).
        let reaper = std::thread::spawn(move || child.wait());
        let killed = terminate(pid, pid, Duration::from_secs(3));
        let _ = reaper.join();
        assert!(!killed, "sleep exits on SIGTERM without needing SIGKILL");
        assert!(!alive(pid));
    }
}
