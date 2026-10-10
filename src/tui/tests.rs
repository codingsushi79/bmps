//! Render every screen into an in-memory terminal from a made-up snapshot.
//! Catches layout panics (slicing, zero-size areas) without a daemon, and
//! `BEAMHOST_DUMP=1 cargo test` prints the frames for a look.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::model::*;

fn sample() -> Snapshot {
    let server = |name: &str, state: ServerState, players: usize| ServerStatus {
        name: name.into(),
        state,
        port: 30814,
        map: "west_coast_usa".into(),
        version: "v3.9.3".into(),
        runtime: "native".into(),
        private: name != "public",
        auth_key_set: name != "nokey",
        pid: Some(4242),
        uptime_secs: 3723,
        players: (0..players)
            .map(|i| Player {
                id: i as i64,
                name: format!("^1driver{i}"),
                vehicles: (i % 3) as u32,
                guest: i % 4 == 0,
            })
            .collect(),
        max_players: 8,
        max_cars: 2,
        telemetry: true,
        cpu: 12.5,
        mem: 180 * 1024 * 1024,
        restarts: 1,
        last_exit: Some("exit code 1".into()),
        listed: Some(true),
        mods: vec![
            ModFile {
                name: "drift_pack.zip".into(),
                bytes: 52_000_000,
                enabled: true,
            },
            ModFile {
                name: "old_map.zip".into(),
                bytes: 900_000_000,
                enabled: false,
            },
        ],
        plugins: vec!["beamhost".into(), "race".into()],
        history: (0..80).map(|i| (i % 7) as u64).collect(),
        autostart: true,
        restart_on_crash: true,
        description: "^4Chill ^rcruising".into(),
        tags: "Freeroam,Drift".into(),
        console_seq: 3,
        phase: (state == ServerState::Preparing).then(|| "building the server container".into()),
        failed: name == "nokey",
    };
    Snapshot {
        daemon: DaemonInfo {
            pid: 1,
            uptime_secs: 99_000,
            version: "0.1.0".into(),
            config_path: "/home/x/.config/beamhost/config.toml".into(),
        },
        servers: vec![
            server("freeroam", ServerState::Running, 5),
            server("public", ServerState::Starting, 0),
            server("nokey", ServerState::Stopped, 0),
            server("fresh", ServerState::Preparing, 0),
        ],
        totals: Totals {
            servers_total: 3,
            servers_running: 1,
            players: 5,
            slots: 16,
            vehicles: 4,
            history: (0..200).map(|i| (i / 20) as u64).collect(),
            peak_players: 9,
        },
        hardware: Hardware {
            cpu_brand: "Test CPU".into(),
            cores: 8,
            cpu_usage: 37.0,
            per_core: vec![10.0, 95.0, 50.0, 0.0, 33.0, 80.0, 5.0, 60.0],
            mem_used: 6 << 30,
            mem_total: 16 << 30,
            load_avg: [0.5, 0.7, 0.9],
            os: "Linux 24.04 Ubuntu".into(),
            host: "box".into(),
        },
        releases: Releases {
            installed: vec![Installed {
                tag: "v3.9.3".into(),
                flavor: "ubuntu.24.04.x86_64".into(),
                bytes: 12_000_000,
            }],
            latest: Some("v3.9.3".into()),
            checked_secs_ago: Some(5),
            installing: Some(("v3.9.4".into(), 3_000_000, 12_000_000)),
            error: None,
            host_flavor: "ubuntu.24.04.x86_64".into(),
            docker: None,
        },
        logs: (0..30)
            .map(|i| LogEntry {
                at: format!("2026-10-07 12:00:{i:02}"),
                level: [LogLevel::Info, LogLevel::Warn, LogLevel::Error][i % 3],
                message: format!("event number {i}"),
            })
            .collect(),
    }
}

fn app() -> App {
    let mut app = App::new();
    app.snapshot = Some(sample());
    app.console = (1..=50)
        .map(|seq| ConsoleLine {
            seq,
            at: "12:00:00".into(),
            text: format!("[INFO] line {seq} [CHAT] hi"),
        })
        .collect();
    app.lan_ip = Some("192.168.1.20".into());
    app
}

fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn every_tab_renders_at_common_and_tiny_sizes() {
    let dump = std::env::var_os("BEAMHOST_DUMP").is_some();
    for tab in Tab::ALL {
        for (w, h) in [(160, 48), (100, 30), (40, 12), (1, 1)] {
            let mut app = app();
            app.tab = tab;
            let frame = render(&mut app, w, h);
            if dump && w == 160 {
                println!("===== {tab:?} =====\n{frame}");
            }
        }
    }
}

#[test]
fn overlays_render_over_every_tab() {
    let mut app = app();
    app.help = true;
    render(&mut app, 120, 40);
    app.help = false;
    app.form = Some(Form::add_server(30815));
    let frame = render(&mut app, 120, 40);
    assert!(frame.contains("New BeamMP server"));
    if std::env::var_os("BEAMHOST_DUMP").is_some() {
        println!("{frame}");
    }
}

#[test]
fn an_empty_daemon_shows_how_to_begin() {
    let mut app = App::new();
    app.snapshot = Some(Snapshot::default());
    for tab in Tab::ALL {
        app.tab = tab;
        render(&mut app, 120, 40);
    }
    app.tab = Tab::Servers;
    assert!(render(&mut app, 120, 40).contains("no servers configured"));
}

#[test]
fn players_are_listed_across_servers_and_selection_clamps() {
    let mut app = app();
    assert_eq!(app.all_players().len(), 5);
    app.player_selected = 99;
    app.clamp_selection();
    assert_eq!(app.player_selected, 4);
}

#[test]
fn a_server_being_prepared_shows_what_it_is_doing() {
    let mut app = app();
    app.tab = Tab::Servers;
    app.server_selected = 3;
    let frame = render(&mut app, 160, 48);
    assert!(frame.contains("building the server container…"), "{frame}");
    app.server_selected = 2;
    assert!(render(&mut app, 160, 48).contains("failed"));
}
