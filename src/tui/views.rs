//! The individual screens. Each one renders straight from the daemon snapshot.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Sparkline, Table, TableState, Wrap};

use super::widgets::{kv, meter, panel, spark};
use super::{App, theme};
use crate::model::{
    LogLevel, ServerState, ServerStatus, Snapshot, fmt_bytes, fmt_duration, strip_beam_codes,
    truncate,
};

// ------------------------------------------------------------- dashboard ---

pub fn dashboard(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    let [main, log_area] =
        Layout::vertical([Constraint::Min(10), Constraint::Length(9)]).areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(main);
    let [chart_area, table_area] =
        Layout::vertical([Constraint::Length(8), Constraint::Min(6)]).areas(left);
    let [system_area, release_area, reach_area] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Min(4),
    ])
    .areas(right);

    players_panel(frame, &snapshot, chart_area);
    server_table(frame, &snapshot, app.server_selected, table_area, true);
    system_panel(frame, &snapshot, system_area);
    release_panel(frame, &snapshot, release_area);
    reach_panel(frame, app, &snapshot, reach_area);
    log_panel(frame, &snapshot, 0, log_area);
}

fn players_panel(frame: &mut Frame, snapshot: &Snapshot, area: Rect) {
    let totals = &snapshot.totals;
    let block = panel("Players online");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [headline, chart] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);

    let fill = if totals.slots > 0 {
        totals.players as f64 / totals.slots as f64 * 100.0
    } else {
        0.0
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} / {}", totals.players, totals.slots),
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" players", theme::label()),
            Span::styled(format!("   {fill:.0}% full"), theme::value()),
            Span::styled("   peak ", theme::label()),
            Span::styled(totals.peak_players.to_string(), theme::value()),
            Span::styled("   vehicles ", theme::label()),
            Span::styled(totals.vehicles.to_string(), theme::value()),
        ]),
        Line::from(vec![
            Span::styled("servers ", theme::label()),
            Span::styled(
                format!("{} running", totals.servers_running),
                if totals.servers_running > 0 {
                    theme::good()
                } else {
                    theme::muted()
                },
            ),
            Span::styled(format!(" of {}", totals.servers_total), theme::muted()),
            Span::styled("   last 30 min", theme::muted()),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), headline);

    let data: Vec<u64> = totals
        .history
        .iter()
        .rev()
        .take(chart.width as usize)
        .rev()
        .copied()
        .collect();
    let peak = data.iter().copied().max().unwrap_or(0);
    // Headroom above the peak, and a floor so one player is not a full bar.
    let ceiling = ((peak as f64 * 1.25) as u64).max(4);
    frame.render_widget(
        Sparkline::default()
            .data(&data)
            .max(ceiling)
            .style(Style::default().fg(theme::ACCENT)),
        chart,
    );
}

fn system_panel(frame: &mut Frame, snapshot: &Snapshot, area: Rect) {
    let hardware = &snapshot.hardware;
    let block = panel("Host");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;
    let mem_ratio = ratio(hardware.mem_used, hardware.mem_total);
    let server_cpu: f32 = snapshot.servers.iter().map(|s| s.cpu).sum();
    let server_mem: u64 = snapshot.servers.iter().map(|s| s.mem).sum();
    let lines = vec![
        meter(
            "cpu",
            hardware.cpu_usage as f64 / 100.0,
            &format!("{:.0}%", hardware.cpu_usage),
            width,
            theme::threshold(hardware.cpu_usage as f64, 75.0, 92.0),
        ),
        meter(
            "mem",
            mem_ratio,
            &format!(
                "{} / {}",
                fmt_bytes(hardware.mem_used),
                fmt_bytes(hardware.mem_total)
            ),
            width,
            theme::threshold(mem_ratio * 100.0, 75.0, 90.0),
        ),
        Line::from(""),
        kv(
            "servers",
            format!("{server_cpu:.1}% cpu · {}", fmt_bytes(server_mem)),
            theme::value(),
        ),
        kv(
            "load",
            format!(
                "{:.2}  {:.2}  {:.2}",
                hardware.load_avg[0], hardware.load_avg[1], hardware.load_avg[2]
            ),
            theme::value(),
        ),
        kv("host", truncate(&hardware.host, 30), theme::value()),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn release_panel(frame: &mut Frame, snapshot: &Snapshot, area: Rect) {
    let releases = &snapshot.releases;
    let block = panel("BeamMP-Server");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = Vec::new();
    let newest_installed = releases.installed.first().map(|i| i.tag.clone());
    let latest = releases.latest.clone().unwrap_or_else(|| "?".into());
    let up_to_date = newest_installed.as_deref() == releases.latest.as_deref();
    lines.push(kv(
        "latest",
        latest,
        if up_to_date {
            theme::good()
        } else {
            theme::accent()
        },
    ));
    lines.push(kv(
        "installed",
        if releases.installed.is_empty() {
            "none yet — downloaded on first start".into()
        } else {
            releases
                .installed
                .iter()
                .take(3)
                .map(|i| i.tag.clone())
                .collect::<Vec<_>>()
                .join(", ")
        },
        theme::value(),
    ));
    lines.push(kv(
        "build",
        truncate(&releases.host_flavor, 28),
        theme::muted(),
    ));
    if let Some((tag, done, total)) = &releases.installing {
        lines.push(meter(
            "fetch",
            ratio(*done, *total),
            &format!("{tag} {}", fmt_bytes(*done)),
            inner.width as usize,
            theme::ACCENT,
        ));
    } else if let Some(docker) = releases.docker {
        lines.push(kv(
            "docker",
            if docker { "running" } else { "not running" },
            if docker { theme::good() } else { theme::bad() },
        ));
    } else if let Some(error) = &releases.error {
        lines.push(Line::from(Span::styled(truncate(error, 60), theme::bad())));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// "Can people reach my server?" — the address to share and listing state.
fn reach_panel(frame: &mut Frame, app: &App, snapshot: &Snapshot, area: Rect) {
    let block = panel("Join info");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if snapshot.servers.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("no servers yet", theme::muted())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("press ", theme::muted()),
                    Span::styled("a", theme::accent()),
                    Span::styled(" to create one. Private servers run", theme::muted()),
                ]),
                Line::from(Span::styled(
                    "without a key; public ones need a free key from keymaster.beammp.com.",
                    theme::muted(),
                )),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    let ip = app
        .lan_ip
        .clone()
        .unwrap_or_else(|| "<this machine>".into());
    let mut lines = Vec::new();
    for server in &snapshot.servers {
        let (dot, style, note) = match (server.private, server.listed, server.state) {
            (_, _, state) if state != ServerState::Running => {
                ("○", theme::muted(), state.label().to_string())
            }
            (true, _, _) => ("●", theme::good(), "private · direct connect".into()),
            (false, Some(true), _) => ("●", theme::good(), "listed publicly".into()),
            (false, Some(false), _) => (
                "✗",
                theme::warn(),
                "not on the public list — port forwarded?".into(),
            ),
            (false, None, _) => ("○", theme::accent(), "checking the public list…".into()),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{dot} "), style),
            Span::styled(format!("{:<14}", truncate(&server.name, 14)), theme::base()),
            Span::styled(format!("{ip}:{}", server.port), theme::value()),
        ]));
        lines.push(Line::from(Span::styled(format!("   {note}"), style)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

// --------------------------------------------------------------- servers ---

pub fn servers(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    let [table_area, detail_area] =
        Layout::vertical([Constraint::Percentage(45), Constraint::Min(12)]).areas(area);
    server_table(frame, &snapshot, app.server_selected, table_area, false);
    match snapshot.servers.get(app.server_selected) {
        Some(server) => server_detail(frame, app, server, detail_area),
        None => frame.render_widget(panel("Server"), detail_area),
    }
}

fn server_table(
    frame: &mut Frame,
    snapshot: &Snapshot,
    selected: usize,
    area: Rect,
    compact: bool,
) {
    let block = panel("Servers");
    if snapshot.servers.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("no servers configured", theme::muted())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("press ", theme::muted()),
                    Span::styled("a", theme::accent()),
                    Span::styled(" here, or from a shell:", theme::muted()),
                ]),
                Line::from(Span::styled(
                    "beamhost server add freeroam --map west_coast_usa --key <auth key>",
                    theme::base(),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "the server binary is downloaded the first time it starts",
                    theme::muted(),
                )),
            ])
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

    let header = if compact {
        Row::new(vec!["SERVER", "STATE", "PLAYERS", "MAP", "PORT", "TREND"])
    } else {
        Row::new(vec![
            "SERVER", "STATE", "PLAYERS", "CARS", "MAP", "PORT", "VERSION", "RUNTIME", "CPU",
            "MEM", "UP", "MODS", "VIS",
        ])
    }
    .style(theme::label())
    .height(1);

    let rows: Vec<Row> = snapshot
        .servers
        .iter()
        .map(|s| {
            let state = Cell::from(Line::from(Span::styled(
                s.state.label(),
                Style::default().fg(theme::state_color(s.state)),
            )));
            let players = Cell::from(Line::from(vec![
                Span::styled(
                    s.players.len().to_string(),
                    if s.players.is_empty() {
                        theme::muted()
                    } else {
                        theme::accent()
                    },
                ),
                Span::styled(format!("/{}", s.max_players), theme::muted()),
            ]));
            let name = Cell::from(truncate(&s.name, 18));
            let map = Cell::from(truncate(&s.map, 20));
            if compact {
                Row::new(vec![
                    name,
                    state,
                    players,
                    map,
                    Cell::from(s.port.to_string()),
                    Cell::from(spark(&s.history, 16)),
                ])
            } else {
                let live = s.state.is_live();
                Row::new(vec![
                    name,
                    state,
                    players,
                    Cell::from(s.vehicles().to_string()),
                    map,
                    Cell::from(s.port.to_string()),
                    Cell::from(s.version.clone()),
                    Cell::from(s.runtime.clone()),
                    Cell::from(if live {
                        format!("{:.1}%", s.cpu)
                    } else {
                        "-".into()
                    }),
                    Cell::from(if live && s.mem > 0 {
                        fmt_bytes(s.mem)
                    } else {
                        "-".into()
                    }),
                    Cell::from(if s.uptime_secs > 0 {
                        fmt_duration(s.uptime_secs)
                    } else {
                        "-".into()
                    }),
                    Cell::from(s.mods.iter().filter(|m| m.enabled).count().to_string()),
                    Cell::from(Line::from(if s.private {
                        Span::styled("private", theme::muted())
                    } else {
                        Span::styled("public", theme::accent())
                    })),
                ])
            }
        })
        .collect();

    let widths: Vec<Constraint> = if compact {
        vec![
            Constraint::Length(18),
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Length(20),
            Constraint::Length(6),
            Constraint::Min(8),
        ]
    } else {
        vec![
            Constraint::Length(18),
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(20),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Min(7),
        ]
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(theme::selected())
        .highlight_symbol("");
    let mut state = TableState::default().with_selected(Some(selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn server_detail(frame: &mut Frame, app: &App, server: &ServerStatus, area: Rect) {
    let block = panel(format!("Server · {}", server.name));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("{:<11}", "state"), theme::label()),
            Span::styled(
                server.state.label(),
                Style::default()
                    .fg(theme::state_color(server.state))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                server.pid.map(|p| format!("  pid {p}")).unwrap_or_default(),
                theme::muted(),
            ),
        ]),
        kv(
            "uptime",
            if server.uptime_secs > 0 {
                fmt_duration(server.uptime_secs)
            } else {
                "-".into()
            },
            theme::value(),
        ),
        kv(
            "players",
            format!(
                "{} / {}   ({} car(s) each)",
                server.players.len(),
                server.max_players,
                server.max_cars
            ),
            theme::value(),
        ),
        kv("map", server.map.clone(), theme::value()),
        kv(
            "version",
            format!("{} · {}", server.version, server.runtime),
            theme::value(),
        ),
        kv(
            "auth key",
            match (server.auth_key_set, server.private) {
                (true, _) => "set",
                (false, true) => "none — fine while private (placeholder key)",
                (false, false) => "missing — press K, or make it private (e)",
            },
            match (server.auth_key_set, server.private) {
                (true, _) => theme::good(),
                (false, true) => theme::warn(),
                (false, false) => theme::bad(),
            },
        ),
        kv(
            "bridge",
            if server.telemetry {
                "online"
            } else if server.state == ServerState::Running {
                "not responding"
            } else {
                "-"
            },
            if server.telemetry {
                theme::good()
            } else {
                theme::muted()
            },
        ),
        kv(
            "restarts",
            format!(
                "{}{}",
                server.restarts,
                if server.restart_on_crash {
                    "  (auto-restart on)"
                } else {
                    "  (auto-restart off)"
                }
            ),
            theme::value(),
        ),
    ];
    if let Some(exit) = &server.last_exit {
        lines.push(Line::from(vec![
            Span::styled(format!("{:<11}", "last exit"), theme::label()),
            Span::styled(
                exit.clone(),
                if server.state == ServerState::Crashed || !server.auth_key_set {
                    theme::bad()
                } else {
                    theme::warn()
                },
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), left);

    let ip = app
        .lan_ip
        .clone()
        .unwrap_or_else(|| "<this machine>".into());
    let mut right_lines = vec![
        kv(
            "visibility",
            if server.private {
                "private (direct connect only)".to_string()
            } else {
                match server.listed {
                    Some(true) => "public · on the server list".into(),
                    Some(false) => "public · NOT on the list yet".into(),
                    None => "public".into(),
                }
            },
            theme::value(),
        ),
        kv("LAN", format!("{ip}:{}", server.port), theme::accent()),
        kv(
            "internet",
            format!("forward TCP+UDP {} to {ip}", server.port),
            theme::muted(),
        ),
        kv("tags", truncate(&server.tags, 40), theme::value()),
        kv(
            "about",
            truncate(&strip_beam_codes(&server.description), 40),
            theme::value(),
        ),
        kv(
            "mods",
            format!(
                "{} served, {} total · {}",
                server.mods.iter().filter(|m| m.enabled).count(),
                server.mods.len(),
                fmt_bytes(
                    server
                        .mods
                        .iter()
                        .filter(|m| m.enabled)
                        .map(|m| m.bytes)
                        .sum()
                )
            ),
            theme::value(),
        ),
        kv("plugins", server.plugins.join(", "), theme::value()),
        kv(
            "autostart",
            if server.autostart { "yes" } else { "no" },
            theme::value(),
        ),
        Line::from(""),
        Line::from(Span::styled("players over the last 30 min", theme::label())),
        spark(&server.history, right.width.saturating_sub(1) as usize),
    ];
    if !server.auth_key_set && !server.private {
        right_lines.insert(
            0,
            Line::from(Span::styled(
                "⚠ public servers need an auth key; private ones run without",
                theme::warn(),
            )),
        );
    }
    frame.render_widget(Paragraph::new(right_lines), right);
}

// --------------------------------------------------------------- players ---

pub fn players(frame: &mut Frame, app: &mut App, area: Rect) {
    let players = app.all_players();
    let block = panel(format!("Players · {} online", players.len()));
    if players.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let running = app
            .snapshot()
            .map(|s| s.totals.servers_running)
            .unwrap_or(0);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("nobody online", theme::muted())),
                Line::from(""),
                Line::from(Span::styled(
                    if running == 0 {
                        "no server is running — start one with s on the Servers tab"
                    } else {
                        "players show up here within a second of joining"
                    },
                    theme::muted(),
                )),
            ]),
            inner,
        );
        return;
    }
    let header = Row::new(vec!["SERVER", "ID", "NAME", "VEHICLES", "ACCOUNT"])
        .style(theme::label())
        .height(1);
    let rows: Vec<Row> = players
        .iter()
        .map(|(server, p)| {
            Row::new(vec![
                Cell::from(Line::from(Span::styled(
                    truncate(server, 16),
                    theme::muted(),
                ))),
                Cell::from(p.id.to_string()),
                Cell::from(Line::from(Span::styled(
                    truncate(&strip_beam_codes(&p.name), 28),
                    theme::value(),
                ))),
                Cell::from(p.vehicles.to_string()),
                Cell::from(Line::from(if p.guest {
                    Span::styled("guest", theme::warn())
                } else {
                    Span::styled("beammp", theme::good())
                })),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(17),
            Constraint::Length(5),
            Constraint::Length(29),
            Constraint::Length(9),
            Constraint::Min(8),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(theme::selected())
    .highlight_symbol("");
    let mut state = TableState::default().with_selected(Some(app.player_selected));
    frame.render_stateful_widget(table, area, &mut state);
}

// ------------------------------------------------------------------ mods ---

pub fn mods(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(server) = app.selected_server().cloned() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "no servers yet — add one first",
                theme::muted(),
            ))
            .block(panel("Mods")),
            area,
        );
        return;
    };
    let [list_area, side] =
        Layout::horizontal([Constraint::Percentage(65), Constraint::Percentage(35)]).areas(area);
    let served: u64 = server
        .mods
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.bytes)
        .sum();
    let block = panel(format!(
        "Client mods · {}  ([ ] to switch server)",
        server.name
    ));
    if server.mods.is_empty() {
        let inner = block.inner(list_area);
        frame.render_widget(block, list_area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("no mods on this server", theme::muted())),
                Line::from(""),
                Line::from(vec![
                    Span::styled("press ", theme::muted()),
                    Span::styled("a", theme::accent()),
                    Span::styled(
                        " and give a .zip path (or a folder of zips)",
                        theme::muted(),
                    ),
                ]),
                Line::from(Span::styled(
                    "players download these automatically when they join",
                    theme::muted(),
                )),
            ]),
            inner,
        );
    } else {
        let header = Row::new(vec!["", "MOD", "SIZE"]).style(theme::label());
        let rows: Vec<Row> = server
            .mods
            .iter()
            .map(|m| {
                Row::new(vec![
                    Cell::from(Line::from(if m.enabled {
                        Span::styled("●", theme::good())
                    } else {
                        Span::styled("○", theme::muted())
                    })),
                    Cell::from(Line::from(Span::styled(
                        m.name.clone(),
                        if m.enabled {
                            theme::value()
                        } else {
                            theme::muted()
                        },
                    ))),
                    Cell::from(fmt_bytes(m.bytes)),
                ])
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(2),
                Constraint::Min(20),
                Constraint::Length(11),
            ],
        )
        .header(header)
        .block(block)
        .row_highlight_style(theme::selected())
        .highlight_symbol("");
        let mut state = TableState::default().with_selected(Some(app.mod_selected));
        frame.render_stateful_widget(table, list_area, &mut state);
    }

    let [summary_area, plugin_area] =
        Layout::vertical([Constraint::Length(9), Constraint::Min(4)]).areas(side);
    let summary = vec![
        kv(
            "served",
            format!(
                "{} mod(s) · {}",
                server.mods.iter().filter(|m| m.enabled).count(),
                fmt_bytes(served)
            ),
            theme::value(),
        ),
        kv(
            "disabled",
            server
                .mods
                .iter()
                .filter(|m| !m.enabled)
                .count()
                .to_string(),
            theme::value(),
        ),
        Line::from(""),
        Line::from(Span::styled(
            "Every joining player downloads all served mods. Big packs mean slow joins — \
             disable what you are not using.",
            theme::muted(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Changes reach players after a restart (R).",
            theme::muted(),
        )),
    ];
    frame.render_widget(
        Paragraph::new(summary)
            .wrap(Wrap { trim: false })
            .block(panel("Download size")),
        summary_area,
    );
    let mut plugin_lines: Vec<Line> = server
        .plugins
        .iter()
        .map(|p| {
            Line::from(vec![
                Span::styled("● ", theme::good()),
                Span::styled(p.clone(), theme::value()),
                Span::styled(
                    if p == crate::instance::BRIDGE_NAME {
                        "  (beamhost bridge)"
                    } else {
                        ""
                    },
                    theme::muted(),
                ),
            ])
        })
        .collect();
    plugin_lines.push(Line::from(""));
    plugin_lines.push(Line::from(Span::styled(
        "Lua plugins: drop folders into Resources/Server/",
        theme::muted(),
    )));
    frame.render_widget(
        Paragraph::new(plugin_lines)
            .wrap(Wrap { trim: false })
            .block(panel("Server plugins")),
        plugin_area,
    );
}

// --------------------------------------------------------------- console ---

pub fn console(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(server) = app.selected_server().cloned() else {
        frame.render_widget(
            Paragraph::new(Span::styled("no servers yet", theme::muted())).block(panel("Console")),
            area,
        );
        return;
    };
    let scroll = app.console_scroll;
    let title = if scroll > 0 {
        format!(
            "Console · {} · {}  (scrolled back {scroll} · End for latest)",
            server.name,
            server.state.label()
        )
    } else {
        format!(
            "Console · {} · {}  ([ ] server · : command)",
            server.name,
            server.state.label()
        )
    };
    let block = panel(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.console.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "nothing yet — output appears here once the server starts",
                theme::muted(),
            )),
            inner,
        );
        return;
    }
    let height = inner.height as usize;
    let end = app.console.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let lines: Vec<Line> = app.console[start..end]
        .iter()
        .map(|line| {
            let style = console_style(&line.text);
            Line::from(vec![
                Span::styled(format!("{} ", line.at), theme::muted()),
                Span::styled(line.text.clone(), style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn console_style(text: &str) -> Style {
    if text.contains("[ERROR]") || text.contains("[FATAL]") {
        theme::bad()
    } else if text.contains("[WARN]") {
        theme::warn()
    } else if text.contains("[CHAT]") {
        theme::accent()
    } else if text.contains("[beamhost]") || text.starts_with("──") || text.starts_with("> ") {
        Style::default().fg(theme::HEADER)
    } else if text.contains("[DEBUG]") {
        theme::muted()
    } else {
        theme::base()
    }
}

// ---------------------------------------------------------------- system ---

pub fn system(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    let hardware = &snapshot.hardware;
    let core_rows = (hardware.per_core.len() as u16).div_ceil(3).clamp(1, 16) + 2;
    let [top, cores_area, procs_area] = Layout::vertical([
        Constraint::Length(9),
        Constraint::Length(core_rows),
        Constraint::Min(5),
    ])
    .areas(area);
    let [machine, daemon_area] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(top);

    let block = panel("Machine");
    let inner = block.inner(machine);
    frame.render_widget(block, machine);
    let width = inner.width as usize;
    let mem_ratio = ratio(hardware.mem_used, hardware.mem_total);
    let lines = vec![
        meter(
            "cpu",
            hardware.cpu_usage as f64 / 100.0,
            &format!("{:.0}%", hardware.cpu_usage),
            width,
            theme::threshold(hardware.cpu_usage as f64, 75.0, 92.0),
        ),
        meter(
            "mem",
            mem_ratio,
            &format!(
                "{} / {}",
                fmt_bytes(hardware.mem_used),
                fmt_bytes(hardware.mem_total)
            ),
            width,
            theme::threshold(mem_ratio * 100.0, 75.0, 90.0),
        ),
        kv("cpu", truncate(&hardware.cpu_brand, 40), theme::value()),
        kv("cores", hardware.cores.to_string(), theme::value()),
        kv(
            "load",
            format!(
                "{:.2}  {:.2}  {:.2}",
                hardware.load_avg[0], hardware.load_avg[1], hardware.load_avg[2]
            ),
            theme::value(),
        ),
        kv("os", truncate(&hardware.os, 40), theme::value()),
        kv("host", truncate(&hardware.host, 40), theme::value()),
    ];
    frame.render_widget(Paragraph::new(lines), inner);

    let daemon = &snapshot.daemon;
    let daemon_lines = vec![
        kv("version", daemon.version.clone(), theme::value()),
        kv("pid", daemon.pid.to_string(), theme::value()),
        kv("uptime", fmt_duration(daemon.uptime_secs), theme::value()),
        kv("config", truncate(&daemon.config_path, 40), theme::muted()),
        kv(
            "binaries",
            snapshot
                .releases
                .installed
                .iter()
                .map(|i| format!("{} {}", i.tag, i.flavor))
                .collect::<Vec<_>>()
                .join(", "),
            theme::value(),
        ),
    ];
    frame.render_widget(
        Paragraph::new(daemon_lines)
            .wrap(Wrap { trim: false })
            .block(panel("Daemon")),
        daemon_area,
    );

    core_grid(frame, &hardware.per_core, cores_area);

    let header = Row::new(vec![
        "SERVER", "STATE", "PID", "CPU", "MEMORY", "PLAYERS", "UP",
    ])
    .style(theme::label());
    let rows: Vec<Row> = snapshot
        .servers
        .iter()
        .map(|s| {
            Row::new(vec![
                Cell::from(truncate(&s.name, 18)),
                Cell::from(Line::from(Span::styled(
                    s.state.label(),
                    Style::default().fg(theme::state_color(s.state)),
                ))),
                Cell::from(s.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())),
                Cell::from(format!("{:.1}%", s.cpu)),
                Cell::from(if s.mem > 0 {
                    fmt_bytes(s.mem)
                } else {
                    "-".into()
                }),
                Cell::from(format!("{}/{}", s.players.len(), s.max_players)),
                Cell::from(if s.uptime_secs > 0 {
                    fmt_duration(s.uptime_secs)
                } else {
                    "-".into()
                }),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Min(8),
        ],
    )
    .header(header)
    .block(panel("Server processes"));
    frame.render_widget(table, procs_area);
}

fn core_grid(frame: &mut Frame, per_core: &[f32], area: Rect) {
    let block = panel("Cores");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if per_core.is_empty() || inner.height == 0 {
        return;
    }
    // Fit as many columns of meters as the panel is wide.
    let column_width = 26u16;
    let columns = (inner.width / column_width).max(1) as usize;
    let capacity = columns * inner.height as usize;
    let cores = &per_core[..per_core.len().min(capacity)];
    let per_column = cores.len().div_ceil(columns);
    let constraints: Vec<Constraint> = (0..columns)
        .map(|_| Constraint::Length(column_width))
        .collect();
    let areas = Layout::horizontal(constraints).split(inner);
    for (column, target) in areas.iter().enumerate() {
        let start = column * per_column;
        if start >= cores.len() {
            break;
        }
        let end = (start + per_column).min(cores.len());
        let lines: Vec<Line> = cores[start..end]
            .iter()
            .enumerate()
            .map(|(offset, usage)| {
                meter(
                    &format!("c{:<2}", start + offset),
                    *usage as f64 / 100.0,
                    &format!("{usage:>3.0}%"),
                    target.width.saturating_sub(2) as usize,
                    theme::threshold(*usage as f64, 75.0, 93.0),
                )
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), *target);
    }
}

// ------------------------------------------------------------------ logs ---

pub fn logs(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(snapshot) = app.snapshot().cloned() else {
        return;
    };
    log_panel(frame, &snapshot, app.log_scroll, area);
}

fn log_panel(frame: &mut Frame, snapshot: &Snapshot, scroll: usize, area: Rect) {
    let title = if scroll > 0 {
        format!("Log  (scrolled back {scroll} lines · End for latest)")
    } else {
        "Log".to_string()
    };
    let block = panel(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    let total = snapshot.logs.len();
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let lines: Vec<Line> = snapshot.logs[start..end]
        .iter()
        .map(|entry| {
            let (label, style) = match entry.level {
                LogLevel::Error => ("err ", theme::bad()),
                LogLevel::Warn => ("warn", theme::warn()),
                LogLevel::Info => ("info", theme::base()),
            };
            let time = entry.at.split(' ').nth(1).unwrap_or(&entry.at);
            Line::from(vec![
                Span::styled(format!("{time} "), theme::muted()),
                Span::styled(format!("{label} "), style),
                Span::styled(entry.message.clone(), style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn ratio(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        used as f64 / total as f64
    }
}
