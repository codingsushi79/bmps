//! The daemon: owns every server process and the config, and answers the
//! TUI and CLI over a unix socket.
//!
//! One `Hub` behind a std mutex holds all state. The lock is only ever held
//! for plain bookkeeping — never across an `.await`, a process spawn or a
//! file download — so a slow docker build cannot stall a snapshot.
//!
//! Per running server there are four small tasks: stdout and stderr readers
//! feeding a bounded console ring, a stdin writer, and a waiter that notices
//! the exit and decides between "stopped", "restart after backoff" and
//! "crashed". A one-second tick does everything periodic.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::config::{Config, Runtime, ServerSpec};
use crate::instance::Instance;
use crate::ipc::{Request, Response};
use crate::model::*;
use crate::{paths, release, runtime};

const CONSOLE_CAP: usize = 5000;
const LOG_CAP: usize = 500;
/// One sample every 5 s: 30 minutes of history.
const HISTORY_CAP: usize = 360;
const HISTORY_EVERY: u64 = 5;
const STOP_GRACE: Duration = Duration::from_secs(10);
/// Bridge status older than this means telemetry is lost.
const TELEMETRY_STALE: Duration = Duration::from_secs(5);
/// Give up restarting after this many crashes in a row.
const MAX_RESTARTS: u32 = 5;
/// A run longer than this resets the crash counter.
const STABLE_RUN: Duration = Duration::from_secs(300);
const RELEASE_REFRESH: Duration = Duration::from_secs(6 * 3600);
const LISTING_URL: &str = "https://backend.beammp.com/servers-info";

struct Console {
    lines: VecDeque<ConsoleLine>,
    next_seq: u64,
}

impl Console {
    fn new() -> Self {
        Self {
            lines: VecDeque::with_capacity(256),
            next_seq: 1,
        }
    }

    fn push(&mut self, text: String) {
        if self.lines.len() == CONSOLE_CAP {
            self.lines.pop_front();
        }
        self.lines.push_back(ConsoleLine {
            seq: self.next_seq,
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            text,
        });
        self.next_seq += 1;
    }

    fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// Lines after `after`; at most the newest `limit` of them.
    fn since(&self, after: u64, limit: usize) -> Vec<ConsoleLine> {
        let first = self.lines.front().map(|l| l.seq).unwrap_or(self.next_seq);
        let skip = after.saturating_add(1).saturating_sub(first) as usize;
        let available = self.lines.len().saturating_sub(skip);
        let start = skip + available.saturating_sub(limit);
        self.lines
            .range(start.min(self.lines.len())..)
            .cloned()
            .collect()
    }
}

struct Server {
    state: ServerState,
    /// Bumped on every launch, so the exit of an old process is never
    /// mistaken for the exit of the current one.
    generation: u64,
    want_running: bool,
    pid: Option<u32>,
    runtime: Runtime,
    version: String,
    started_at: Option<Instant>,
    stdin: Option<mpsc::Sender<String>>,
    console: Console,
    players: Vec<Player>,
    telemetry_at: Option<Instant>,
    cpu: f32,
    mem: u64,
    restarts: u32,
    restart_at: Option<Instant>,
    last_exit: Option<String>,
    last_error: Option<String>,
    listed: Option<bool>,
    history: VecDeque<u64>,
    mods: Vec<ModFile>,
    plugins: Vec<String>,
}

impl Server {
    fn new() -> Self {
        Self {
            state: ServerState::Stopped,
            generation: 0,
            want_running: false,
            pid: None,
            runtime: Runtime::Auto,
            version: String::new(),
            started_at: None,
            stdin: None,
            console: Console::new(),
            players: Vec::new(),
            telemetry_at: None,
            cpu: 0.0,
            mem: 0,
            restarts: 0,
            restart_at: None,
            last_exit: None,
            last_error: None,
            listed: None,
            history: VecDeque::with_capacity(HISTORY_CAP),
            mods: Vec::new(),
            plugins: Vec::new(),
        }
    }
}

#[derive(Default)]
struct ReleaseCache {
    list: Vec<release::Release>,
    latest: Option<String>,
    checked_at: Option<Instant>,
    installing: Option<(String, u64, u64)>,
    error: Option<String>,
    installed: Vec<Installed>,
    docker: Option<bool>,
}

struct Hub {
    config: Config,
    servers: HashMap<String, Server>,
    logs: VecDeque<LogEntry>,
    hardware: Hardware,
    releases: ReleaseCache,
    started: Instant,
    totals_history: VecDeque<u64>,
    peak_players: u64,
    shutting_down: bool,
}

impl Hub {
    fn log(&mut self, level: LogLevel, message: impl Into<String>) {
        let message = message.into();
        let at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        append_log_file(&at, level, &message);
        if self.logs.len() == LOG_CAP {
            self.logs.pop_front();
        }
        self.logs.push_back(LogEntry { at, level, message });
    }

    fn server(&mut self, name: &str) -> Result<&mut Server> {
        if self.config.server(name).is_none() {
            bail!("no server called `{name}`");
        }
        Ok(self
            .servers
            .entry(name.to_string())
            .or_insert_with(Server::new))
    }

    fn snapshot(&self) -> Snapshot {
        let mut servers = Vec::with_capacity(self.config.servers.len());
        for spec in &self.config.servers {
            let Some(server) = self.servers.get(&spec.name) else {
                continue;
            };
            let telemetry = server
                .telemetry_at
                .is_some_and(|t| t.elapsed() < TELEMETRY_STALE);
            servers.push(ServerStatus {
                name: spec.name.clone(),
                state: server.state,
                port: spec.port,
                map: spec.map_short(),
                version: if server.version.is_empty() {
                    self.config.version_for(spec)
                } else {
                    server.version.clone()
                },
                runtime: self.config.runtime_for(spec).label().to_string(),
                private: spec.private,
                auth_key_set: !spec.auth_key.trim().is_empty(),
                pid: server.pid,
                uptime_secs: server
                    .started_at
                    .filter(|_| server.state.is_live())
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or(0),
                players: if server.state == ServerState::Running {
                    server.players.clone()
                } else {
                    Vec::new()
                },
                max_players: spec.max_players,
                max_cars: spec.max_cars,
                telemetry,
                cpu: server.cpu,
                mem: server.mem,
                restarts: server.restarts,
                last_exit: server
                    .last_error
                    .clone()
                    .filter(|_| !server.state.is_live() || server.state == ServerState::Restarting)
                    .or_else(|| server.last_exit.clone()),
                listed: server.listed,
                mods: server.mods.clone(),
                plugins: server.plugins.clone(),
                history: server.history.iter().copied().collect(),
                autostart: spec.autostart,
                restart_on_crash: spec.restart_on_crash,
                description: spec.description.clone(),
                tags: spec.tags.clone(),
                console_seq: server.console.last_seq(),
            });
        }
        let totals = Totals {
            servers_total: servers.len(),
            servers_running: servers
                .iter()
                .filter(|s| s.state == ServerState::Running)
                .count(),
            players: servers.iter().map(|s| s.players.len()).sum(),
            slots: servers
                .iter()
                .filter(|s| s.state.is_live())
                .map(|s| s.max_players)
                .sum(),
            vehicles: servers.iter().map(|s| s.vehicles()).sum(),
            history: self.totals_history.iter().copied().collect(),
            peak_players: self.peak_players,
        };
        Snapshot {
            daemon: DaemonInfo {
                pid: std::process::id(),
                uptime_secs: self.started.elapsed().as_secs(),
                version: env!("CARGO_PKG_VERSION").into(),
                config_path: paths::config_file().display().to_string(),
            },
            servers,
            totals,
            hardware: self.hardware.clone(),
            releases: Releases {
                installed: self.releases.installed.clone(),
                latest: self.releases.latest.clone(),
                checked_secs_ago: self.releases.checked_at.map(|t| t.elapsed().as_secs()),
                installing: self.releases.installing.clone(),
                error: self.releases.error.clone(),
                host_flavor: release::host_flavor()
                    .unwrap_or_else(|| format!("docker: {}", release::docker_flavor())),
                docker: self.releases.docker,
            },
            logs: self.logs.iter().cloned().collect(),
        }
    }
}

struct Ctx {
    hub: Mutex<Hub>,
    /// One download at a time; a second start of the same version waits and
    /// then finds the binary already there.
    install_lock: tokio::sync::Mutex<()>,
    image_lock: tokio::sync::Mutex<()>,
    shutdown: tokio::sync::Notify,
}

impl Ctx {
    fn hub(&self) -> MutexGuard<'_, Hub> {
        lock_or_give_up(&self.hub, LOCK_PATIENCE)
    }

    fn log(&self, level: LogLevel, message: impl Into<String>) {
        self.hub().log(level, message);
    }
}

type Shared = Arc<Ctx>;

/// The hub lock is only ever held for bookkeeping (microseconds). Waiting
/// this long means a bug, most likely the same thread taking it twice, which
/// would otherwise freeze the daemon for good.
const LOCK_PATIENCE: Duration = Duration::from_secs(10);

/// Lock, but never wait forever. Past `patience` this panics: the panic
/// unwinds the stuck task (dropping any guard it holds, which frees the lock)
/// and tokio contains it, so one request fails instead of the whole daemon
/// hanging. A poisoned lock still holds plain data, so it is used anyway.
fn lock_or_give_up<T>(mutex: &Mutex<T>, patience: Duration) -> MutexGuard<'_, T> {
    use std::sync::TryLockError;
    let start = Instant::now();
    let mut spins = 0u32;
    loop {
        match mutex.try_lock() {
            Ok(guard) => return guard,
            Err(TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {}
        }
        if start.elapsed() > patience {
            let message = format!(
                "daemon state lock not released after {}s (a bug: please report it with daemon.log)",
                patience.as_secs()
            );
            append_log_file(
                &chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                LogLevel::Error,
                &message,
            );
            panic!("{message}");
        }
        spins += 1;
        if spins < 64 {
            std::hint::spin_loop();
        } else {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}

// ------------------------------------------------------------- startup ---

pub fn run_foreground() -> Result<()> {
    paths::ensure_dirs()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()?;
    runtime.block_on(serve())
}

async fn serve() -> Result<()> {
    let socket = paths::socket();
    if UnixStream::connect(&socket).await.is_ok() {
        bail!("a daemon is already running ({})", socket.display());
    }
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    restrict(&socket);
    std::fs::write(paths::pid_file(), std::process::id().to_string())?;

    // Servers left behind by a daemon that was killed would hold their
    // ports; stop them before anything starts.
    let reaped = tokio::task::spawn_blocking(crate::rescue::reap_orphans).await?;
    let config = Config::load()?;
    let mut servers = HashMap::new();
    for spec in &config.servers {
        servers.insert(spec.name.clone(), Server::new());
    }
    let ctx: Shared = Arc::new(Ctx {
        hub: Mutex::new(Hub {
            config,
            servers,
            logs: VecDeque::with_capacity(LOG_CAP),
            hardware: Hardware::default(),
            releases: ReleaseCache {
                installed: release::installed(),
                ..Default::default()
            },
            started: Instant::now(),
            totals_history: VecDeque::with_capacity(HISTORY_CAP),
            peak_players: 0,
            shutting_down: false,
        }),
        install_lock: tokio::sync::Mutex::new(()),
        image_lock: tokio::sync::Mutex::new(()),
        shutdown: tokio::sync::Notify::new(),
    });
    ctx.log(
        LogLevel::Info,
        format!(
            "daemon {} up (pid {})",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        ),
    );
    for line in reaped {
        ctx.log(LogLevel::Warn, format!("cleanup: {line}"));
    }
    refresh_files(&ctx);

    tokio::spawn(ticker(ctx.clone()));
    tokio::spawn(release_refresher(ctx.clone()));
    tokio::spawn(listing_checker(ctx.clone()));

    let autostart: Vec<String> = ctx
        .hub()
        .config
        .servers
        .iter()
        .filter(|s| s.autostart)
        .map(|s| s.name.clone())
        .collect();
    for name in autostart {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(err) = start_server(&ctx, &name).await {
                ctx.log(LogLevel::Error, format!("autostart `{name}`: {err:#}"));
            }
        });
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    tokio::spawn(connection(ctx.clone(), stream));
                }
            }
            _ = ctx.shutdown.notified() => break,
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }

    ctx.log(LogLevel::Info, "shutting down: stopping servers");
    ctx.hub().shutting_down = true;
    stop_all(&ctx).await;
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(paths::pid_file());
    ctx.log(LogLevel::Info, "daemon stopped");
    Ok(())
}

fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

fn append_log_file(at: &str, level: LogLevel, message: &str) {
    use std::io::Write;
    // Unit tests must never write into a real user's log.
    if cfg!(test) {
        return;
    }
    let path = paths::daemon_log();
    // Crude rotation: one previous file, 4 MiB each.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 4 * 1024 * 1024) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{at} {level:?} {message}");
    }
}

// ------------------------------------------------------------------ ipc ---

async fn connection(ctx: Shared, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => handle(&ctx, request).await,
            Err(err) => Response::err(format!("bad request: {err}")),
        };
        let mut payload = match serde_json::to_vec(&response) {
            Ok(p) => p,
            Err(err) => serde_json::to_vec(&Response::err(err.to_string())).unwrap_or_default(),
        };
        payload.push(b'\n');
        if write.write_all(&payload).await.is_err() {
            return;
        }
    }
}

async fn handle(ctx: &Shared, request: Request) -> Response {
    let result: Result<String> = match request {
        Request::Snapshot => {
            let snapshot = ctx.hub().snapshot();
            return Response {
                snapshot: Some(Box::new(snapshot)),
                ..Response::ok("")
            };
        }
        Request::Console {
            server,
            after,
            limit,
        } => {
            let mut hub = ctx.hub();
            return match hub.server(&server) {
                Ok(s) => Response {
                    console: Some(s.console.since(after, limit.clamp(1, CONSOLE_CAP))),
                    ..Response::ok("")
                },
                Err(err) => Response::err(err.to_string()),
            };
        }
        Request::Start { name } => start_server(ctx, &name)
            .await
            .map(|_| format!("`{name}` starting")),
        Request::Stop { name } => stop_server(ctx, &name)
            .await
            .map(|_| format!("`{name}` stopped")),
        Request::Restart { name } => {
            async {
                stop_server(ctx, &name).await?;
                start_server(ctx, &name).await?;
                Ok(format!("`{name}` restarting"))
            }
            .await
        }
        Request::StartAll => {
            let names: Vec<String> = ctx
                .hub()
                .config
                .servers
                .iter()
                .map(|s| s.name.clone())
                .collect();
            let mut started = 0;
            let mut errors = Vec::new();
            for name in names {
                match start_server(ctx, &name).await {
                    Ok(true) => started += 1,
                    Ok(false) => {}
                    Err(err) => errors.push(format!("{name}: {err:#}")),
                }
            }
            if errors.is_empty() {
                Ok(format!("{started} server(s) starting"))
            } else {
                Err(anyhow!("{}", errors.join("; ")))
            }
        }
        Request::StopAll => {
            stop_all(ctx).await;
            Ok("all servers stopped".into())
        }
        Request::AddServer { spec } => add_server(ctx, spec),
        Request::UpdateServer { spec } => update_server(ctx, spec),
        Request::RemoveServer { name, purge } => remove_server(ctx, &name, purge).await,
        Request::SetAuthKey { name, key } => set_auth_key(ctx, &name, key),
        Request::Install { version } => install_request(ctx, version).await,
        Request::CheckReleases => check_releases(ctx).await,
        Request::Command { server, line } => send_stdin(ctx, &server, line).await,
        Request::Say { server, message } => {
            bridge_command(ctx, &server, format!("say {message}")).map(|_| "message sent".into())
        }
        Request::Kick {
            server,
            player,
            reason,
        } => bridge_command(ctx, &server, format!("kick {player} {reason}"))
            .map(|_| format!("kicked player {player}")),
        Request::AddMod { server, path } => {
            mod_op(ctx, &server, move |i| {
                let added = i.add_mod(Path::new(&path))?;
                Ok(format!("added {}", added.join(", ")))
            })
            .await
        }
        Request::RemoveMod { server, file } => {
            mod_op(ctx, &server, move |i| {
                i.remove_mod(&file)?;
                Ok(format!("removed {file}"))
            })
            .await
        }
        Request::ToggleMod { server, file } => {
            mod_op(ctx, &server, move |i| {
                let on = i.toggle_mod(&file)?;
                Ok(format!(
                    "{file} {}",
                    if on { "enabled" } else { "disabled" }
                ))
            })
            .await
        }
        Request::Reload => reload(ctx).await,
        Request::Shutdown => {
            // Answer first: exit only after this reply is on its way.
            let ctx = ctx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                ctx.shutdown.notify_one();
            });
            Ok("daemon shutting down; stopping servers".into())
        }
    };
    match result {
        Ok(message) => Response::ok(message),
        Err(err) => Response::err(format!("{err:#}")),
    }
}

// --------------------------------------------------------------- config ---

fn save_config(hub: &mut Hub, next: Config) -> Result<()> {
    next.save()?;
    hub.config = next;
    Ok(())
}

fn add_server(ctx: &Shared, spec: ServerSpec) -> Result<String> {
    let mut hub = ctx.hub();
    let mut next = hub.config.clone();
    let name = spec.name.clone();
    next.upsert(spec, false)?;
    save_config(&mut hub, next)?;
    hub.servers.insert(name.clone(), Server::new());
    hub.log(LogLevel::Info, format!("server `{name}` added"));
    drop(hub);
    // Lay out the directory now so mods can be added before the first start.
    if let Some(spec) = ctx.hub().config.server(&name).cloned() {
        let _ = Instance::new(&name).prepare(&spec);
    }
    refresh_files(ctx);
    Ok(format!("server `{name}` added — press s to start it"))
}

fn update_server(ctx: &Shared, spec: ServerSpec) -> Result<String> {
    let mut hub = ctx.hub();
    let mut next = hub.config.clone();
    let name = spec.name.clone();
    next.upsert(spec, true)?;
    save_config(&mut hub, next)?;
    let live = hub.servers.get(&name).is_some_and(|s| s.state.is_live());
    hub.log(LogLevel::Info, format!("server `{name}` updated"));
    Ok(if live {
        format!("`{name}` saved — restart it (R) to apply")
    } else {
        format!("`{name}` saved")
    })
}

fn set_auth_key(ctx: &Shared, name: &str, key: String) -> Result<String> {
    let key = key.trim().to_string();
    if !key.is_empty() && !crate::config::is_plausible_key(&key) {
        bail!("that does not look like a keymaster key (36-char UUID)");
    }
    let mut hub = ctx.hub();
    let mut next = hub.config.clone();
    next.server_mut(name)
        .with_context(|| format!("no server called `{name}`"))?
        .auth_key = key;
    save_config(&mut hub, next)?;
    Ok(format!("auth key for `{name}` saved"))
}

async fn remove_server(ctx: &Shared, name: &str, purge: bool) -> Result<String> {
    if ctx.hub().config.server(name).is_none() {
        bail!("no server called `{name}`");
    }
    stop_server(ctx, name).await?;
    {
        let mut hub = ctx.hub();
        let mut next = hub.config.clone();
        next.remove(name)?;
        save_config(&mut hub, next)?;
        hub.servers.remove(name);
        hub.log(LogLevel::Info, format!("server `{name}` removed"));
    }
    if purge {
        let dir = paths::server_dir(name);
        if dir.starts_with(paths::servers_dir()) && dir.exists() {
            tokio::fs::remove_dir_all(&dir).await?;
        }
        Ok(format!("`{name}` removed with its files"))
    } else {
        Ok(format!(
            "`{name}` removed (files kept in {})",
            paths::server_dir(name).display()
        ))
    }
}

async fn reload(ctx: &Shared) -> Result<String> {
    let config = Config::load()?;
    let gone: Vec<String> = {
        let mut hub = ctx.hub();
        let gone = hub
            .servers
            .keys()
            .filter(|n| config.server(n).is_none())
            .cloned()
            .collect();
        for spec in &config.servers {
            hub.servers
                .entry(spec.name.clone())
                .or_insert_with(Server::new);
        }
        hub.config = config;
        gone
    };
    for name in &gone {
        // Still running under the old config: stop it before forgetting it.
        let _ = stop_process(ctx, name).await;
        ctx.hub().servers.remove(name);
    }
    ctx.log(LogLevel::Info, "config reloaded");
    refresh_files(ctx);
    Ok(format!("config reloaded ({} removed)", gone.len()))
}

async fn mod_op(
    ctx: &Shared,
    server: &str,
    op: impl FnOnce(&Instance) -> Result<String> + Send + 'static,
) -> Result<String> {
    if ctx.hub().config.server(server).is_none() {
        bail!("no server called `{server}`");
    }
    let instance = Instance::new(server);
    let message = tokio::task::spawn_blocking(move || op(&instance)).await??;
    refresh_files(ctx);
    let live = ctx
        .hub()
        .servers
        .get(server)
        .is_some_and(|s| s.state == ServerState::Running);
    ctx.log(LogLevel::Info, format!("{server}: {message}"));
    Ok(if live {
        format!("{message} — players get it after a restart (R)")
    } else {
        message
    })
}

// ------------------------------------------------------------ processes ---

/// Start a server. `Ok(false)` when it was already up.
async fn start_server(ctx: &Shared, name: &str) -> Result<bool> {
    let (spec, runtime, version, image, generation) = {
        let mut hub = ctx.hub();
        if hub.shutting_down {
            bail!("the daemon is shutting down");
        }
        let spec = hub
            .config
            .server(name)
            .cloned()
            .with_context(|| format!("no server called `{name}`"))?;
        let runtime = hub.config.runtime_for(&spec);
        let version = hub.config.version_for(&spec);
        let image = hub.config.settings.docker_image.clone();
        let server = hub.server(name)?;
        if server.state.is_live() && server.state != ServerState::Restarting {
            return Ok(false);
        }
        server.state = ServerState::Preparing;
        server.generation += 1;
        server.want_running = true;
        server.restart_at = None;
        server.last_error = None;
        server.runtime = runtime;
        (spec, runtime, version, image, server.generation)
    };

    match launch(ctx, &spec, runtime, &version, &image, generation).await {
        Ok(()) => Ok(true),
        Err(err) => {
            let mut hub = ctx.hub();
            if let Some(server) = hub.servers.get_mut(name)
                && server.generation == generation
            {
                server.state = ServerState::Stopped;
                server.want_running = false;
                server.last_error = Some(format!("{err:#}"));
            }
            hub.log(LogLevel::Error, format!("{name}: failed to start: {err:#}"));
            Err(err)
        }
    }
}

async fn launch(
    ctx: &Shared,
    spec: &ServerSpec,
    runtime: Runtime,
    version: &str,
    image: &str,
    generation: u64,
) -> Result<()> {
    // BeamMP-Server refuses an empty key, but only a public server needs a
    // real one (to register on the server list). A private server gets a
    // placeholder so it can run for direct connect without keymaster.
    let keyless = spec.auth_key.trim().is_empty();
    if keyless && !spec.private {
        bail!(
            "no auth key: public servers need one to appear on the BeamMP server list. Get a \
             free key at https://keymaster.beammp.com (Keys → New) and press K, or make the \
             server private (e) to run it without a key for direct connect"
        );
    }
    let name = spec.name.clone();
    if runtime == Runtime::Docker {
        let engine = tokio::task::spawn_blocking(|| crate::docker::detect(true)).await?;
        ctx.hub().releases.docker = Some(engine.is_ok());
        match engine {
            Ok(engine) => ctx.log(LogLevel::Info, format!("docker: using {}", engine.via)),
            Err(reason) => bail!(
                "BeamMP only ships Linux builds, so on this OS servers run in Docker, and {reason}"
            ),
        }
        let _guard = ctx.image_lock.lock().await;
        let image_owned = image.to_string();
        tokio::task::spawn_blocking(move || runtime::ensure_image(&image_owned)).await??;
        let stale = name.clone();
        tokio::task::spawn_blocking(move || runtime::remove_stale_container(&stale)).await?;
    }
    let (binary, tag) = resolve_binary(ctx, runtime, version).await?;
    let instance = Instance::new(&name);
    let mut spec_owned = spec.clone();
    let dir = instance.dir.clone();
    tokio::task::spawn_blocking(move || {
        if keyless {
            spec_owned.auth_key = instance.placeholder_key()?;
        }
        instance.prepare(&spec_owned)
    })
    .await??;

    let mut command = runtime::build_command(runtime, spec, &binary, &dir, image);
    let mut child = command
        .spawn()
        .with_context(|| format!("launching {}", binary.display()))?;
    let pid = child.id();
    if let Some(pid) = pid {
        crate::rescue::record(&spec.name, pid, runtime);
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdin = child.stdin.take();

    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(32);
    {
        let mut hub = ctx.hub();
        let server = hub.server(&name)?;
        if server.generation != generation || !server.want_running {
            // Stopped while we were preparing: do not leave it running.
            drop(hub);
            let _ = child.start_kill();
            bail!("cancelled");
        }
        server.state = ServerState::Starting;
        server.pid = pid;
        server.version = tag.clone();
        server.started_at = Some(Instant::now());
        server.stdin = Some(stdin_tx);
        server.players.clear();
        server.telemetry_at = None;
        server.listed = None;
        server.console.push(format!(
            "── beamhost: starting {tag} ({}) ──",
            runtime.label()
        ));
        hub.log(
            LogLevel::Info,
            format!(
                "{name}: started {tag} on port {} (pid {})",
                spec.port,
                pid.unwrap_or(0)
            ),
        );
    }

    if let Some(stdout) = stdout {
        tokio::spawn(pump(ctx.clone(), name.clone(), generation, stdout));
    }
    if let Some(stderr) = stderr {
        tokio::spawn(pump(ctx.clone(), name.clone(), generation, stderr));
    }
    if let Some(mut stdin) = stdin {
        tokio::spawn(async move {
            while let Some(mut line) = stdin_rx.recv().await {
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });
    }
    let waiter_ctx = ctx.clone();
    tokio::spawn(async move {
        let status = child.wait().await;
        on_exit(&waiter_ctx, &name, generation, status);
    });
    Ok(())
}

/// Copy a pipe into the console ring, line by line, tolerating bad UTF-8 and
/// the colour codes the server writes.
async fn pump(ctx: Shared, name: String, generation: u64, pipe: impl AsyncRead + Unpin) {
    let mut reader = BufReader::with_capacity(16 * 1024, pipe);
    let mut buf = Vec::with_capacity(512);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = strip_ansi(&String::from_utf8_lossy(&buf));
        let text = text.trim_end_matches(['\r', '\n']).trim_start_matches("> ");
        if text.trim().is_empty() {
            continue;
        }
        let mut hub = ctx.hub();
        let Some(server) = hub.servers.get_mut(&name) else {
            return;
        };
        if server.generation != generation {
            return;
        }
        let lower = text.to_ascii_lowercase();
        if server.state == ServerState::Starting
            && (text.contains("[beamhost] bridge online") || lower.contains("all systems started"))
        {
            server.state = ServerState::Running;
        }
        if lower.contains("[error]") || lower.contains("authkey") && lower.contains("invalid") {
            server.last_error = Some(truncate(text, 200));
        }
        server.console.push(text.to_string());
    }
}

fn on_exit(
    ctx: &Shared,
    name: &str,
    generation: u64,
    status: std::io::Result<std::process::ExitStatus>,
) {
    let mut hub = ctx.hub();
    let shutting_down = hub.shutting_down;
    let restart_on_crash = hub.config.server(name).is_some_and(|s| s.restart_on_crash);
    let Some(server) = hub.servers.get_mut(name) else {
        return;
    };
    if server.generation != generation {
        return;
    }
    let ran = server.started_at.map(|t| t.elapsed()).unwrap_or_default();
    let described = match &status {
        Ok(s) => describe_exit(s),
        Err(err) => format!("wait failed: {err}"),
    };
    server.pid = None;
    crate::rescue::clear(name);
    server.stdin = None;
    server.players.clear();
    server.telemetry_at = None;
    server.cpu = 0.0;
    server.mem = 0;
    server.listed = None;
    server.last_exit = Some(described.clone());
    server
        .console
        .push(format!("── beamhost: process {described} ──"));

    let message;
    let level;
    if !server.want_running || shutting_down {
        server.state = ServerState::Stopped;
        level = LogLevel::Info;
        message = format!("{name}: stopped ({described})");
    } else {
        if ran >= STABLE_RUN {
            server.restarts = 0;
        }
        if restart_on_crash && server.restarts < MAX_RESTARTS {
            let delay = Duration::from_secs((2u64 << server.restarts.min(5)).min(60));
            server.restarts += 1;
            server.state = ServerState::Restarting;
            server.restart_at = Some(Instant::now() + delay);
            level = LogLevel::Warn;
            message = format!(
                "{name}: exited unexpectedly ({described}); restart {} of {MAX_RESTARTS} in {}s",
                server.restarts,
                delay.as_secs()
            );
        } else {
            server.state = ServerState::Crashed;
            server.want_running = false;
            level = LogLevel::Error;
            message = if restart_on_crash {
                format!("{name}: crashed {MAX_RESTARTS} times in a row ({described}); giving up")
            } else {
                format!("{name}: crashed ({described})")
            };
        }
    }
    hub.log(level, message);
}

fn describe_exit(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(0), _) => "exited cleanly".into(),
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        _ => "exited".into(),
    }
}

/// Stop the process (if any) and wait for it, escalating to SIGKILL after the
/// grace period. Leaves `want_running` false.
async fn stop_process(ctx: &Shared, name: &str) -> Result<()> {
    let (pid, generation, runtime) = {
        let mut hub = ctx.hub();
        let Some(server) = hub.servers.get_mut(name) else {
            return Ok(());
        };
        server.want_running = false;
        server.restart_at = None;
        match server.state {
            ServerState::Stopped | ServerState::Crashed => return Ok(()),
            ServerState::Restarting | ServerState::Preparing if server.pid.is_none() => {
                // Nothing running yet; a launch in flight sees want_running
                // and cancels itself.
                server.state = ServerState::Stopped;
                return Ok(());
            }
            _ => {}
        }
        server.state = ServerState::Stopping;
        (server.pid, server.generation, server.runtime)
    };
    let Some(pid) = pid else {
        return Ok(());
    };
    // Native servers lead their own process group; the docker CLI forwards
    // the signal into the container (`--init` makes PID 1 honour it).
    let target = if runtime == Runtime::Docker {
        pid as i32
    } else {
        -(pid as i32)
    };
    unsafe {
        libc::kill(target, libc::SIGTERM);
    }
    let deadline = Instant::now() + STOP_GRACE;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let done = {
            let hub = ctx.hub();
            hub.servers
                .get(name)
                .is_none_or(|s| s.generation != generation || s.pid.is_none())
        };
        if done {
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    ctx.log(
        LogLevel::Warn,
        format!(
            "{name}: did not stop within {}s, killing",
            STOP_GRACE.as_secs()
        ),
    );
    unsafe {
        libc::kill(target, libc::SIGKILL);
    }
    if runtime == Runtime::Docker {
        let stale = name.to_string();
        let _ = tokio::task::spawn_blocking(move || runtime::remove_stale_container(&stale)).await;
    }
    Ok(())
}

async fn stop_server(ctx: &Shared, name: &str) -> Result<()> {
    if ctx.hub().config.server(name).is_none() {
        bail!("no server called `{name}`");
    }
    stop_process(ctx, name).await
}

async fn stop_all(ctx: &Shared) {
    let names: Vec<String> = ctx.hub().servers.keys().cloned().collect();
    // In parallel: total shutdown time is one grace period, not N of them.
    let tasks: Vec<_> = names
        .into_iter()
        .map(|name| {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let _ = stop_process(&ctx, &name).await;
            })
        })
        .collect();
    for task in tasks {
        let _ = task.await;
    }
}

async fn send_stdin(ctx: &Shared, server: &str, line: String) -> Result<String> {
    let sender = {
        let mut hub = ctx.hub();
        let s = hub.server(server)?;
        s.console.push(format!("> {line}"));
        s.stdin.clone().context("server is not running")?
    };
    sender
        .send(line)
        .await
        .map_err(|_| anyhow!("server console is closed"))?;
    Ok("sent".into())
}

fn bridge_command(ctx: &Shared, server: &str, line: String) -> Result<()> {
    {
        let mut hub = ctx.hub();
        let s = hub.server(server)?;
        if s.state != ServerState::Running {
            bail!("`{server}` is not running");
        }
        if s.telemetry_at
            .is_none_or(|t| t.elapsed() >= TELEMETRY_STALE)
        {
            bail!("the beamhost bridge plugin is not responding on `{server}`");
        }
    }
    Instance::new(server).queue_command(&line)
}

// ------------------------------------------------------------- releases ---

async fn resolve_binary(
    ctx: &Shared,
    runtime: Runtime,
    version: &str,
) -> Result<(PathBuf, String)> {
    let flavor = match runtime {
        Runtime::Docker => release::docker_flavor(),
        _ => release::host_flavor().ok_or_else(|| {
            anyhow!(
                "BeamMP-Server has no native build for this OS; set the server's runtime to docker"
            )
        })?,
    };
    let tag = if version.eq_ignore_ascii_case("latest") {
        let cached = ctx.hub().releases.latest.clone();
        match cached {
            Some(tag) => tag,
            None => match refresh_release_list(ctx).await {
                Ok(()) => ctx
                    .hub()
                    .releases
                    .latest
                    .clone()
                    .context("GitHub lists no BeamMP-Server releases")?,
                // Offline: the newest build already on disk will do.
                Err(err) => release::newest_installed(&flavor).ok_or(err)?,
            },
        }
    } else {
        release::normalize_tag(version)
    };
    let path = release::binary_path(&tag, &flavor);
    if path.exists() {
        return Ok((path, tag));
    }
    install(ctx, &tag, &flavor).await?;
    Ok((path, tag))
}

async fn refresh_release_list(ctx: &Shared) -> Result<()> {
    match tokio::task::spawn_blocking(release::fetch_releases).await? {
        Ok(list) => {
            let mut hub = ctx.hub();
            hub.releases.latest = release::latest(&list).map(|r| r.tag_name.clone());
            hub.releases.list = list;
            hub.releases.checked_at = Some(Instant::now());
            hub.releases.error = None;
            Ok(())
        }
        Err(err) => {
            ctx.hub().releases.error = Some(format!("{err:#}"));
            Err(err)
        }
    }
}

async fn install(ctx: &Shared, tag: &str, flavor: &str) -> Result<PathBuf> {
    let _guard = ctx.install_lock.lock().await;
    let dest = release::binary_path(tag, flavor);
    if dest.exists() {
        return Ok(dest);
    }
    let mut asset = ctx
        .hub()
        .releases
        .list
        .iter()
        .find(|r| r.tag_name == tag)
        .and_then(|r| r.asset(flavor).cloned());
    if asset.is_none() {
        refresh_release_list(ctx).await?;
        asset = ctx
            .hub()
            .releases
            .list
            .iter()
            .find(|r| r.tag_name == tag)
            .and_then(|r| r.asset(flavor).cloned());
    }
    let asset = asset.with_context(|| format!("release {tag} has no {flavor} build"))?;
    ctx.hub().releases.installing = Some((tag.to_string(), 0, asset.size));
    ctx.log(
        LogLevel::Info,
        format!("downloading {} ({tag})", asset.name),
    );

    let progress_ctx = ctx.clone();
    let (tag_owned, flavor_owned) = (tag.to_string(), flavor.to_string());
    let result = tokio::task::spawn_blocking(move || {
        release::install(&tag_owned, &flavor_owned, &asset, |done, total| {
            if let Some(installing) = progress_ctx.hub().releases.installing.as_mut() {
                installing.1 = done;
                installing.2 = total;
            }
        })
    })
    .await?;

    let mut hub = ctx.hub();
    hub.releases.installing = None;
    hub.releases.installed = release::installed();
    match result {
        Ok(path) => {
            hub.log(LogLevel::Info, format!("installed {tag} ({flavor})"));
            Ok(path)
        }
        Err(err) => {
            hub.log(LogLevel::Error, format!("install {tag} failed: {err:#}"));
            hub.releases.error = Some(format!("{err:#}"));
            Err(err)
        }
    }
}

async fn install_request(ctx: &Shared, version: Option<String>) -> Result<String> {
    let flavor = {
        // One lock, read once: the hub mutex is not re-entrant, and taking
        // it again inside this expression used to deadlock the daemon.
        let docker = {
            let hub = ctx.hub();
            hub.config
                .servers
                .iter()
                .any(|s| hub.config.runtime_for(s) == Runtime::Docker)
        };
        match release::host_flavor() {
            Some(flavor) if !docker => flavor,
            _ => release::docker_flavor(),
        }
    };
    let tag = match version {
        Some(v) if !v.eq_ignore_ascii_case("latest") => release::normalize_tag(&v),
        _ => {
            refresh_release_list(ctx).await?;
            ctx.hub()
                .releases
                .latest
                .clone()
                .context("no releases found")?
        }
    };
    if release::binary_path(&tag, &flavor).exists() {
        return Ok(format!("{tag} ({flavor}) is already installed"));
    }
    // Downloads run in the background; progress shows on the dashboard.
    let ctx = ctx.clone();
    let (tag2, flavor2) = (tag.clone(), flavor.clone());
    tokio::spawn(async move {
        let _ = install(&ctx, &tag2, &flavor2).await;
    });
    Ok(format!("installing {tag} ({flavor})"))
}

async fn check_releases(ctx: &Shared) -> Result<String> {
    refresh_release_list(ctx).await?;
    let latest = ctx.hub().releases.latest.clone().unwrap_or_default();
    Ok(format!("latest BeamMP-Server release: {latest}"))
}

async fn release_refresher(ctx: Shared) {
    loop {
        if let Err(err) = refresh_release_list(&ctx).await {
            ctx.log(LogLevel::Warn, format!("release check failed: {err:#}"));
        }
        tokio::time::sleep(RELEASE_REFRESH).await;
    }
}

// --------------------------------------------------------------- listing ---

/// Confirm public servers actually appear on the BeamMP server list, which is
/// the question every host asks ("can people see my server?").
async fn listing_checker(ctx: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let targets: Vec<(String, u16, String)> = {
            let hub = ctx.hub();
            if !hub.config.settings.check_listing {
                continue;
            }
            hub.config
                .servers
                .iter()
                .filter(|s| !s.private)
                .filter(|s| {
                    hub.servers
                        .get(&s.name)
                        .is_some_and(|r| r.state == ServerState::Running)
                })
                .map(|s| (s.name.clone(), s.port, strip_beam_codes(&s.name_or_title())))
                .collect()
        };
        if targets.is_empty() {
            continue;
        }
        let list = tokio::task::spawn_blocking(|| -> Result<serde_json::Value> {
            let mut response = ureq::get(LISTING_URL)
                .header(
                    "User-Agent",
                    concat!("beamhost/", env!("CARGO_PKG_VERSION")),
                )
                .call()?;
            let text = response
                .body_mut()
                .with_config()
                .limit(32 * 1024 * 1024)
                .read_to_string()?;
            Ok(serde_json::from_str(&text)?)
        })
        .await;
        let Ok(Ok(serde_json::Value::Array(entries))) = list else {
            continue;
        };
        let mut hub = ctx.hub();
        for (name, port, title) in targets {
            let found = entries.iter().any(|e| listing_matches(e, port, &title));
            if let Some(server) = hub.servers.get_mut(&name) {
                server.listed = Some(found);
            }
        }
    }
}

fn listing_matches(entry: &serde_json::Value, port: u16, title: &str) -> bool {
    let entry_port = match &entry["port"] {
        serde_json::Value::String(s) => s.parse::<u16>().ok(),
        serde_json::Value::Number(n) => n.as_u64().map(|n| n as u16),
        _ => None,
    };
    let name = entry["sname"].as_str().map(strip_beam_codes);
    entry_port == Some(port) && name.as_deref().map(str::trim) == Some(title.trim())
}

// ----------------------------------------------------------------- tick ---

async fn ticker(ctx: Shared) {
    let mut system = sysinfo::System::new();
    let mut ticks: u64 = 0;
    let mut interval = tokio::time::interval(Duration::from_millis(
        ctx.hub()
            .config
            .settings
            .sample_interval_ms
            .clamp(250, 10_000),
    ));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        ticks += 1;
        sample(&ctx, &mut system, ticks);
        read_bridges(&ctx);
        due_restarts(&ctx);
        if ticks.is_multiple_of(3) {
            refresh_files(&ctx);
        }
        if ticks.is_multiple_of(5) {
            docker_stats(&ctx).await;
        }
        if ticks.is_multiple_of(HISTORY_EVERY) {
            record_history(&ctx);
        }
        if ticks.is_multiple_of(30) {
            let docker_used = {
                let hub = ctx.hub();
                hub.config
                    .servers
                    .iter()
                    .any(|s| hub.config.runtime_for(s) == Runtime::Docker)
            };
            if docker_used {
                let available = tokio::task::spawn_blocking(runtime::docker_available)
                    .await
                    .unwrap_or(false);
                ctx.hub().releases.docker = Some(available);
            }
        }
    }
}

fn sample(ctx: &Shared, system: &mut sysinfo::System, ticks: u64) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate};
    system.refresh_cpu_usage();
    system.refresh_memory();
    let native: Vec<(String, u32)> = {
        let hub = ctx.hub();
        hub.servers
            .iter()
            .filter(|(_, s)| s.runtime != Runtime::Docker)
            .filter_map(|(n, s)| s.pid.map(|p| (n.clone(), p)))
            .collect()
    };
    if !native.is_empty() {
        let pids: Vec<Pid> = native.iter().map(|(_, p)| Pid::from_u32(*p)).collect();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
    }
    let mut hub = ctx.hub();
    let cpus = system.cpus();
    hub.hardware.cpu_usage = system.global_cpu_usage();
    hub.hardware.per_core = cpus.iter().map(|c| c.cpu_usage()).collect();
    hub.hardware.cores = cpus.len();
    hub.hardware.mem_used = system.used_memory();
    hub.hardware.mem_total = system.total_memory();
    let load = sysinfo::System::load_average();
    hub.hardware.load_avg = [load.one, load.five, load.fifteen];
    if ticks == 1 {
        hub.hardware.cpu_brand = cpus
            .first()
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_default();
        hub.hardware.os = sysinfo::System::long_os_version().unwrap_or_default();
        hub.hardware.host = sysinfo::System::host_name().unwrap_or_default();
    }
    for (name, pid) in native {
        if let Some(process) = system.process(Pid::from_u32(pid))
            && let Some(server) = hub.servers.get_mut(&name)
        {
            server.cpu = process.cpu_usage();
            server.mem = process.memory();
        }
    }
}

fn read_bridges(ctx: &Shared) {
    let live: Vec<String> = {
        let hub = ctx.hub();
        hub.servers
            .iter()
            .filter(|(_, s)| matches!(s.state, ServerState::Starting | ServerState::Running))
            .map(|(n, _)| n.clone())
            .collect()
    };
    for name in live {
        let status = Instance::new(&name).read_status();
        let now = chrono::Utc::now().timestamp();
        let mut hub = ctx.hub();
        let Some(server) = hub.servers.get_mut(&name) else {
            continue;
        };
        match status {
            Some(status) if (now - status.time).abs() <= TELEMETRY_STALE.as_secs() as i64 => {
                server.players = status.players;
                server.telemetry_at = Some(Instant::now());
                if server.state == ServerState::Starting {
                    server.state = ServerState::Running;
                }
            }
            _ => {
                // No bridge (yet): a server that has been up a while without
                // dying is running even if the plugin failed to load.
                if server.state == ServerState::Starting
                    && server
                        .started_at
                        .is_some_and(|t| t.elapsed() > Duration::from_secs(20))
                {
                    server.state = ServerState::Running;
                }
            }
        }
    }
}

fn due_restarts(ctx: &Shared) {
    let due: Vec<String> = {
        let hub = ctx.hub();
        hub.servers
            .iter()
            .filter(|(_, s)| {
                s.state == ServerState::Restarting
                    && s.restart_at.is_some_and(|t| Instant::now() >= t)
            })
            .map(|(n, _)| n.clone())
            .collect()
    };
    for name in due {
        if let Some(server) = ctx.hub().servers.get_mut(&name) {
            server.restart_at = None;
        }
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(err) = start_server(&ctx, &name).await {
                ctx.log(LogLevel::Error, format!("{name}: restart failed: {err:#}"));
            }
        });
    }
}

fn record_history(ctx: &Shared) {
    let mut hub = ctx.hub();
    let mut total = 0u64;
    for server in hub.servers.values_mut() {
        let players = if server.state == ServerState::Running {
            server.players.len() as u64
        } else {
            0
        };
        total += players;
        if server.history.len() == HISTORY_CAP {
            server.history.pop_front();
        }
        server.history.push_back(players);
    }
    if hub.totals_history.len() == HISTORY_CAP {
        hub.totals_history.pop_front();
    }
    hub.totals_history.push_back(total);
    hub.peak_players = hub.peak_players.max(total);
}

/// Mods and plugins change on disk (users drop zips in by hand), so re-list
/// them every few seconds rather than on every snapshot.
fn refresh_files(ctx: &Shared) {
    let names: Vec<String> = ctx.hub().servers.keys().cloned().collect();
    let listed: Vec<(String, Vec<ModFile>, Vec<String>)> = names
        .into_iter()
        .map(|name| {
            let instance = Instance::new(&name);
            let (mods, plugins) = (instance.mods(), instance.plugins());
            (name, mods, plugins)
        })
        .collect();
    let installed = release::installed();
    let mut hub = ctx.hub();
    for (name, mods, plugins) in listed {
        if let Some(server) = hub.servers.get_mut(&name) {
            server.mods = mods;
            server.plugins = plugins;
        }
    }
    hub.releases.installed = installed;
}

/// Containers are not our children, so their usage comes from docker itself:
/// one `docker stats` call for every container, only when any are running.
async fn docker_stats(ctx: &Shared) {
    let names: Vec<String> = {
        let hub = ctx.hub();
        hub.servers
            .iter()
            .filter(|(_, s)| s.runtime == Runtime::Docker && s.pid.is_some())
            .map(|(n, _)| n.clone())
            .collect()
    };
    if names.is_empty() {
        return;
    }
    let output = tokio::task::spawn_blocking(|| {
        crate::docker::command()
            .args([
                "stats",
                "--no-stream",
                "--format",
                "{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}",
            ])
            .output()
    })
    .await;
    let Ok(Ok(output)) = output else {
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut hub = ctx.hub();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let (Some(container), Some(cpu), Some(mem)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Some(name) = container.strip_prefix("beamhost-") else {
            continue;
        };
        if let Some(server) = hub.servers.get_mut(name) {
            server.cpu = cpu.trim_end_matches('%').trim().parse().unwrap_or(0.0);
            server.mem = parse_docker_mem(mem.split('/').next().unwrap_or(""));
        }
    }
}

fn parse_docker_mem(text: &str) -> u64 {
    let text = text.trim();
    let split = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number.trim().parse().unwrap_or(0.0);
    let scale = match unit.trim() {
        "KiB" | "kB" | "KB" => 1024.0,
        "MiB" | "MB" => 1024.0 * 1024.0,
        "GiB" | "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (value * scale) as u64
}

pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ params final-byte
            if chars.clone().next() == Some('[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

// ------------------------------------------------------------ lifecycle ---

/// Is a daemon answering on the socket?
pub fn is_running() -> bool {
    std::os::unix::net::UnixStream::connect(paths::socket()).is_ok()
}

/// Start the daemon detached from this terminal, then wait for its socket.
pub fn spawn_detached() -> Result<()> {
    use std::os::unix::process::CommandExt;
    paths::ensure_dirs()?;
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::daemon_log())?;
    let mut command = std::process::Command::new(exe);
    command
        .args(["daemon", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().context("spawning the daemon")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if is_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "the daemon did not come up; see {}",
        paths::daemon_log().display()
    )
}

/// Make sure a daemon is up. Returns true when this call started it.
pub fn ensure_running() -> Result<bool> {
    if is_running() {
        return Ok(false);
    }
    spawn_detached()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_returns_only_what_is_new_and_caps_the_tail() {
        let mut console = Console::new();
        for i in 0..10 {
            console.push(format!("line {i}"));
        }
        assert_eq!(console.last_seq(), 10);
        let tail = console.since(0, 3);
        assert_eq!(
            tail.iter().map(|l| l.seq).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );
        let new = console.since(7, 100);
        assert_eq!(
            new.iter().map(|l| l.seq).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );
        assert!(console.since(10, 100).is_empty());
    }

    #[test]
    fn console_ring_is_bounded() {
        let mut console = Console::new();
        for i in 0..CONSOLE_CAP + 50 {
            console.push(i.to_string());
        }
        assert_eq!(console.lines.len(), CONSOLE_CAP);
        // A client that fell behind the ring gets what is left, not a panic.
        let lines = console.since(3, CONSOLE_CAP);
        assert_eq!(lines.len(), CONSOLE_CAP);
        assert_eq!(lines[0].seq, 51);
    }

    #[test]
    fn a_lock_taken_twice_fails_instead_of_hanging() {
        let mutex = Mutex::new(1);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _first = lock_or_give_up(&mutex, Duration::from_millis(200));
            // The bug pattern behind the old deadlock: same thread, second lock.
            let _second = lock_or_give_up(&mutex, Duration::from_millis(200));
        }));
        assert!(result.is_err(), "re-entrant locking must give up, not hang");
        // Unwinding released the first guard; the lock is usable again.
        assert_eq!(*lock_or_give_up(&mutex, Duration::from_millis(200)), 1);
    }

    #[test]
    fn ansi_colour_is_stripped() {
        assert_eq!(
            strip_ansi("\u{1b}[1;32m[INFO]\u{1b}[0m Server started"),
            "[INFO] Server started"
        );
    }

    #[test]
    fn docker_memory_units_parse() {
        assert_eq!(parse_docker_mem("512KiB"), 512 * 1024);
        assert_eq!(
            parse_docker_mem("45.5MiB "),
            (45.5 * 1024.0 * 1024.0) as u64
        );
        assert_eq!(parse_docker_mem("1GiB"), 1024 * 1024 * 1024);
    }

    #[test]
    fn listing_matches_port_and_name_ignoring_colour_codes() {
        let entry = serde_json::json!({"port": "30814", "sname": "^1My ^rServer"});
        assert!(listing_matches(&entry, 30814, "My Server"));
        assert!(!listing_matches(&entry, 30815, "My Server"));
        let numeric = serde_json::json!({"port": 30814, "sname": "My Server"});
        assert!(listing_matches(&numeric, 30814, "My Server"));
    }
}
