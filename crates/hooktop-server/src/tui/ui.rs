//! Drawing. Everything here reads [`App`] and paints a frame; no state
//! changes except the scroll offsets ratatui keeps in the list states.

use hooktop_core::view::{self, Member, Role, Row, RowKind};

use hooktop_core::{Sample, SessionGraph, SessionStatus, SessionSummary, Status, Task, ToolCall};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Cell, Chart, Clear, Dataset, GraphType, List, ListItem, Padding, Paragraph, Row as TRow,
    Table, TableState, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{App, Confirm, Focus, Mode, View};
use super::dag;
use super::drill::Panel;
use super::theme::Theme;
use hooktop_core::pricing::{format_tokens, format_usd};

pub const BOSS_ICON: &str = "🦊";
pub const FOLLOWER_ICON: &str = "♞";

/// Space between the window's edges and hooktop's panes.
const WINDOW: Margin = Margin {
    horizontal: 1,
    vertical: 0,
};
/// Columns between panes that sit side by side.
const GUTTER: u16 = 1;

pub fn draw(f: &mut Frame, app: &mut App, now: i64) {
    let t = app.theme.theme();
    let [top, body, foot] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)])
        .areas(f.area().inner(WINDOW));

    title_bar(f, app, &t, top);
    footer(f, app, &t, foot);

    let main = if app.show_sidebar && body.width >= 60 {
        let side_w = (body.width / 5).clamp(24, 34);
        let [side, main] = Layout::horizontal([Constraint::Length(side_w), Constraint::Fill(1)])
            .spacing(GUTTER)
            .areas(body);
        connections(f, app, &t, side, now);
        main
    } else {
        body
    };

    if app.graph.is_none() {
        empty_main(f, app, &t, main);
    } else {
        session(f, app, &t, main, now);
    }

    if app.view == View::Stats && app.drill_open {
        drill_overlay(f, app, &t, now);
    }
    if app.help {
        help(f, app, &t);
    }
}

// ── Chrome ─────────────────────────────────────────────────────────────

fn title_bar(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let mut left: Vec<Span> = vec![
        Span::raw(BOSS_ICON),
        Span::styled(" hooktop", Style::new().fg(t.accent).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
    ];
    for (i, (v, name)) in View::ALL.into_iter().map(|v| (v, v.label())).enumerate() {
        if i > 0 {
            left.push(Span::styled(" │ ", t.faint()));
        }
        left.push(if app.view == v {
            Span::styled(format!(" {name} "), t.selected().fg(t.accent))
        } else {
            Span::styled(format!(" {name} "), t.muted())
        });
    }
    // With the sidebar hidden, its count is the one thing worth keeping.
    if !app.show_sidebar {
        left.push(Span::styled("  ·  ", t.faint()));
        left.push(Span::styled(format!("{} open", app.open_count()), t.muted()));
    }

    // The right side speaks up only when something needs attention; the
    // port, mode and version live in the help (`?`).
    let right = if app.connected {
        Line::styled("● live", Style::new().fg(t.ok))
    } else {
        Line::styled("○ reconnecting…", Style::new().fg(t.err))
    }
    .alignment(Alignment::Right);
    let rw = right.width() as u16;
    let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(rw)]).areas(area);
    f.render_widget(Line::from(left), l);
    f.render_widget(right, r);
}

fn footer(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    if let Some(c) = &app.confirm {
        let q = match c {
            Confirm::Remove { title, .. } => format!(" Remove “{}” and its history? ", ellipsize(title, 60)),
            Confirm::RemoveStale { count } => format!(
                " Remove {count} connection{} (closed, or quiet for over an hour) and their history? ",
                plural(*count)
            ),
        };
        f.render_widget(
            Line::from(vec![
                Span::styled(q, Style::new().fg(t.err).add_modifier(Modifier::BOLD)),
                Span::styled("y", Style::new().fg(t.accent).add_modifier(Modifier::BOLD)),
                Span::styled(" remove  ", t.muted()),
                Span::styled("any other key", Style::new().fg(t.accent)),
                Span::styled(" keep", t.muted()),
            ]),
            area,
        );
        return;
    }
    if let Some(msg) = app.flash() {
        f.render_widget(
            Line::from(Span::styled(format!(" {msg}"), Style::new().fg(t.accent))),
            area,
        );
        return;
    }
    if let Some(w) = &app.hook_warning {
        f.render_widget(
            Line::from(vec![
                Span::styled(format!(" ⚠ {w}"), Style::new().fg(t.err).add_modifier(Modifier::BOLD)),
                Span::styled("   ? ", Style::new().fg(t.accent)),
                Span::styled("help  ", t.muted()),
                Span::styled("q ", Style::new().fg(t.accent)),
                Span::styled("quit", t.muted()),
            ]),
            area,
        );
        return;
    }
    if app.view == View::Stats {
        let keys: &[(&str, &str)] = if app.drill_open {
            &[("↑↓", "pick"), ("⏎", "show in Timeline"), ("esc", "close")]
        } else {
            &[
                ("q", "quit"),
                ("tab ←→", "panel"),
                ("↑↓", "row"),
                ("⏎", "what's behind it"),
                ("o", "next tab"),
                ("?", "help"),
            ]
        };
        let mut spans = Vec::new();
        for (k, v) in keys {
            spans.push(Span::styled(format!(" {k}"), Style::new().fg(t.accent)));
            spans.push(Span::styled(format!(" {v} "), t.muted()));
        }
        f.render_widget(Line::from(spans), area);
        return;
    }
    let mut keys: Vec<(&str, &str)> = vec![("q", "quit"), ("tab", "pane"), ("↑↓", "move")];
    match app.focus {
        Focus::History if app.view == View::Graph => {
            keys.push(("←→", "walk"));
            keys.push(("⏎", "subtasks"));
        }
        Focus::History => {
            keys.push(("←→", "fold"));
            keys.push(("⏎", "open"));
        }
        Focus::Party => keys.push(("⏎", "view")),
        Focus::Connections => keys.push(("⏎", "view")),
    }
    if app.agent.is_some() {
        keys.push(("esc", "back to Watcher"));
    }
    keys.extend([
        ("d", "details"),
        (
            "n",
            if app.hide_notices {
                "show notices"
            } else {
                "hide notices"
            },
        ),
        ("f", if app.follow { "unfollow" } else { "follow" }),
        ("o", "next tab"),
        ("x", "remove"),
        ("t", "theme"),
        ("?", "help"),
    ]);
    let mut spans = vec![Span::raw(" ")];
    for (i, (k, v)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", t.faint()));
        }
        spans.push(Span::styled(*k, Style::new().fg(t.accent)));
        spans.push(Span::styled(format!(" {v}"), t.muted()));
    }
    f.render_widget(Line::from(spans), area);
}

fn panel<'a>(t: &Theme, title: &'a str, focused: bool) -> Block<'a> {
    let title_style = if focused {
        Style::new().fg(t.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(t.ink).add_modifier(Modifier::BOLD)
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(t.border(focused))
        .padding(Padding::horizontal(1))
        .title(Span::styled(format!(" {title} "), title_style))
}

// ── Connections ────────────────────────────────────────────────────────

fn conn_dot(t: &Theme, s: SessionStatus) -> Span<'static> {
    match s {
        SessionStatus::Running => Span::styled("●", Style::new().fg(t.accent)),
        SessionStatus::Idle => Span::styled("◐", Style::new().fg(t.ok)),
        SessionStatus::Ended => Span::styled("○", t.faint()),
    }
}

fn connections(f: &mut Frame, app: &mut App, t: &Theme, area: Rect, now: i64) {
    let focused = app.focus == Focus::Connections;
    let title = format!("Connections · {} open", app.open_count());
    let block = panel(t, &title, focused);
    let inner_w = area.width.saturating_sub(4) as usize;

    if app.sessions.is_empty() {
        let msg = Paragraph::new(vec![
            Line::styled("No connections yet.", t.muted()),
            Line::raw(""),
            Line::styled("Start Claude Code in any", t.muted()),
            Line::styled("terminal.", t.muted()),
        ])
        .wrap(Wrap { trim: true })
        .block(block);
        f.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = app.sessions.iter().map(|s| conn_item(t, s, inner_w, now)).collect();
    let list = List::new(items).block(block).highlight_style(t.selected());
    f.render_stateful_widget(list, area, &mut app.conn_state);
}

fn conn_item<'a>(t: &Theme, s: &SessionSummary, width: usize, now: i64) -> ListItem<'a> {
    let title = ellipsize(&s.title, width.saturating_sub(2));
    let status = match s.status {
        SessionStatus::Running => Style::new().fg(t.accent),
        SessionStatus::Idle => Style::new().fg(t.ok),
        SessionStatus::Ended => t.faint(),
    };
    let ago = ago(now - s.updated_ms);
    let meta = format!("{} · {}", view::connection_text(s.status), ago);
    ListItem::new(vec![
        Line::from(vec![conn_dot(t, s.status), Span::raw(" "), Span::raw(title)]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(ellipsize(&meta, width.saturating_sub(2)), status),
        ]),
    ])
}

// ── Main area ──────────────────────────────────────────────────────────

fn empty_main(f: &mut Frame, app: &App, t: &Theme, area: Rect) {
    let block = panel(t, "Watcher's party", app.focus == Focus::History);
    let lines = if !app.connected {
        vec![
            Line::styled(format!("Connecting to hooktop on :{}…", app.port), t.muted()),
            Line::raw(""),
            Line::styled(app.link_error.clone().unwrap_or_default(), t.faint()),
        ]
    } else if app.sessions.is_empty() {
        let code = Style::new().fg(t.accent);
        vec![
            Line::from(vec![
                Span::raw(BOSS_ICON),
                Span::styled(
                    " Watcher is waiting for work.",
                    Style::new().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::raw(""),
            Line::styled("Point Claude Code at hooktop once:", t.muted()),
            Line::styled("    hooktop hooks install", code),
            Line::raw(""),
            Line::styled("then start claude in any terminal: each session shows up", t.muted()),
            Line::styled("here as a connection. Just looking around? Try", t.muted()),
            Line::styled("    hooktop --demo", code),
        ]
    } else {
        vec![Line::styled("Loading…", t.muted())]
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    let h = lines.len() as u16;
    let [_, mid, _] = Layout::vertical([Constraint::Fill(1), Constraint::Length(h), Constraint::Fill(2)]).areas(inner);
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
        mid,
    );
}

fn session(f: &mut Frame, app: &mut App, t: &Theme, area: Rect, now: i64) {
    let members = app.members();
    let [head, rest] = Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);
    header(f, app, t, head, &members, now);

    let rest = match app.view {
        View::Stats => {
            stats_pane(f, app, t, rest, now);
            return;
        }
        View::Graph => rest,
        View::Timeline => {
            let metrics_h = match rest.height {
                0..22 => 0,
                22..34 => 8,
                _ => 10,
            };
            let party_h = (members.len() as u16 + 2).clamp(3, (rest.height / 4).max(4));
            let [m, p, r] = Layout::vertical([
                Constraint::Length(metrics_h),
                Constraint::Length(party_h),
                Constraint::Fill(1),
            ])
            .areas(rest);
            if metrics_h > 0 {
                metrics(f, app, t, m, now);
            }
            party(f, app, t, p, &members, now);
            r
        }
    };

    let main = |f: &mut Frame, app: &mut App, a: Rect| match app.view {
        View::Timeline => history(f, app, t, a, now),
        View::Graph => graph_pane(f, app, t, a, now),
        View::Stats => {}
    };
    if !app.show_details {
        main(f, app, rest);
        return;
    }
    if rest.width >= 120 {
        let dw = (rest.width / 3).clamp(36, 64);
        let [h, d] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(dw)])
            .spacing(GUTTER)
            .areas(rest);
        main(f, app, h);
        details(f, app, t, d, now);
    } else {
        let dh = (rest.height * 2 / 5).max(6);
        let [h, d] = Layout::vertical([Constraint::Fill(1), Constraint::Length(dh)]).areas(rest);
        main(f, app, h);
        details(f, app, t, d, now);
    }
}

// ── Metrics: CPU, RAM, tool mix ────────────────────────────────────────

/// The session's time span, ending now while it's still open.
fn session_span(g: &SessionGraph, now: i64) -> (i64, i64) {
    let end = if g.status == SessionStatus::Ended {
        g.updated_ms
    } else {
        now.max(g.updated_ms)
    };
    (g.started_ms, end.max(g.started_ms + 1))
}

fn metrics(f: &mut Frame, app: &App, t: &Theme, area: Rect, now: i64) {
    let Some(g) = &app.graph else { return };
    let [cpu_a, ram_a] = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)])
        .spacing(GUTTER)
        .areas(area);

    if g.samples.is_empty() {
        let both = Rect {
            width: ram_a.x + ram_a.width - cpu_a.x,
            ..cpu_a
        };
        let lines = vec![
            Line::styled("No CPU or RAM readings for this session.", t.muted()),
            Line::styled(
                "hooktop records them once a second while Claude works, for sessions it sees start live.",
                t.faint(),
            ),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(panel(t, "CPU · RAM", false)),
            both,
        );
        return;
    }

    let (start, end) = session_span(g, now);
    let span_s = (end - start) as f64 / 1000.0;
    let sel = app.selected_row().map(|r| {
        let e = r
            .ended_ms
            .unwrap_or(if r.status == Status::Running { now } else { r.started_ms });
        ((r.started_ms - start) as f64 / 1000.0, (e - start) as f64 / 1000.0)
    });
    let x = |s: &Sample| (s.at_ms - start) as f64 / 1000.0;

    // CPU
    let cpu: Vec<(f64, f64)> = g.samples.iter().map(|s| (x(s), s.cpu as f64)).collect();
    let peak = g.samples.iter().max_by(|a, b| a.cpu.total_cmp(&b.cpu)).unwrap();
    let cur = g.samples.last().unwrap();
    let cpu_max = ((peak.cpu as f64 * 1.15 / 100.0).ceil() * 100.0).max(100.0);
    let mut title = vec![
        Span::styled(" CPU ", Style::new().fg(t.ink).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{} ", view::cores(cur.cpu)),
            Style::new().fg(t.exec).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("· peak {}", view::cores(peak.cpu)), t.muted()),
    ];
    if let Some(top) = &peak.top {
        title.push(Span::styled(format!(" {top}"), Style::new().fg(t.accent)));
    }
    title.push(Span::raw(" "));
    chart(
        f,
        t,
        cpu_a,
        Line::from(title),
        &cpu,
        t.exec,
        span_s,
        cpu_max,
        ["0".into(), view::cores(cpu_max as f32)],
        sel,
    );

    // RAM
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    let ram: Vec<(f64, f64)> = g.samples.iter().map(|s| (x(s), mb(s.rss))).collect();
    let peak = g.samples.iter().map(|s| s.rss).max().unwrap_or(0);
    let ram_max = (mb(peak) * 1.2).max(64.0);
    let title = Line::from(vec![
        Span::styled(" RAM ", Style::new().fg(t.ink).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{} ", bytes(cur.rss)),
            Style::new().fg(t.read).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("· peak {} · {} procs ", bytes(peak), cur.procs), t.muted()),
    ]);
    chart(
        f,
        t,
        ram_a,
        title,
        &ram,
        t.read,
        span_s,
        ram_max,
        ["0".into(), bytes((ram_max * 1024.0 * 1024.0) as u64)],
        sel,
    );
}

#[allow(clippy::too_many_arguments)]
fn chart(
    f: &mut Frame,
    t: &Theme,
    area: Rect,
    title: Line,
    data: &[(f64, f64)],
    color: ratatui::style::Color,
    span_s: f64,
    y_max: f64,
    y_labels: [String; 2],
    sel: Option<(f64, f64)>,
) {
    let marks: Vec<[(f64, f64); 2]> = sel
        .map(|(a, b)| vec![[(a, 0.0), (a, y_max)], [(b, 0.0), (b, y_max)]])
        .unwrap_or_default();
    let mut sets: Vec<Dataset> = marks
        .iter()
        .map(|m| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(t.accent))
                .data(m)
        })
        .collect();
    sets.push(
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(color))
            .data(data),
    );
    let mid = view::duration((span_s * 500.0) as i64);
    let end = view::duration((span_s * 1000.0) as i64);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.line))
        .title(title);
    let chart = Chart::new(sets)
        .block(block)
        .x_axis(
            Axis::default()
                .bounds([0.0, span_s.max(0.001)])
                .labels(vec![Line::from("0s"), Line::from(mid), Line::from(end)])
                .style(t.faint()),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, y_max])
                .labels(y_labels.into_iter().map(Line::from).collect::<Vec<_>>())
                .style(t.faint()),
        );
    f.render_widget(chart, area);
}

/// One line of a bar list: label, value, what to print after the bar.
struct Bar {
    label: String,
    value: f64,
    text: String,
    color: ratatui::style::Color,
    extra: Option<Span<'static>>,
}

#[allow(clippy::too_many_arguments)]
fn bars(f: &mut Frame, t: &Theme, area: Rect, title: &str, rows: Vec<Bar>, empty: &str, sel: Option<usize>) {
    let block = panel(t, title, sel.is_some());
    let inner = block.inner(area);
    f.render_widget(block, area);
    if rows.is_empty() {
        f.render_widget(Paragraph::new(Line::styled(empty.to_string(), t.muted())), inner);
        return;
    }
    let max = rows.iter().map(|r| r.value).fold(0.0, f64::max).max(f64::MIN_POSITIVE);
    let label_w = rows.iter().map(|r| r.label.width()).max().unwrap_or(4).clamp(4, 12) + 1;
    let text_w = rows.iter().map(|r| r.text.width()).max().unwrap_or(0) + 1;
    let bar_w = (inner.width as usize).saturating_sub(label_w + text_w + 4);
    // Keep the selected row in view.
    let sel = sel.map(|i| i.min(rows.len() - 1));
    let skip = sel.map_or(0, |i| (i + 1).saturating_sub(inner.height as usize));
    let lines: Vec<Line> = rows
        .into_iter()
        .enumerate()
        .skip(skip)
        .take(inner.height as usize)
        .map(|(i, r)| {
            let mut spans = vec![
                Span::styled(
                    format!("{:<label_w$}", ellipsize(&r.label, label_w - 1)),
                    Style::new().fg(t.ink),
                ),
                Span::styled(eighths(r.value / max * bar_w as f64), Style::new().fg(r.color)),
                Span::styled(format!(" {}", r.text), t.muted()),
            ];
            spans.extend(r.extra);
            let line = Line::from(spans);
            if sel == Some(i) { line.style(t.selected()) } else { line }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

// ── Statistics ─────────────────────────────────────────────────────────

fn who(agent_type: Option<&str>) -> String {
    match agent_type {
        None => format!("{BOSS_ICON} Watcher"),
        Some(a) => format!("{FOLLOWER_ICON} {}", view::agent_name(Some(a))),
    }
}

fn tile(f: &mut Frame, t: &Theme, area: Rect, title: &str, value: String, color: ratatui::style::Color, sub: String) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.line))
        .title(Span::styled(format!(" {title} "), t.muted()));
    let lines = vec![
        Line::styled(value, Style::new().fg(color).add_modifier(Modifier::BOLD)),
        Line::styled(sub, t.faint()),
    ];
    f.render_widget(Paragraph::new(lines).block(block).alignment(Alignment::Center), area);
}

fn stats_pane(f: &mut Frame, app: &App, t: &Theme, area: Rect, now: i64) {
    let Some(g) = &app.graph else { return };
    let Some(st) = app.stats(now) else { return };
    let costs = &st.costs;
    // The row under the cursor, in the panel that has it.
    let sel = |p: Panel| (app.focus == Focus::History && app.stats_panel == p).then_some(app.stats_row);

    let tall = area.height >= 34;
    let [tiles, mix_row, calls_row, res_row] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length((st.mix.len() as u16 + 2).clamp(4, if tall { 10 } else { 7 })),
        Constraint::Fill(1),
        Constraint::Fill(1),
    ])
    .areas(area);

    // Tiles
    let n_tiles = if tiles.width >= 120 { 6 } else { 4 };
    let cells = Layout::horizontal(vec![Constraint::Fill(1); n_tiles])
        .spacing(GUTTER)
        .split(tiles);
    let mut tile_data = vec![
        (
            "Wall time",
            view::duration(st.wall_ms),
            t.accent,
            format!("working {}", view::duration(st.time.thinking_ms + st.time.tools_ms)),
        ),
        (
            "Prompts",
            st.prompts.to_string(),
            t.prompt,
            if st.automated_prompts > 0 {
                format!("{} turns · {} automated", st.turns, st.automated_prompts)
            } else {
                format!("{} turn{}", st.turns, plural(st.turns))
            },
        ),
        (
            "Tool calls",
            st.calls.to_string(),
            if st.failed > 0 { t.err } else { t.ok },
            if st.failed > 0 {
                format!("{} failed · {:.0}%", st.failed, st.failure_rate() * 100.0)
            } else {
                "none failed".into()
            },
        ),
        (
            "Cost · API prices",
            if costs.has_usage() {
                format_usd(costs.total)
            } else {
                "—".into()
            },
            t.write,
            if costs.has_usage() {
                format!("{} tokens", format_tokens(costs.tokens()))
            } else {
                "no usage read yet".into()
            },
        ),
    ];
    if n_tiles == 6 {
        // While Claude works, what's happening now matters most; once it
        // stops, the last reading is stale and the peak tells the story.
        let last = g.samples.last();
        let working = g.status == SessionStatus::Running && last.is_some();
        let avg_rss =
            (!g.samples.is_empty()).then(|| g.samples.iter().map(|x| x.rss).sum::<u64>() / g.samples.len() as u64);
        let peak_cpu = st.peak_cpu.map(view::cores);
        let by = st
            .peak_cpu_process
            .as_deref()
            .map(|p| format!(" · {p}"))
            .unwrap_or_default();
        if working {
            let x = last.unwrap();
            tile_data.push((
                "CPU now",
                view::cores(x.cpu),
                t.exec,
                format!("peak {}{by}", st.peak_cpu.map(view::cores_value).unwrap_or_default()),
            ));
            tile_data.push((
                "RAM now",
                bytes(x.rss),
                t.read,
                format!("peak {}", st.peak_rss.map(bytes).unwrap_or_default()),
            ));
        } else {
            tile_data.push((
                "Peak CPU",
                peak_cpu.unwrap_or_else(|| "—".into()),
                t.exec,
                match st.avg_cpu {
                    Some(a) => format!("avg {}{by}", view::cores_value(a)),
                    None => "no readings".into(),
                },
            ));
            tile_data.push((
                "Peak RAM",
                st.peak_rss.map_or("—".into(), bytes),
                t.read,
                avg_rss.map_or("no readings".into(), |a| format!("avg {}", bytes(a))),
            ));
        }
    }
    for (cell, (title, value, color, sub)) in cells.iter().zip(tile_data) {
        tile(f, t, *cell, title, value, color, sub);
    }

    // Tool mix, time per tool, cost by model
    let [mix_a, time_a, cost_a] = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1), Constraint::Fill(1)])
        .spacing(GUTTER)
        .areas(mix_row);
    let cost_rows: Vec<Bar> = costs
        .by_model
        .iter()
        .map(|m| Bar {
            label: hooktop_core::pricing::model_key(&m.model),
            value: m.cost.unwrap_or(0.0),
            text: match m.cost {
                Some(c) => format!("{} · {}", format_usd(c), format_tokens(m.tokens)),
                None => format!("no price · {}", format_tokens(m.tokens)),
            },
            color: t.write,
            extra: None,
        })
        .collect();
    bars(
        f,
        t,
        cost_a,
        "Cost by model · API prices",
        cost_rows,
        "No token usage yet: hooktop reads it from Claude Code's transcript.",
        sel(Panel::Models),
    );
    // One row per tool: how often it ran and how long it took in total.
    let mix_rows: Vec<Bar> = st
        .mix
        .iter()
        .map(|m| Bar {
            label: m.name.clone(),
            value: m.count as f64,
            text: format!("{} · {}", m.count, view::duration(m.total_ms)),
            color: t.tool(&m.name),
            extra: (m.failed > 0).then(|| Span::styled(format!(" {}✗", m.failed), Style::new().fg(t.err))),
        })
        .collect();
    bars(
        f,
        t,
        mix_a,
        "Tools · calls and time",
        mix_rows,
        "No tool calls yet.",
        sel(Panel::Tools),
    );
    let split = st.time;
    let pct = |ms: i64| {
        if st.wall_ms > 0 {
            format!("{:.0}%", ms as f64 * 100.0 / st.wall_ms as f64)
        } else {
            "—".into()
        }
    };
    let time_rows: Vec<Bar> = [
        ("Thinking", split.thinking_ms, t.prompt),
        ("Tools", split.tools_ms, t.exec),
        ("Waiting", split.waiting_ms, t.line),
    ]
    .into_iter()
    .map(|(label, ms, color)| Bar {
        label: label.into(),
        value: ms as f64,
        text: format!("{} · {}", view::duration(ms), pct(ms)),
        color,
        extra: None,
    })
    .collect();
    bars(
        f,
        t,
        time_a,
        "Where the time went",
        time_rows,
        "Nothing yet.",
        sel(Panel::Time),
    );

    // Slowest calls and longest prompts
    let [slow_a, prompt_a] = Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)])
        .spacing(GUTTER)
        .areas(calls_row);
    let max_took = st.slowest.first().map_or(1, |c| c.took_ms).max(1);
    let rows: Vec<TRow> = st
        .slowest
        .iter()
        .enumerate()
        .map(|(i, c)| {
            TRow::new(vec![
                Cell::from(Span::styled(format!("{}", i + 1), t.faint())),
                Cell::from(Line::from(vec![
                    Span::styled("● ", Style::new().fg(t.tool(&c.name))),
                    Span::styled(c.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(repeats(c.repeats), Style::new().fg(t.accent)),
                    Span::styled(format!(" {}", c.target), t.muted()),
                ])),
                Cell::from(Span::styled(who(c.agent_type.as_deref()), t.muted())),
                Cell::from(Line::from(vec![Span::styled(
                    eighths(c.took_ms as f64 / max_took as f64 * 8.0),
                    Style::new().fg(if c.status == Status::Failed {
                        t.err
                    } else {
                        t.tool(&c.name)
                    }),
                )])),
                Cell::from(Line::styled(view::duration(c.took_ms), Style::new().fg(t.ink)).alignment(Alignment::Right)),
            ])
        })
        .collect();
    table(
        f,
        t,
        slow_a,
        "Slowest tool calls",
        rows,
        vec![
            Constraint::Length(2),
            // The call's name wins over the agent and bar columns when space runs out.
            Constraint::Min(14),
            Constraint::Length(13),
            Constraint::Length(9),
            Constraint::Min(7),
        ],
        "No tool calls yet.",
        sel(Panel::Slowest),
    );
    let rows: Vec<TRow> = st
        .longest_prompts
        .iter()
        .map(|p| {
            TRow::new(vec![
                Cell::from(Line::from(vec![
                    Span::styled("❯ ", Style::new().fg(t.prompt)),
                    Span::raw(p.title.clone()),
                ])),
                Cell::from(Span::styled(format!("{} tools", p.tools), t.muted())),
                Cell::from(Line::styled(view::duration(p.took_ms), Style::new().fg(t.ink)).alignment(Alignment::Right)),
                Cell::from(
                    Line::styled(
                        if costs.has_usage() {
                            format_usd(p.cost)
                        } else {
                            String::new()
                        },
                        Style::new().fg(t.write),
                    )
                    .alignment(Alignment::Right),
                ),
            ])
        })
        .collect();
    table(
        f,
        t,
        prompt_a,
        "Longest prompts · cost",
        rows,
        vec![
            Constraint::Fill(1),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(7),
        ],
        "No prompts yet.",
        sel(Panel::Prompts),
    );

    // Resources
    let [ram_a, cpu_a, side_a] = Layout::horizontal([Constraint::Fill(2), Constraint::Fill(2), Constraint::Fill(2)])
        .spacing(GUTTER)
        .areas(res_row);
    let no_samples = "No CPU/RAM readings for this session.";
    let call_rows = |list: &[hooktop_core::stats::CallStat], ram: bool| -> Vec<TRow<'static>> {
        list.iter()
            .map(|c| {
                let v = if ram {
                    bytes(c.peak_rss.unwrap_or(0))
                } else {
                    view::cores_value(c.avg_cpu.unwrap_or(0.0))
                };
                TRow::new(vec![
                    Cell::from(Line::from(vec![
                        Span::styled("● ", Style::new().fg(t.tool(&c.name))),
                        Span::styled(c.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
                        Span::styled(repeats(c.repeats), Style::new().fg(t.accent)),
                        Span::styled(format!(" {}", c.target), t.muted()),
                    ])),
                    Cell::from(
                        Line::styled(v, Style::new().fg(if ram { t.read } else { t.exec })).alignment(Alignment::Right),
                    ),
                    // Readings are per session: mark the ones that aren't this call's alone.
                    Cell::from(Span::styled(c.reading.mark().unwrap_or(""), t.faint())),
                ])
            })
            .collect()
    };
    // A key under each ranking, naming only the marks it uses.
    let legend = |list: &[hooktop_core::stats::CallStat]| -> Option<Line<'static>> {
        use hooktop_core::stats::Reading;
        let keys: Vec<String> = [Reading::Shared, Reading::Borrowed]
            .into_iter()
            .filter(|r| list.iter().any(|c| c.reading == *r))
            .map(|r| format!("{} {}", r.mark().unwrap_or(""), r.label().unwrap_or("")))
            .collect();
        (!keys.is_empty()).then(|| Line::styled(format!(" {} ", keys.join(" · ")), t.faint()).right_aligned())
    };
    let ram_block = panel(t, "RAM eaters · peak", sel(Panel::Ram).is_some());
    let ram_block = match legend(&st.ram_eaters) {
        Some(l) => ram_block.title_bottom(l),
        None => ram_block,
    };
    table_in(
        f,
        t,
        ram_a,
        ram_block,
        call_rows(&st.ram_eaters, true),
        vec![Constraint::Fill(1), Constraint::Length(8), Constraint::Length(1)],
        no_samples,
        sel(Panel::Ram),
    );
    let cpu_block = panel(t, "CPU hogs · avg cores", sel(Panel::Cpu).is_some());
    let cpu_block = match legend(&st.cpu_hogs) {
        Some(l) => cpu_block.title_bottom(l),
        None => cpu_block,
    };
    table_in(
        f,
        t,
        cpu_a,
        cpu_block,
        call_rows(&st.cpu_hogs, false),
        vec![Constraint::Fill(1), Constraint::Length(5), Constraint::Length(1)],
        no_samples,
        sel(Panel::Cpu),
    );
    let [proc_a, foll_a] = Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(side_a);
    let proc_rows: Vec<Bar> = st
        .processes
        .iter()
        .map(|p| Bar {
            label: p.name.clone(),
            value: p.peak_cpu as f64,
            text: format!("{} · top {}", view::cores(p.peak_cpu), view::duration(p.ms_on_top)),
            color: t.exec,
            extra: None,
        })
        .collect();
    bars(f, t, proc_a, "Busiest processes", proc_rows, no_samples, None);
    let foll_rows: Vec<Bar> = st
        .follower_runs
        .iter()
        .map(|r| Bar {
            label: format!("{FOLLOWER_ICON} {}", view::agent_name(r.agent_type.as_deref())),
            value: r.took_ms as f64,
            text: if costs.has_usage() {
                format!(
                    "{} · {} tools · {}",
                    view::duration(r.took_ms),
                    r.tools,
                    format_usd(r.cost)
                )
            } else {
                format!("{} · {} tools", view::duration(r.took_ms), r.tools)
            },
            color: t.sub,
            extra: None,
        })
        .collect();
    let foll_title = format!("Followers · {}", st.followers);
    bars(
        f,
        t,
        foll_a,
        &foll_title,
        foll_rows,
        "Watcher worked alone.",
        sel(Panel::Followers),
    );
}

#[allow(clippy::too_many_arguments)]
fn table(
    f: &mut Frame,
    t: &Theme,
    area: Rect,
    title: &str,
    rows: Vec<TRow>,
    widths: Vec<Constraint>,
    empty: &str,
    sel: Option<usize>,
) {
    table_in(f, t, area, panel(t, title, sel.is_some()), rows, widths, empty, sel);
}

/// A table in `block`; `sel` is the highlighted row when the panel has the cursor.
#[allow(clippy::too_many_arguments)]
fn table_in(
    f: &mut Frame,
    t: &Theme,
    area: Rect,
    block: Block,
    rows: Vec<TRow>,
    widths: Vec<Constraint>,
    empty: &str,
    sel: Option<usize>,
) {
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(Line::styled(empty.to_string(), t.muted())).block(block),
            area,
        );
        return;
    }
    let n = rows.len();
    let mut state = TableState::default().with_selected(sel.map(|i| i.min(n - 1)));
    f.render_stateful_widget(
        Table::new(rows, widths)
            .block(block)
            .column_spacing(1)
            .row_highlight_style(t.selected()),
        area,
        &mut state,
    );
}

/// A bar `cells` wide (fractional), drawn with eighth blocks.
fn eighths(cells: f64) -> String {
    const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let whole = cells.floor() as usize;
    let part = ((cells - whole as f64) * 8.0).round() as usize;
    let mut s = "█".repeat(whole);
    if part >= 8 {
        s.push('█');
    } else {
        s.push_str(PARTS[part]);
    }
    if s.is_empty() {
        s.push('▏');
    }
    s
}

/// ` ×3` after a ranked call that stands for several identical ones.
fn repeats(n: usize) -> String {
    if n > 1 { format!(" ×{n}") } else { String::new() }
}

fn bytes(b: u64) -> String {
    let mb = b as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

// ── Graph ──────────────────────────────────────────────────────────────

fn graph_pane(f: &mut Frame, app: &mut App, t: &Theme, area: Rect, now: i64) {
    let focused = app.focus == Focus::History;
    let mut block = panel(t, "Graph · tasks → subtasks", focused).title_bottom(
        Line::from(vec![
            Span::styled(" ━ ", Style::new().fg(t.accent)),
            Span::styled("selected  ", t.faint()),
            Span::styled("▶ ", Style::new().fg(t.line)),
            Span::styled("next  ", t.faint()),
            Span::styled("▼ ", Style::new().fg(t.sub)),
            Span::styled(format!("sent a {FOLLOWER_ICON} Follower  "), t.faint()),
            Span::styled("⏎ ", Style::new().fg(t.accent)),
            Span::styled("subtasks ", t.faint()),
        ])
        .alignment(Alignment::Right),
    );
    if app.follow {
        block = block.title(Line::styled(" following ", t.faint()).alignment(Alignment::Right));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(g) = app.graph.as_ref() else { return };
    let lay = app.layout();
    if lay.nodes.is_empty() {
        f.render_widget(
            Paragraph::new(Line::styled("Nothing has happened yet.", t.muted())),
            inner,
        );
        return;
    }
    let mut scroll = app.graph_scroll;
    if let Some(n) = app.graph_sel.and_then(|id| lay.get(id)) {
        scroll = dag::scroll_to(n, inner, scroll);
    }
    scroll.0 = scroll.0.min(lay.width.saturating_sub(inner.width));
    scroll.1 = scroll.1.min(lay.height.saturating_sub(inner.height));
    app.graph_scroll = scroll;
    let sel = app.graph_sel;
    dag::render(f.buffer_mut(), inner, t, g, &lay, sel, scroll, now);
}

fn header(f: &mut Frame, app: &App, t: &Theme, area: Rect, members: &[Member], now: i64) {
    let Some(g) = &app.graph else { return };
    let prompts = g
        .tasks
        .iter()
        .filter(|x| {
            x.header.kind == hooktop_core::TaskKind::Prompt
                && is_user_prompt(x)
                && !hooktop_core::stats::is_automated(&x.header.title, &app.automated_prompts)
        })
        .count();
    let tools: usize = g.tasks.iter().map(|x| x.tools.len()).sum();
    let failed: u32 = g.tasks.iter().map(|x| x.header.failed_count).sum();
    let end = if g.status == SessionStatus::Ended {
        g.updated_ms
    } else {
        now.max(g.updated_ms)
    };

    let title = Line::from(vec![
        Span::raw(" "),
        conn_dot(t, g.status),
        Span::raw(" "),
        Span::styled(g.display_title(), Style::new().add_modifier(Modifier::BOLD)),
    ]);
    let mut stats = vec![
        Span::raw("   "),
        Span::styled(view::connection_text(g.status), status_session_style(t, g.status)),
        Span::styled(" · ", t.faint()),
        Span::styled(view::duration(end - g.started_ms), t.muted()),
        Span::styled(" · ", t.faint()),
        Span::styled(format!("{prompts} prompt{}", plural(prompts)), t.muted()),
        Span::styled(" · ", t.faint()),
        Span::styled(format!("{tools} tool call{}", plural(tools)), t.muted()),
    ];
    if failed > 0 {
        stats.push(Span::styled(format!(" ({failed} failed)"), Style::new().fg(t.err)));
    }
    let costs = hooktop_core::stats::costs(g, &app.pricing);
    if costs.has_usage() {
        stats.push(Span::styled(" · ", t.faint()));
        stats.push(Span::styled(
            format_usd(costs.total),
            Style::new().fg(t.write).add_modifier(Modifier::BOLD),
        ));
        stats.push(Span::styled(
            format!(" for {} tokens", format_tokens(costs.tokens())),
            t.muted(),
        ));
    }
    let followers = members.len().saturating_sub(1);
    stats.push(Span::styled(" · ", t.faint()));
    stats.push(Span::styled(
        format!("{followers} follower{}", plural(followers)),
        t.muted(),
    ));
    if let Some(cwd) = &g.cwd {
        stats.push(Span::styled(" · ", t.faint()));
        stats.push(Span::styled(short_path(cwd), t.faint()));
    }
    f.render_widget(Paragraph::new(vec![title, Line::from(stats)]), area);
}

fn party(f: &mut Frame, app: &mut App, t: &Theme, area: Rect, members: &[Member], now: i64) {
    let focused = app.focus == Focus::Party;
    let block = panel(t, "Party", focused);
    let width = area.width.saturating_sub(4) as usize;
    let costs = app
        .graph
        .as_ref()
        .map(|g| (hooktop_core::stats::costs(g, &app.pricing), g));
    let items: Vec<ListItem> = members
        .iter()
        .map(|m| {
            let mut spans = Vec::new();
            if m.depth > 0 {
                spans.push(Span::styled(format!("{}└ ", "  ".repeat(m.depth - 1)), t.faint()));
            }
            let (icon, tone) = match m.role() {
                Role::Boss => (BOSS_ICON, t.accent),
                Role::Follower => (FOLLOWER_ICON, t.sub),
            };
            spans.push(Span::styled(icon, Style::new().fg(tone)));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format!("{} · ", m.role().label()), t.muted()));
            spans.push(Span::styled(
                m.name(),
                Style::new().fg(tone).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw("  "));
            spans.push(status_span(t, m.status));
            spans.push(Span::styled(
                format!("  {} tool{}", m.tools, plural(m.tools as usize)),
                t.muted(),
            ));
            if m.failed > 0 {
                spans.push(Span::styled(format!(" · {} failed", m.failed), Style::new().fg(t.err)));
            }
            spans.push(Span::styled(
                format!(" · {}", view::duration(m.ended_ms.unwrap_or(now) - m.started_ms)),
                t.muted(),
            ));
            if let Some((c, g)) = &costs
                && c.has_usage()
            {
                let v = match m.key {
                    None => c.boss(g),
                    Some(k) => c.of(k),
                };
                spans.push(Span::styled(" · ", t.muted()));
                spans.push(Span::styled(format_usd(v), Style::new().fg(t.write)));
            }
            let used: usize = spans.iter().map(|s| s.width()).sum();
            if width > used + 4 {
                spans.push(Span::styled(
                    format!("  {}", ellipsize(&m.title, width - used - 2)),
                    t.faint(),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items).block(block).highlight_style(t.selected());
    f.render_stateful_widget(list, area, &mut app.party_state);
}

fn history(f: &mut Frame, app: &mut App, t: &Theme, area: Rect, now: i64) {
    let Some(g) = app.graph.as_ref() else { return };
    let rows = app.rows();
    let title = match app.agent.and_then(|a| g.task(a)) {
        Some(task) => format!(
            "History · {} {}",
            FOLLOWER_ICON,
            view::agent_name(task.header.agent_type.as_deref())
        ),
        None => format!("History · {BOSS_ICON} Watcher"),
    };
    let mut block = panel(t, &title, app.focus == Focus::History);
    if app.follow {
        block = block.title(Line::styled(" following ", t.faint()).alignment(Alignment::Right));
    }

    if rows.is_empty() {
        let msg = if app.agent.is_some() {
            "This follower hasn't used any tools yet."
        } else {
            "Nothing has happened yet."
        };
        f.render_widget(Paragraph::new(Line::styled(msg, t.muted())).block(block), area);
        return;
    }

    let wide = area.width >= 84;
    let costs = hooktop_core::stats::costs(g, &app.pricing);
    let show_cost = costs.has_usage();
    let cost_w: u16 = if show_cost { 8 } else { 0 };
    // Fixed columns: status 10, tools 5, started 9, took 8, cost, plus
    // spacing and the cursor; give the timeline a good share of the rest.
    let spare = area.width.saturating_sub(2 + 1 + 10 + 5 + 9 + 8 + 5 + cost_w);
    let bar_w = (spare * 9 / 20).clamp(16, 90);
    let span_start = g.started_ms;
    let span_end = if g.status == SessionStatus::Ended {
        g.updated_ms
    } else {
        now.max(g.updated_ms)
    };

    let body: Vec<TRow> = rows
        .iter()
        .map(|r| {
            let end = r
                .ended_ms
                .unwrap_or(if r.status == Status::Running { now } else { r.started_ms });
            let mut cells = vec![
                Cell::from(name_line(t, g, r)),
                Cell::from(status_span(t, r.status)),
                Cell::from(Span::styled(tool_count(g, r), t.muted())),
                Cell::from(Line::styled(view::duration(end - r.started_ms), t.muted()).alignment(Alignment::Right)),
            ];
            if show_cost {
                let v = match r.kind {
                    RowKind::Tool => None,
                    RowKind::Turn => Some(costs.of(r.task)),
                    _ => Some(costs.prompt(g, r.task).0),
                };
                let text = v.filter(|v| *v > 0.0).map(format_usd).unwrap_or_default();
                cells.push(Cell::from(
                    Line::styled(text, Style::new().fg(t.write)).alignment(Alignment::Right),
                ));
            }
            if wide {
                cells.insert(
                    3,
                    Cell::from(Span::styled(
                        format!("+{}", view::duration(r.started_ms - span_start)),
                        t.faint(),
                    )),
                );
                cells.push(Cell::from(bar(t, g, r, span_start, span_end, end, bar_w)));
            }
            TRow::new(cells)
        })
        .collect();

    let mut widths = vec![
        Constraint::Fill(1),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(8),
    ];
    let mut head = vec!["Name", "Status", "Tools", "Took"];
    if show_cost {
        widths.push(Constraint::Length(cost_w - 1));
        head.push("Cost");
    }
    if wide {
        widths.insert(3, Constraint::Length(9));
        head.insert(3, "Started");
        widths.push(Constraint::Length(bar_w));
    }
    let axis = time_axis(span_end - span_start, bar_w as usize);
    let mut header_cells: Vec<Cell> = head
        .into_iter()
        .map(|h| {
            let line = Line::styled(h, t.faint());
            Cell::from(if h == "Took" || h == "Cost" {
                line.alignment(Alignment::Right)
            } else {
                line
            })
        })
        .collect();
    if wide {
        header_cells.push(Cell::from(Line::styled(axis, t.faint())));
    }
    let header = TRow::new(header_cells);
    let table = Table::new(body, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(t.selected())
        .highlight_symbol(Span::styled("▌", Style::new().fg(t.accent)));
    f.render_stateful_widget(table, area, &mut app.hist_state);
}

fn name_line<'a>(t: &Theme, g: &'a SessionGraph, r: &Row) -> Line<'a> {
    let mut spans = vec![Span::raw("  ".repeat(r.depth))];
    spans.push(Span::styled(
        if r.expandable {
            if r.expanded { "▾ " } else { "▸ " }
        } else {
            "  "
        },
        t.faint(),
    ));
    let task = &g.tasks[r.task as usize];
    match r.kind {
        RowKind::Tool => {
            let c = &task.tools[r.tool.unwrap_or(0)];
            spans.push(Span::styled("● ", Style::new().fg(t.tool(&c.name))));
            spans.push(Span::styled(
                view::tool_name(&c.name).to_string(),
                Style::new().add_modifier(Modifier::BOLD),
            ));
            if let Some(sub) = c.spawned_task.and_then(|s| g.task(s)) {
                spans.push(Span::styled(
                    format!(
                        " {FOLLOWER_ICON} {}",
                        view::agent_name(sub.header.agent_type.as_deref())
                    ),
                    Style::new().fg(t.sub),
                ));
            }
            spans.push(Span::raw(" "));
            spans.push(Span::styled(view::first_line(&c.target).to_string(), t.muted()));
        }
        RowKind::Turn => {
            spans.push(Span::styled(
                task.header.label.clone(),
                Style::new().fg(t.turn).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(" · ", t.faint()));
            spans.push(Span::raw(view::first_line(&task.header.title).to_string()));
        }
        RowKind::Prompt | RowKind::Command => {
            let tag = if r.kind == RowKind::Command { "/ " } else { "❯ " };
            spans.push(Span::styled(
                tag,
                Style::new().fg(t.prompt).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                view::first_line(&task.header.title).to_string(),
                Style::new().add_modifier(Modifier::BOLD),
            ));
        }
        RowKind::Notification | RowKind::System => {
            spans.push(Span::styled(format!("{} · ", task.header.label), t.faint()));
            spans.push(Span::styled(
                view::first_line(&task.header.title).to_string(),
                t.muted(),
            ));
        }
    }
    Line::from(spans)
}

fn tool_count(g: &SessionGraph, r: &Row) -> String {
    match r.kind {
        RowKind::Tool => String::new(),
        RowKind::Turn => g.tasks[r.task as usize].tools.len().to_string(),
        _ => {
            let n: usize = view::prompt_groups(g)
                .into_iter()
                .find(|(p, _)| *p == Some(r.task))
                .map(|(_, turns)| turns.iter().map(|t| g.tasks[*t as usize].tools.len()).sum())
                .unwrap_or(0);
            if n == 0 { String::new() } else { n.to_string() }
        }
    }
}

/// `0s        3.2 s        6.5 s`, spread over `width` cells.
fn time_axis(span_ms: i64, width: usize) -> String {
    let a = "0s".to_string();
    let m = view::duration(span_ms / 2);
    let b = view::duration(span_ms);
    if width < a.len() + m.len() + b.len() + 4 {
        return format!("{a}{}{b}", " ".repeat(width.saturating_sub(a.len() + b.len())));
    }
    let mid_at = width / 2 - m.len() / 2;
    let mut s = a.clone();
    s.push_str(&" ".repeat(mid_at - a.len()));
    s.push_str(&m);
    s.push_str(&" ".repeat(width - s.chars().count() - b.len()));
    s.push_str(&b);
    s
}

fn bar<'a>(t: &Theme, g: &SessionGraph, r: &Row, start: i64, end: i64, row_end: i64, width: u16) -> Line<'a> {
    let span = (end - start).max(1) as f64;
    let w = width as f64;
    let a = (((r.started_ms - start) as f64 / span) * w).floor().clamp(0.0, w - 1.0) as usize;
    let b = (((row_end - start) as f64 / span) * w).ceil().clamp(a as f64 + 1.0, w) as usize;
    let tone = match r.kind {
        RowKind::Tool => r.tool_call(g).map(|c| t.tool(&c.name)).unwrap_or(t.other),
        RowKind::Turn => t.turn,
        RowKind::Prompt | RowKind::Command => t.prompt,
        _ => t.faint,
    };
    let tone = if r.status == Status::Failed { t.err } else { tone };
    Line::from(vec![
        Span::styled("·".repeat(a), Style::new().fg(t.line)),
        Span::styled("━".repeat(b - a), Style::new().fg(tone)),
        Span::styled("·".repeat(width as usize - b), Style::new().fg(t.line)),
    ])
}

// ── Details ────────────────────────────────────────────────────────────

fn details(f: &mut Frame, app: &App, t: &Theme, area: Rect, now: i64) {
    let block = panel(t, "Details", false);
    let Some(g) = &app.graph else { return };
    let lines = match app.selected_row() {
        Some(r) => detail_lines(t, g, &r, now, &hooktop_core::stats::costs(g, &app.pricing)),
        None => vec![Line::styled(
            "Select a row to see what went in and what came out.",
            t.muted(),
        )],
    };
    let inner_h = area.height.saturating_sub(2);
    let max_scroll = (wrapped_height(&lines, area.width.saturating_sub(2)) as u16).saturating_sub(inner_h);
    let scroll = app.details_scroll.min(max_scroll);
    let mut block = block;
    if max_scroll > 0 {
        block = block.title(Line::styled(" PgUp/PgDn ", t.faint()).alignment(Alignment::Right));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn detail_lines<'a>(
    t: &Theme,
    g: &'a SessionGraph,
    r: &Row,
    now: i64,
    costs: &hooktop_core::stats::Costs,
) -> Vec<Line<'a>> {
    let task = &g.tasks[r.task as usize];
    let mut out = Vec::new();
    let end = r
        .ended_ms
        .unwrap_or(if r.status == Status::Running { now } else { r.started_ms });
    let fact = |out: &mut Vec<Line<'a>>, k: &str, v: Span<'a>| {
        out.push(Line::from(vec![Span::styled(format!("{k:<11}"), t.muted()), v]));
    };
    match r.tool_call(g) {
        Some(c) => {
            out.push(Line::from(vec![
                Span::styled("● ", Style::new().fg(t.tool(&c.name))),
                Span::styled(
                    view::tool_name(&c.name).to_string(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  in {}", owner(task)), t.faint()),
            ]));
            if !c.target.is_empty() {
                out.push(Line::raw(c.target.clone()));
            }
            out.push(Line::raw(""));
            fact(&mut out, "Status", status_span(t, c.status));
            fact(&mut out, "Took", Span::raw(took(c.status, end - c.started_ms)));
            fact(
                &mut out,
                "Started",
                Span::raw(format!("+{}", view::duration(c.started_ms - g.started_ms))),
            );
            if let Some(sub) = c.spawned_task.and_then(|s| g.task(s)) {
                fact(
                    &mut out,
                    "Follower",
                    Span::styled(
                        format!(
                            "{FOLLOWER_ICON} {} · ⏎ to follow",
                            view::agent_name(sub.header.agent_type.as_deref())
                        ),
                        Style::new().fg(t.sub),
                    ),
                );
            }
            tool_sections(t, c, &mut out);
        }
        None => {
            let h = &task.header;
            let label = match r.kind {
                RowKind::Turn => h.label.clone(),
                RowKind::Prompt => "Prompt".into(),
                _ => h.label.clone(),
            };
            out.push(Line::from(vec![Span::styled(
                label,
                Style::new()
                    .fg(if r.kind == RowKind::Turn { t.turn } else { t.prompt })
                    .add_modifier(Modifier::BOLD),
            )]));
            out.push(Line::raw(""));
            fact(&mut out, "Status", status_span(t, r.status));
            let dur_label = if r.kind == RowKind::Turn { "Took" } else { "Answered" };
            fact(&mut out, dur_label, Span::raw(took(r.status, end - r.started_ms)));
            fact(
                &mut out,
                "Started",
                Span::raw(format!("+{}", view::duration(r.started_ms - g.started_ms))),
            );
            if r.kind == RowKind::Turn {
                let failed = if h.failed_count > 0 {
                    format!(" · {} failed", h.failed_count)
                } else {
                    String::new()
                };
                fact(&mut out, "Tool calls", Span::raw(format!("{}{failed}", h.tool_count)));
            }
            // Tokens and cost: a turn's own model calls, or a prompt and
            // every turn that answered it.
            let tasks: Vec<hooktop_core::TaskId> = match r.kind {
                RowKind::Turn => vec![r.task],
                _ => std::iter::once(r.task)
                    .chain(
                        view::prompt_groups(g)
                            .into_iter()
                            .find(|(p, _)| *p == Some(r.task))
                            .map(|(_, turns)| turns)
                            .unwrap_or_default(),
                    )
                    .collect(),
            };
            let mine: Vec<_> = g
                .usage
                .iter()
                .filter(|u| u.task.is_some_and(|x| tasks.contains(&x)))
                .collect();
            if !mine.is_empty() {
                let sum = |f: fn(&hooktop_core::usage::Usage) -> u64| mine.iter().map(|u| f(u)).sum::<u64>();
                let (input, output) = (sum(|u| u.input), sum(|u| u.output));
                let (read, write) = (sum(|u| u.cache_read), sum(|u| u.cache_write_5m + u.cache_write_1h));
                fact(
                    &mut out,
                    "Tokens",
                    Span::raw(format!(
                        "{} in · {} out · {} cache read · {} cache write",
                        format_tokens(input),
                        format_tokens(output),
                        format_tokens(read),
                        format_tokens(write)
                    )),
                );
                let cost: f64 = tasks.iter().map(|x| costs.of(*x)).sum();
                fact(
                    &mut out,
                    "Cost",
                    Span::styled(
                        format!("{} at API prices · {} model calls", format_usd(cost), mine.len()),
                        Style::new().fg(t.write),
                    ),
                );
            }
            if let Some(s) = &h.summary {
                section(
                    t,
                    &mut out,
                    if r.kind == RowKind::Command {
                        "Command"
                    } else {
                        "Prompt"
                    },
                );
                text_block(&mut out, s, Style::new(), 400);
            } else if r.kind != RowKind::Turn {
                section(t, &mut out, "Prompt");
                text_block(&mut out, &h.title, Style::new(), 400);
            }
            if r.kind != RowKind::Turn {
                section(t, &mut out, "Claude's reply");
                match &h.reply {
                    Some(reply) => text_block(&mut out, reply, Style::new().fg(t.accent), 400),
                    None if r.status == Status::Running => {
                        out.push(Line::styled("Claude is still working on this…", t.muted()))
                    }
                    None => out.push(Line::styled("—", t.faint())),
                }
            } else if let Some(reply) = &h.reply {
                section(t, &mut out, "Message");
                text_block(&mut out, reply, Style::new(), 400);
            }
        }
    }
    out
}

fn tool_sections<'a>(t: &Theme, c: &'a ToolCall, out: &mut Vec<Line<'a>>) {
    section(t, out, "Input");
    let input = serde_json::to_string_pretty(&c.input).unwrap_or_default();
    text_block(out, &input, t.muted(), 200);
    if let Some(e) = &c.error {
        section(t, out, "Error");
        text_block(out, e, Style::new().fg(t.err), 200);
    }
    section(t, out, "Output");
    match &c.output {
        Some(o) if !o.trim().is_empty() => text_block(out, o, Style::new(), 300),
        _ if c.status == Status::Running => out.push(Line::styled("Waiting for the result…", t.muted())),
        _ => out.push(Line::styled("—", t.faint())),
    }
}

fn section<'a>(t: &Theme, out: &mut Vec<Line<'a>>, title: &str) {
    out.push(Line::raw(""));
    out.push(Line::styled(
        title.to_uppercase(),
        t.faint().add_modifier(Modifier::BOLD),
    ));
}

fn text_block<'a>(out: &mut Vec<Line<'a>>, s: &str, style: Style, max_lines: usize) {
    let lines: Vec<&str> = s.lines().collect();
    for l in lines.iter().take(max_lines) {
        out.push(Line::styled(l.replace('\t', "    "), style));
    }
    if lines.len() > max_lines {
        out.push(Line::styled(
            format!("… {} more lines", lines.len() - max_lines),
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
}

fn owner(task: &Task) -> String {
    match task.header.agent_type.as_deref() {
        Some(a) => format!("{FOLLOWER_ICON} {}", view::agent_name(Some(a))),
        None => task.header.label.clone(),
    }
}

fn took(status: Status, ms: i64) -> String {
    let d = view::duration(ms);
    if status == Status::Running {
        format!("{d} (live)")
    } else {
        d
    }
}

// ── Statistics details ─────────────────────────────────────────────────

fn drill_overlay(f: &mut Frame, app: &App, t: &Theme, now: i64) {
    let Some(st) = app.stats(now) else { return };
    let Some(d) = app.drill(&st) else { return };
    let area = f.area();
    let w = (area.width * 9 / 10).clamp(40, 130).min(area.width.saturating_sub(2));
    // Tall enough for the facts and the list, at most most of the screen.
    let want = d.facts.len() + d.items.len() + if d.note.is_some() { 6 } else { 4 } + 2;
    let h = (want as u16)
        .clamp(12, (area.height * 4 / 5).max(12))
        .min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let block = panel(t, &d.title, true)
        .title_bottom(Line::styled(" ↑↓ pick · enter show in Timeline · esc close ", t.faint()).right_aligned());
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let key_w = d.facts.iter().map(|(k, _)| k.width()).max().unwrap_or(0) + 2;
    let mut head: Vec<Line> = d
        .facts
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("{k:<key_w$}"), t.muted()),
                Span::styled(v.clone(), Style::new().fg(t.ink)),
            ])
        })
        .collect();
    if let Some(n) = &d.note {
        head.push(Line::raw(""));
        head.push(Line::styled(n.clone(), t.faint()));
    }
    head.push(Line::raw(""));
    head.push(Line::styled(
        d.items_title.clone(),
        Style::new().fg(t.ink).add_modifier(Modifier::BOLD),
    ));
    // Rows the head needs once wrapped, so the list starts right below it.
    let cols = inner.width.max(1) as usize;
    let head_h: u16 = head.iter().map(|l| l.width().max(1).div_ceil(cols) as u16).sum();
    let [top, list] = Layout::vertical([Constraint::Length(head_h), Constraint::Fill(1)]).areas(inner);
    f.render_widget(Paragraph::new(head).wrap(Wrap { trim: false }), top);

    if d.items.is_empty() {
        f.render_widget(Paragraph::new(Line::styled("Nothing here yet.", t.muted())), list);
        return;
    }
    let value_w = d.items.iter().map(|i| i.value.width()).max().unwrap_or(0) as u16;
    let rows: Vec<TRow> = d
        .items
        .iter()
        .map(|i| {
            TRow::new(vec![
                Cell::from(Line::from(vec![
                    Span::styled(
                        i.label.clone(),
                        Style::new()
                            .fg(if i.failed { t.err } else { t.ink })
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {}", i.detail), t.muted()),
                ])),
                Cell::from(Line::styled(i.value.clone(), Style::new().fg(t.ink)).alignment(Alignment::Right)),
            ])
        })
        .collect();
    let mut state = TableState::default().with_selected(Some(app.drill_sel.min(d.items.len() - 1)));
    f.render_stateful_widget(
        Table::new(rows, [Constraint::Fill(1), Constraint::Length(value_w)])
            .column_spacing(2)
            .row_highlight_style(t.selected()),
        list,
        &mut state,
    );
}

// ── Help ───────────────────────────────────────────────────────────────

fn help(f: &mut Frame, app: &App, t: &Theme) {
    let keys: &[(&str, &str)] = &[
        ("o / O", "next / previous tab: Timeline, Graph, Statistics"),
        ("s", "statistics; there, tab picks a panel and enter opens a row"),
        ("x", "remove this connection and its history"),
        ("X", "remove closed and long-quiet connections"),
        ("tab / shift-tab", "next / previous pane (1 2 3 jump)"),
        ("↑ ↓  or  j k", "move"),
        ("→ ←  or  l h", "expand / collapse, or go to the parent"),
        ("enter", "toggle a row or node; on a spawn, follow that follower"),
        ("esc", "back to Watcher's history"),
        ("g / G", "first / last row (G also follows new work)"),
        ("e / c", "expand / collapse everything"),
        ("f", "follow: jump to where Claude is working"),
        ("n", "show or hide notifications"),
        ("d  or  ]", "show or hide details"),
        ("[", "show or hide connections"),
        ("PgUp / PgDn", "scroll details"),
        ("t", "theme: Ayu Dark or Ayu Light"),
        ("q", "quit"),
    ];
    let mut lines = vec![
        Line::from(vec![
            Span::raw(BOSS_ICON),
            Span::styled(" Watcher", Style::new().fg(t.accent).add_modifier(Modifier::BOLD)),
            Span::styled(" is the Boss: the main agent. Every subagent is a ", t.muted()),
            Span::styled(format!("{FOLLOWER_ICON} Follower"), Style::new().fg(t.sub)),
            Span::styled(".", t.muted()),
        ]),
        Line::styled(
            "Each Claude Code session is a connection: open while it runs.",
            t.muted(),
        ),
        Line::styled(
            "CPU/RAM is read once a second for the whole session: ≈ shared, another call ran \
             too; ↳ borrowed, too short for a reading, so it uses the next one.",
            t.muted(),
        ),
        Line::raw(""),
    ];
    for (k, v) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<18}"), Style::new().fg(t.accent)),
            Span::raw(*v),
        ]));
    }
    let mode = match app.mode {
        Mode::Attached => "attached to",
        Mode::Embedded => "running",
    };
    let server = app.server_version.as_deref().unwrap_or(crate::VERSION);
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!(
            "hooktop v{} · {mode} the server on :{} (v{server})",
            crate::VERSION,
            app.port
        ),
        t.faint(),
    ));
    let area = f.area();
    let w = 80.min(area.width.saturating_sub(2));
    // Rows after wrapping, inside the border and padding.
    let inner_w = w.saturating_sub(4).max(1) as usize;
    let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner_w)).sum();
    let h = (rows as u16 + 2).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel(t, "Keys", true)),
        rect,
    );
}

// ── Helpers ────────────────────────────────────────────────────────────

fn status_span(t: &Theme, s: Status) -> Span<'static> {
    match s {
        Status::Running => Span::styled("● running", Style::new().fg(t.accent)),
        Status::Done => Span::styled("✓ done", Style::new().fg(t.ok)),
        Status::Failed => Span::styled("✗ failed", Style::new().fg(t.err)),
        Status::Interrupted => Span::styled("◌ stopped", t.faint()),
    }
}

fn status_session_style(t: &Theme, s: SessionStatus) -> Style {
    match s {
        SessionStatus::Running => Style::new().fg(t.accent),
        SessionStatus::Idle => Style::new().fg(t.ok),
        SessionStatus::Ended => t.faint(),
    }
}

fn is_user_prompt(t: &Task) -> bool {
    matches!(t.header.label.as_str(), "Prompt" | "Command")
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn ago(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..5 => "just now".into(),
        5..60 => format!("{s}s ago"),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

fn short_path(p: &str) -> String {
    if let Some(home) = dirs::home_dir().and_then(|h| h.to_str().map(str::to_string))
        && let Some(rest) = p.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    p.to_string()
}

/// Cut `s` to `width` terminal cells, ending in `…` when it was cut.
pub fn ellipsize(s: &str, width: usize) -> String {
    let s = view::first_line(s);
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Roughly how many rows `lines` take when wrapped to `width`.
fn wrapped_height(lines: &[Line], width: u16) -> usize {
    let w = width.max(1) as usize;
    lines.iter().map(|l| l.width().div_ceil(w).max(1)).sum()
}
