<div align="center">

# beamhost

**Host BeamMP servers from a live TUI, with a daemon that keeps them up after you close it.**

</div>

beamhost runs the official [BeamMP-Server](https://github.com/BeamMP/BeamMP-Server),
so any stock BeamMP client can join. It downloads and checksums the server
binary, writes the server config, supervises the process (restarting it after
crashes), serves your mods, and shows players, console, CPU/RAM and public
server-list status on a dashboard modelled on
[ccli](https://github.com/codingsushi79/ccli).

## Install

One command, no clone needed:

```bash
curl -fsSL https://raw.githubusercontent.com/codingsushi79/bmps/main/install.sh | bash
```

It builds beamhost from this repo with Cargo and installs it as `beamhost` in
`~/.cargo/bin`. It needs Rust; if you don't have it, the script tells you how
to get it.

<details>
<summary>Manual install</summary>

```bash
git clone https://github.com/codingsushi79/bmps
cd bmps
./install.sh            # or: cargo install --path .
```

Environment overrides for the script: `BEAMHOST_REPO`, `BEAMHOST_BRANCH`,
`BEAMHOST_INSTALL_DIR`.
</details>

## Quick start

1. For a **public** server (listed in the in-game browser), get a free auth
   key at **https://keymaster.beammp.com** (Keys → New). A **private** server
   (the default; players join with Direct Connect) runs without one, because
   beamhost gives it a placeholder key. BeamMP-Server only refuses an *empty*
   key, and private servers never register with the server list.
2. Run `beamhost`, press `a`, fill in the form (paste the key, if you have one, into the last
   field) and press Enter.
3. Press `s` to start the server. The first start downloads BeamMP-Server.

From a shell instead:

```bash
beamhost server add freeroam --map west_coast_usa --max-players 10 --public \
  --key 0f8fad5b-d9cb-469f-a165-70867728950e
beamhost start freeroam
beamhost mod add freeroam ~/Downloads/drift_pack.zip
beamhost restart freeroam   # players get new mods after a restart
```

Press `q` to close the dashboard. **The servers keep running.** Run `beamhost`
again to come back.

## Platforms

| Host | How servers run |
|---|---|
| Linux x86_64 / arm64 | natively. The Ubuntu 22.04/24.04 or Debian 12/13 build is picked from `/etc/os-release` |
| macOS (Intel or Apple Silicon) | in Docker. BeamMP only ships Linux builds, so the Linux build runs in a small Debian container that beamhost builds on first start. Needs Docker Desktop, OrbStack or colima |

The runtime is `auto` by default. Set `runtime = "docker"` on a server (or
`--runtime docker`) to containerise it on Linux too. Either way the server
directory is bind-mounted, so mods, config and logs are ordinary files in
`~/.local/share/beamhost/servers/<name>/`.

## Letting people join

- **LAN:** players use Direct Connect with the address the dashboard shows in
  *Join info*.
- **Internet:** forward the server's port (TCP **and** UDP, default 30814) on
  your router to this machine. For a public server (`public = yes`), the
  dashboard checks the BeamMP server list every minute and tells you whether
  the server shows up. If it doesn't, the port forward is usually the problem.

## What it does

- **Releases:** finds the newest stable BeamMP-Server on GitHub, picks the
  right build for the OS, downloads it and checks it against GitHub's sha256
  digest. Several versions can be installed side by side, and a server can
  pin one (`version = "v3.9.3"`).
- **Supervision:** crash detection with exponential backoff (2s → 60s,
  giving up after 5 crashes in a row; a run longer than 5 minutes resets the
  count). SIGTERM with a 10s grace period before SIGKILL. All servers stop in
  parallel on shutdown.
- **Bridge plugin:** beamhost installs a small Lua plugin
  (`Resources/Server/beamhost`) that reports players and vehicles once a
  second and carries out `say`/`kick` from the dashboard. It uses only the
  stock server Lua API and files in the server directory.
- **Mods:** add a `.zip` or a whole folder of them. Zips are checked to be real
  archives first. Disable a mod to stop serving it without deleting it, and
  see the total download size joining players will face.
- **Console:** live console per server, scrollback, and a prompt that sends
  commands to the server's stdin (`status`, `list`, `kick`, `say`, ...).
- **Cheap to run:** a 2-thread tokio runtime, bounded ring buffers (5000
  console lines per server, 500 log lines, 30 min of player history), mods
  re-listed every 3 seconds rather than on every snapshot, one `docker stats`
  call for all containers every 5 seconds, and a TUI that only redraws when
  something changed.

## Keys

| Key | Action |
|---|---|
| `1`–`7`, `Tab` | switch view |
| `↑ ↓` / `j k` | move selection (scroll, in Console and Logs) |
| `[` / `]` | previous / next server, in Mods and Console |
| `a` | add a server (a mod, in Mods) |
| `e` | edit the selected server (enable/disable a mod, in Mods) |
| `K` | set the auth key (kick the selected player, in Players) |
| `d` | remove the selected server or mod |
| `s` / `x` / `R` | start / stop / restart the selected server |
| `S` / `X` | start every server / stop every server |
| `m` | chat message to everyone on the server |
| `:` / `Enter` | console command, in Console |
| `c` | jump to the selected server's console |
| `i` | install a BeamMP-Server release |
| `r` | reload the config file |
| `f` | freeze the display |
| `Q` | shut the daemon down (stops every server) |
| `q` / `Esc` / `Ctrl-C` | close the dashboard, keep hosting |
| `?` | help |

## Commands

```
beamhost                          open the dashboard
beamhost status [--json]          one-shot summary
beamhost start|stop [NAME]        one server, or all of them
beamhost restart NAME
beamhost server add|edit|key|list|rm
beamhost mod add|list|rm|toggle SERVER ...
beamhost players | say SERVER MSG | kick SERVER ID [REASON]
beamhost console SERVER [-f]      print (and follow) the console
beamhost cmd SERVER LINE          send a raw console command
beamhost install [VERSION] | releases | maps
beamhost daemon start|stop [--force]|status|run|log|reload
beamhost config path|show         show masks auth keys
```

## Troubleshooting

**`the daemon didn't answer`.** Run `beamhost daemon stop`. It asks the
daemon to stop, and if the daemon doesn't respond it kills it and stops any
BeamMP servers it left running. `--force` skips the polite request.
`beamhost daemon log` shows what happened.

## Configuration

`~/.config/beamhost/config.toml` (mode 0600, because it holds auth keys). Data
lives in `~/.local/share/beamhost/`. Set `BEAMHOST_HOME` to move both.

```toml
[settings]
default_version = "latest"
runtime = "auto"                 # auto | native | docker
docker_image = "beamhost-runtime:debian12"
check_listing = true

[[server]]
name = "freeroam"
title = "^4Chill ^rFreeroam"     # shown in the server browser
port = 30814
map = "west_coast_usa"           # or "/levels/<map>/info.json" for modded maps
max_players = 10
max_cars = 2
private = false
auth_key = "…"
description = "No rules, just vibes"
tags = "Freeroam"
autostart = true
restart_on_crash = true
```

`ServerConfig.toml` in each server directory is generated from this on every
start, so make edits here (or with `e` in the TUI). Hand edits need
`beamhost daemon reload` (or `r`).

## Design

```
beamhost (TUI/CLI) ──unix socket, JSON lines──▶ beamhost daemon
  thin client, no state                          ├── server "freeroam" ── BeamMP-Server (native)
                                                 │      stdout/stderr → console ring
                                                 │      stdin ← console commands
                                                 │      bridge plugin ⇄ status.json / cmd/*.cmd
                                                 ├── server "drift" ── docker run … BeamMP-Server
                                                 ├── 1s tick: host + process stats, bridge, restarts
                                                 ├── release checker (GitHub, every 6h)
                                                 └── listing checker (backend.beammp.com, every 60s)
```

## Tests

```bash
cargo test
```

The tests cover config validation (port clashes, names, key shape), the
generated `ServerConfig.toml`, release and flavor selection, mod
add/toggle/remove (including path-traversal and fake-zip rejection), bridge
command queueing, the console ring, exit handling helpers, docker command
construction, form building, and an in-memory render of every TUI screen at
several sizes (`BEAMHOST_DUMP=1 cargo test -- --nocapture` prints them).
