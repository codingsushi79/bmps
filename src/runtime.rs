//! How a server process is actually launched.
//!
//! BeamMP publishes Linux builds only. On Linux the binary runs directly; on
//! macOS (or wherever `runtime = "docker"`) the Linux build runs in a tiny
//! Debian container that beamhost builds once. The binary and the server
//! directory are bind-mounted, so the container holds no state and mods, the
//! config and the bridge files are the same files either way.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

use crate::config::{Runtime, ServerSpec};

/// The binary needs only glibc and libstdc++ (checked against the published
/// builds); ca-certificates is for its HTTPS calls to the BeamMP backend.
const DOCKERFILE: &str = "FROM debian:12-slim\n\
RUN apt-get update \\\n \
 && apt-get install -y --no-install-recommends ca-certificates libstdc++6 \\\n \
 && rm -rf /var/lib/apt/lists/*\n\
WORKDIR /srv\n";

pub fn container_name(server: &str) -> String {
    format!("beamhost-{server}")
}

pub fn docker_available() -> bool {
    std::process::Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the runtime image if it is not there yet. Blocking; run it off the
/// async threads.
pub fn ensure_image(image: &str) -> Result<()> {
    let present = std::process::Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running docker — is Docker Desktop (or colima/OrbStack) installed?")?
        .success();
    if present {
        return Ok(());
    }
    use std::io::Write;
    let mut child = std::process::Command::new("docker")
        .args(["build", "-t", image, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting docker build")?;
    child
        .stdin
        .take()
        .context("docker build stdin")?
        .write_all(DOCKERFILE.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(3).collect();
        bail!(
            "building the runtime image failed: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
        );
    }
    Ok(())
}

/// Remove a leftover container with this server's name (from a daemon that
/// was killed), so `docker run --name` does not refuse to start.
pub fn remove_stale_container(server: &str) {
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", &container_name(server)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn build_command(
    runtime: Runtime,
    spec: &ServerSpec,
    binary: &Path,
    dir: &Path,
    image: &str,
) -> Command {
    let mut command = match runtime.resolve() {
        Runtime::Docker => {
            let mut c = Command::new("docker");
            let port = spec.port;
            c.args(["run", "--rm", "-i", "--init", "--name"])
                .arg(container_name(&spec.name))
                .arg("-v")
                .arg(format!("{}:/srv", dir.display()))
                .arg("-v")
                .arg(format!(
                    "{}:/usr/local/bin/BeamMP-Server:ro",
                    binary.display()
                ))
                .args(["-w", "/srv", "-p"])
                .arg(format!("{port}:{port}/tcp"))
                .arg("-p")
                .arg(format!("{port}:{port}/udp"))
                // Same ownership inside and out, so files the server writes
                // stay editable by the user.
                .args(["--user", &format!("{}:{}", uid(), gid())])
                .args(["--memory", "1g", "--pids-limit", "512"])
                .arg(image)
                .arg("/usr/local/bin/BeamMP-Server");
            c
        }
        _ => {
            let mut c = Command::new(binary);
            c.current_dir(dir);
            c
        }
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A new process group, so ^C in the shell that started the daemon never
    // reaches the servers.
    command.process_group(0);
    command
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn gid() -> u32 {
    unsafe { libc::getgid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_command_maps_both_protocols_and_mounts_the_binary() {
        let spec = ServerSpec {
            name: "west".into(),
            port: 30815,
            ..Default::default()
        };
        let command = build_command(
            Runtime::Docker,
            &spec,
            Path::new("/b/BeamMP-Server"),
            Path::new("/s/west"),
            "img",
        );
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(joined.contains("30815:30815/tcp"), "{joined}");
        assert!(joined.contains("30815:30815/udp"), "{joined}");
        assert!(joined.contains("/s/west:/srv"), "{joined}");
        assert!(joined.contains("--name beamhost-west"), "{joined}");
    }

    #[test]
    fn native_command_runs_in_the_server_directory() {
        let spec = ServerSpec::default();
        let command = build_command(
            Runtime::Native,
            &spec,
            Path::new("/b/BeamMP-Server"),
            Path::new("/s/x"),
            "img",
        );
        assert_eq!(command.as_std().get_program(), "/b/BeamMP-Server");
        assert_eq!(command.as_std().get_current_dir(), Some(Path::new("/s/x")));
    }
}
