//! Command line. With no arguments it opens the dashboard; everything else is
//! a thin wrapper over one daemon request, so the CLI and the TUI can never
//! disagree about what a command does.

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::config::{Config, MAPS, Runtime, ServerSpec};
use crate::ipc::{Client, Request};
use crate::model::{ServerState, fmt_bytes, fmt_duration};
use crate::{daemon, paths};

#[derive(Parser)]
#[command(
    name = "beamhost",
    version,
    about = "Host BeamMP servers from a live dashboard. Servers keep running after you close it."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// One-shot summary of every server
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Start a server, or every server
    Start { name: Option<String> },
    /// Stop a server, or every server
    Stop { name: Option<String> },
    /// Stop and start a server (applies config and mod changes)
    Restart { name: String },
    /// Manage servers
    #[command(subcommand)]
    Server(ServerCommand),
    /// Manage client mods (Resources/Client)
    #[command(subcommand, name = "mod")]
    Mod(ModCommand),
    /// Broadcast a chat message to everyone on a server
    Say {
        server: String,
        message: Vec<String>,
    },
    /// Kick a player by id (see `beamhost players`)
    Kick {
        server: String,
        id: i64,
        reason: Vec<String>,
    },
    /// List players on every server
    Players,
    /// Print a server's console; -f keeps following it
    Console {
        server: String,
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Send a raw line to a server's console (e.g. `status`, `list`)
    Cmd { server: String, line: Vec<String> },
    /// Download a BeamMP-Server release (default: newest stable)
    Install { version: Option<String> },
    /// Check GitHub for the newest BeamMP-Server release
    Releases,
    /// Maps that ship with BeamNG.drive
    Maps,
    /// Check this machine: Docker engine, server builds, daemon
    Doctor,
    /// Control the background daemon
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// Where the config lives, and what is in it
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Args, Clone, Default)]
struct ServerOptions {
    /// UDP+TCP port (default: next free from 30814)
    #[arg(long)]
    port: Option<u16>,
    /// Map short name (gridmap_v2, utah, ...) or /levels/x/info.json
    #[arg(long)]
    map: Option<String>,
    #[arg(long)]
    max_players: Option<u32>,
    #[arg(long)]
    max_cars: Option<u32>,
    /// Show in the public server list
    #[arg(long)]
    public: bool,
    /// Hide from the public server list
    #[arg(long, conflicts_with = "public")]
    private: bool,
    /// Auth key from https://keymaster.beammp.com
    #[arg(long)]
    key: Option<String>,
    /// Name shown in the server browser
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    description: Option<String>,
    #[arg(long)]
    tags: Option<String>,
    /// BeamMP-Server version tag, or `latest`
    #[arg(long)]
    version: Option<String>,
    /// auto, native or docker
    #[arg(long)]
    runtime: Option<String>,
    /// Start with the daemon
    #[arg(long)]
    autostart: Option<bool>,
    #[arg(long)]
    restart_on_crash: Option<bool>,
}

impl ServerOptions {
    fn apply(&self, spec: &mut ServerSpec) -> Result<()> {
        if let Some(v) = self.port {
            spec.port = v;
        }
        if let Some(v) = &self.map {
            spec.map = v.clone();
        }
        if let Some(v) = self.max_players {
            spec.max_players = v;
        }
        if let Some(v) = self.max_cars {
            spec.max_cars = v;
        }
        if self.public {
            spec.private = false;
        }
        if self.private {
            spec.private = true;
        }
        if let Some(v) = &self.key {
            spec.auth_key = v.trim().to_string();
        }
        if let Some(v) = &self.title {
            spec.title = v.clone();
        }
        if let Some(v) = &self.description {
            spec.description = v.clone();
        }
        if let Some(v) = &self.tags {
            spec.tags = v.clone();
        }
        if let Some(v) = &self.version {
            spec.version = v.clone();
        }
        if let Some(v) = &self.runtime {
            spec.runtime = match Runtime::parse(v)? {
                Runtime::Auto => None,
                other => Some(other),
            };
        }
        if let Some(v) = self.autostart {
            spec.autostart = v;
        }
        if let Some(v) = self.restart_on_crash {
            spec.restart_on_crash = v;
        }
        Ok(())
    }
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Add a server
    Add {
        name: String,
        #[command(flatten)]
        options: ServerOptions,
    },
    /// Change a server's settings
    Edit {
        name: String,
        #[command(flatten)]
        options: ServerOptions,
    },
    /// Set (or clear, with "") a server's auth key
    Key { name: String, key: String },
    /// List servers
    List,
    /// Remove a server; --purge also deletes its files and mods
    Rm {
        name: String,
        #[arg(long)]
        purge: bool,
    },
}

#[derive(Subcommand)]
enum ModCommand {
    /// Copy a mod .zip (or a folder of them) onto a server
    Add {
        server: String,
        path: String,
    },
    List {
        server: String,
    },
    Rm {
        server: String,
        file: String,
    },
    /// Keep a mod but stop serving it (or serve it again)
    Toggle {
        server: String,
        file: String,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    Start,
    /// Stop the daemon and every server. Forces it if it doesn't answer.
    Stop {
        /// Skip the polite request and kill it straight away
        #[arg(long)]
        force: bool,
    },
    Status,
    /// Run in the foreground (what `start` launches)
    Run,
    /// Print the daemon's log file
    Log,
    /// Re-read the config file after a hand edit
    Reload,
}

#[derive(Subcommand)]
enum ConfigCommand {
    Path,
    Show,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        let started = daemon::ensure_running()?;
        return crate::tui::run(started);
    };
    match command {
        Command::Daemon(DaemonCommand::Run) => daemon::run_foreground(),
        Command::Daemon(DaemonCommand::Start) => {
            if daemon::ensure_running()? {
                println!("daemon started");
            } else {
                println!("daemon already running");
            }
            Ok(())
        }
        Command::Daemon(DaemonCommand::Stop { force }) => stop_daemon(force),
        Command::Daemon(DaemonCommand::Status) => {
            if daemon::is_running() {
                let snapshot = client()?.snapshot()?;
                println!(
                    "running  pid {}  up {}  {} server(s)",
                    snapshot.daemon.pid,
                    fmt_duration(snapshot.daemon.uptime_secs),
                    snapshot.servers.len()
                );
            } else {
                println!("not running");
            }
            Ok(())
        }
        Command::Daemon(DaemonCommand::Log) => {
            let text = std::fs::read_to_string(paths::daemon_log()).unwrap_or_default();
            print!("{text}");
            Ok(())
        }
        Command::Daemon(DaemonCommand::Reload) => say(Request::Reload),
        Command::Config(ConfigCommand::Path) => {
            println!("{}", paths::config_file().display());
            Ok(())
        }
        Command::Config(ConfigCommand::Show) => {
            let config = Config::load()?;
            let mut shown = config.clone();
            // Never print auth keys to a terminal someone might screenshot.
            for server in &mut shown.servers {
                if !server.auth_key.is_empty() {
                    server.auth_key = "<set>".into();
                }
            }
            print!("{}", toml::to_string_pretty(&shown)?);
            Ok(())
        }
        Command::Doctor => doctor(),
        Command::Maps => {
            for map in MAPS {
                println!("{map}");
            }
            Ok(())
        }
        Command::Status { json } => status(json),
        Command::Start { name } => say(match name {
            Some(name) => Request::Start { name },
            None => Request::StartAll,
        }),
        Command::Stop { name } => say(match name {
            Some(name) => Request::Stop { name },
            None => Request::StopAll,
        }),
        Command::Restart { name } => say(Request::Restart { name }),
        Command::Server(ServerCommand::Add { name, options }) => {
            let config = Config::load()?;
            let mut spec = ServerSpec {
                name,
                port: config.free_port(),
                ..Default::default()
            };
            options.apply(&mut spec)?;
            spec.validate()?;
            say(Request::AddServer { spec })
        }
        Command::Server(ServerCommand::Edit { name, options }) => {
            let config = Config::load()?;
            let mut spec = config
                .server(&name)
                .cloned()
                .with_context(|| format!("no server called `{name}`"))?;
            options.apply(&mut spec)?;
            spec.validate()?;
            say(Request::UpdateServer { spec })
        }
        Command::Server(ServerCommand::Key { name, key }) => say(Request::SetAuthKey { name, key }),
        Command::Server(ServerCommand::Rm { name, purge }) => {
            say(Request::RemoveServer { name, purge })
        }
        Command::Server(ServerCommand::List) => {
            let config = Config::load()?;
            if config.servers.is_empty() {
                println!("no servers — add one with `beamhost server add NAME --key <auth key>`");
            }
            for s in &config.servers {
                println!(
                    "{:<16} port {:<6} {:<22} {:>3} players  {}  {}",
                    s.name,
                    s.port,
                    s.map_short(),
                    s.max_players,
                    if s.private { "private" } else { "public " },
                    if s.auth_key.is_empty() {
                        "no key"
                    } else {
                        "key set"
                    }
                );
            }
            Ok(())
        }
        Command::Mod(ModCommand::Add { server, path }) => {
            // Resolve relative to where the user is, not where the daemon is.
            let path = std::fs::canonicalize(&path)
                .with_context(|| format!("{path} does not exist"))?
                .display()
                .to_string();
            say(Request::AddMod { server, path })
        }
        Command::Mod(ModCommand::Rm { server, file }) => say(Request::RemoveMod { server, file }),
        Command::Mod(ModCommand::Toggle { server, file }) => {
            say(Request::ToggleMod { server, file })
        }
        Command::Mod(ModCommand::List { server }) => {
            let snapshot = client()?.snapshot()?;
            let status = snapshot
                .servers
                .iter()
                .find(|s| s.name == server)
                .with_context(|| format!("no server called `{server}`"))?;
            if status.mods.is_empty() {
                println!("no mods on `{server}`");
            }
            for m in &status.mods {
                println!(
                    "{}  {:>10}  {}",
                    if m.enabled { "on " } else { "off" },
                    fmt_bytes(m.bytes),
                    m.name
                );
            }
            Ok(())
        }
        Command::Say { server, message } => say(Request::Say {
            server,
            message: message.join(" "),
        }),
        Command::Kick { server, id, reason } => say(Request::Kick {
            server,
            player: id,
            reason: reason.join(" "),
        }),
        Command::Cmd { server, line } => say(Request::Command {
            server,
            line: line.join(" "),
        }),
        Command::Players => {
            let snapshot = client()?.snapshot()?;
            let mut any = false;
            for server in &snapshot.servers {
                for p in &server.players {
                    any = true;
                    println!(
                        "{:<16} {:>4}  {:<24} {} vehicle(s){}",
                        server.name,
                        p.id,
                        p.name,
                        p.vehicles,
                        if p.guest { "  guest" } else { "" }
                    );
                }
            }
            if !any {
                println!("nobody online");
            }
            Ok(())
        }
        Command::Console {
            server,
            follow,
            lines,
        } => {
            let mut client = client()?;
            let mut after = 0;
            let mut limit = lines;
            loop {
                for line in client.console(&server, after, limit)? {
                    println!("{} {}", line.at, line.text);
                    after = line.seq;
                }
                if !follow {
                    return Ok(());
                }
                limit = 5000;
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
        Command::Install { version } => say(Request::Install { version }),
        Command::Releases => say(Request::CheckReleases),
    }
}

fn doctor() -> Result<()> {
    let check = |ok: bool| if ok { "ok  " } else { "FAIL" };
    println!(
        "{}  platform        {} {}",
        check(true),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let native = crate::release::host_flavor();
    println!(
        "{}  native build    {}",
        check(true),
        native.clone().unwrap_or_else(|| format!(
            "none for this OS; servers run in Docker ({})",
            crate::release::docker_flavor()
        ))
    );
    let needs_docker = native.is_none()
        || Config::load()
            .map(|c| {
                c.servers
                    .iter()
                    .any(|s| c.runtime_for(s) == Runtime::Docker)
            })
            .unwrap_or(false);
    match crate::docker::detect(true) {
        Ok(engine) => println!(
            "{}  docker          {} via {}{}",
            check(true),
            engine.bin.display(),
            engine.via,
            engine.host.map(|h| format!(" ({h})")).unwrap_or_default()
        ),
        Err(reason) => println!(
            "{}  docker          {reason}",
            if needs_docker { "FAIL" } else { "--  " }
        ),
    }
    println!(
        "{}  daemon          {}",
        check(true),
        if daemon::is_running() {
            "running"
        } else {
            "not running (starts on demand)"
        }
    );
    let installed = crate::release::installed();
    println!(
        "{}  server builds   {}",
        check(true),
        if installed.is_empty() {
            "none yet (downloaded on first start)".to_string()
        } else {
            installed
                .iter()
                .map(|i| format!("{} {}", i.tag, i.flavor))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    println!("      config          {}", paths::config_file().display());
    println!("      daemon log      {}", paths::daemon_log().display());
    Ok(())
}

/// Ask the daemon to stop; if it can't be reached or doesn't answer, kill
/// it and clean up whatever servers it left behind.
fn stop_daemon(force: bool) -> Result<()> {
    if !force && daemon::is_running() {
        let asked = Client::connect().and_then(|mut c| c.command(&Request::Shutdown));
        match asked {
            Ok(message) => {
                println!("{message}");
                // Return once it has actually exited (servers get 10s each,
                // stopped in parallel).
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                while daemon::is_running() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                if !daemon::is_running() {
                    println!("daemon stopped");
                    return Ok(());
                }
                println!("daemon is taking too long to stop; forcing it");
            }
            Err(err) => println!("daemon isn't responding ({err:#}); forcing it to stop"),
        }
    }
    let report = crate::rescue::force_stop_daemon()?;
    for line in report {
        println!("{line}");
    }
    Ok(())
}

fn client() -> Result<Client> {
    daemon::ensure_running()?;
    Client::connect()
}

fn say(request: Request) -> Result<()> {
    println!("{}", client()?.command(&request)?);
    Ok(())
}

fn status(json: bool) -> Result<()> {
    let snapshot = client()?.snapshot()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }
    println!(
        "{} of {} server(s) running · {} player(s) online · daemon up {}",
        snapshot.totals.servers_running,
        snapshot.totals.servers_total,
        snapshot.totals.players,
        fmt_duration(snapshot.daemon.uptime_secs)
    );
    for s in &snapshot.servers {
        let state = s.state.label();
        println!(
            "  {:<16} {:<10} port {:<6} {:<20} {}/{} players  {}",
            s.name,
            state,
            s.port,
            s.map,
            s.players.len(),
            s.max_players,
            match (s.state, &s.last_exit) {
                (ServerState::Running, _) => format!("up {}", fmt_duration(s.uptime_secs)),
                (_, Some(exit)) => exit.clone(),
                _ => String::new(),
            }
        );
    }
    if snapshot.servers.is_empty() {
        bail!("no servers configured — `beamhost server add NAME --key <auth key>`");
    }
    Ok(())
}
