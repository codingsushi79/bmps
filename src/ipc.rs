//! TUI/CLI ↔ daemon protocol: one JSON object per line over a unix socket.
//! The client side is blocking std I/O on purpose — the TUI has nothing else
//! to do while it waits, and a request is answered in microseconds.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::config::ServerSpec;
use crate::model::{ConsoleLine, Snapshot};
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Snapshot,
    Console {
        server: String,
        after: u64,
        limit: usize,
    },
    Start {
        name: String,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
    },
    StartAll,
    StopAll,
    AddServer {
        spec: ServerSpec,
    },
    UpdateServer {
        spec: ServerSpec,
    },
    RemoveServer {
        name: String,
        purge: bool,
    },
    SetAuthKey {
        name: String,
        key: String,
    },
    /// `None` = newest stable release.
    Install {
        version: Option<String>,
    },
    CheckReleases,
    /// A raw line for the server console (stdin).
    Command {
        server: String,
        line: String,
    },
    Say {
        server: String,
        message: String,
    },
    Kick {
        server: String,
        player: i64,
        reason: String,
    },
    AddMod {
        server: String,
        path: String,
    },
    RemoveMod {
        server: String,
        file: String,
    },
    ToggleMod {
        server: String,
        file: String,
    },
    Reload,
    Shutdown,
}

impl Request {
    /// Requests that may legitimately take a while (downloads, docker builds).
    pub fn timeout(&self) -> Duration {
        match self {
            Request::Install { .. } | Request::CheckReleases => Duration::from_secs(30),
            Request::Stop { .. } | Request::StopAll | Request::Restart { .. } => {
                Duration::from_secs(20)
            }
            Request::AddMod { .. } => Duration::from_secs(60),
            _ => Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Box<Snapshot>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console: Option<Vec<ConsoleLine>>,
}

impl Response {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            snapshot: None,
            console: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
            snapshot: None,
            console: None,
        }
    }
}

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    line: String,
}

impl Client {
    pub fn connect() -> Result<Client> {
        let path = paths::socket();
        let stream = UnixStream::connect(&path)
            .with_context(|| format!("daemon not reachable at {}", path.display()))?;
        let writer = stream.try_clone()?;
        Ok(Client {
            reader: BufReader::new(stream),
            writer,
            line: String::with_capacity(64 * 1024),
        })
    }

    pub fn call(&mut self, request: &Request) -> Result<Response> {
        self.writer
            .set_write_timeout(Some(Duration::from_secs(5)))?;
        self.reader
            .get_ref()
            .set_read_timeout(Some(request.timeout()))?;
        let mut payload = serde_json::to_vec(request)?;
        payload.push(b'\n');
        self.writer.write_all(&payload)?;
        self.line.clear();
        let timeout = request.timeout();
        let read = self.reader.read_line(&mut self.line).map_err(|err| {
            if matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) {
                anyhow::anyhow!(
                    "the daemon didn't answer within {}s, so it looks stuck. \
                     `beamhost daemon stop` force-stops it (and any servers it left running).",
                    timeout.as_secs()
                )
            } else {
                anyhow::Error::new(err).context("waiting for the daemon")
            }
        })?;
        if read == 0 {
            bail!("daemon closed the connection");
        }
        serde_json::from_str(&self.line).context("garbled reply from the daemon")
    }

    /// A request whose only interesting output is a message.
    pub fn command(&mut self, request: &Request) -> Result<String> {
        let response = self.call(request)?;
        if response.ok {
            Ok(response.message)
        } else {
            bail!("{}", response.message)
        }
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        let response = self.call(&Request::Snapshot)?;
        match response.snapshot {
            Some(snapshot) if response.ok => Ok(*snapshot),
            _ => bail!("{}", response.message),
        }
    }

    pub fn console(&mut self, server: &str, after: u64, limit: usize) -> Result<Vec<ConsoleLine>> {
        let response = self.call(&Request::Console {
            server: server.into(),
            after,
            limit,
        })?;
        if !response.ok {
            bail!("{}", response.message);
        }
        Ok(response.console.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_tagged_json() {
        let json = serde_json::to_string(&Request::Kick {
            server: "main".into(),
            player: 3,
            reason: "afk".into(),
        })
        .unwrap();
        assert!(json.contains(r#""op":"kick""#), "{json}");
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Request::Kick { player: 3, .. }));
    }
}
