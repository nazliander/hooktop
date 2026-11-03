use hooktop_core::SessionSummary;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, Focus, Mode};
use super::client::{Net, ServerMsg};
use super::theme::ThemeName;
use super::ui;

fn key(app: &mut App, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

fn demo_app() -> App {
    let g = crate::demo::demo_graph("demo-1");
    let mut app = App::new(4317, Mode::Embedded, ThemeName::Dark);
    app.on_net(Net::Up);
    let summary: SessionSummary = g.summary();
    app.on_net(Net::Msg(Box::new(ServerMsg::Hello {
        version: "test".into(),
        sessions: vec![summary],
    })));
    assert_eq!(app.current.as_deref(), Some("demo-1"), "first connection is opened");
    assert!(app.outbox.iter().any(|m| m.contains("subscribe")));
    app.on_net(Net::Msg(Box::new(ServerMsg::Snapshot {
        session_id: "demo-1".into(),
        seq: 1,
        graph: Box::new(g),
    })));
    app
}

fn render(app: &mut App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    let now = app.graph.as_ref().map(|g| g.updated_ms).unwrap_or(0);
    term.draw(|f| ui::draw(f, app, now)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn renders_the_party_and_history() {
    let mut app = demo_app();
    let screen = render(&mut app, 160, 44);
    for want in [
        "hooktop",
        "Connections",
        "Party",
        "Boss · Watcher",
        "Follower ·",
        "History",
        "Details",
        "❯",
    ] {
        assert!(screen.contains(want), "missing {want:?} in\n{screen}");
    }
    // Followed: cursor on the newest row.
    let rows = app.rows();
    assert_eq!(app.hist_state.selected(), Some(rows.len() - 1));
}

#[test]
fn renders_narrow_and_empty() {
    let mut app = App::new(4317, Mode::Attached, ThemeName::Dark);
    let s = render(&mut app, 70, 20);
    assert!(s.contains("Connecting to hooktop"), "{s}");
    app.on_net(Net::Up);
    app.on_net(Net::Msg(Box::new(ServerMsg::Hello {
        version: "x".into(),
        sessions: vec![],
    })));
    let s = render(&mut app, 100, 24);
    assert!(s.contains("hooktop hooks install"), "{s}");
    let mut app = demo_app();
    let s = render(&mut app, 80, 24);
    assert!(s.contains("Party"), "{s}");
}

#[test]
fn keys_fold_and_follow_followers() {
    let mut app = demo_app();
    assert_eq!(app.focus, Focus::History);
    let before = app.rows().len();
    key(&mut app, KeyCode::Char('c'));
    let collapsed = app.rows().len();
    assert!(collapsed < before, "collapse all hides rows");
    assert!(!app.follow);
    key(&mut app, KeyCode::Char('e'));
    assert_eq!(app.rows().len(), before.max(app.rows().len()));

    // Find a spawn row and follow it.
    let g = app.graph.clone().unwrap();
    let rows = app.rows();
    let i = rows
        .iter()
        .position(|r| r.tool_call(&g).is_some_and(|c| c.spawned_task.is_some()))
        .expect("demo spawns a follower");
    key(&mut app, KeyCode::Char('g'));
    for _ in 0..i {
        key(&mut app, KeyCode::Down);
    }
    key(&mut app, KeyCode::Enter);
    assert!(app.agent.is_some(), "enter on a spawn follows the follower");
    assert_ne!(app.party_state.selected(), Some(0));
    let s = render(&mut app, 160, 44);
    assert!(s.contains("History · ♞"), "{s}");
    key(&mut app, KeyCode::Esc);
    assert_eq!(app.agent, None);

    // Party pane: moving selects a member's history.
    key(&mut app, KeyCode::Char('2'));
    key(&mut app, KeyCode::Down);
    assert!(app.agent.is_some());

    key(&mut app, KeyCode::Char('?'));
    assert!(render(&mut app, 120, 40).contains("Keys"));
    key(&mut app, KeyCode::Esc);
    assert!(!app.help);
    key(&mut app, KeyCode::Char('q'));
    assert!(app.quit);
}

#[test]
fn deltas_apply_live() {
    let full = crate::demo::demo_graph("demo-1");
    let mut app = App::new(4317, Mode::Embedded, ThemeName::Dark);
    app.on_net(Net::Up);
    app.on_net(Net::Msg(Box::new(ServerMsg::Hello {
        version: "t".into(),
        sessions: vec![full.summary()],
    })));
    // Start from an empty graph and stream the full one in as deltas.
    let mut empty = hooktop_core::SessionGraph::new("demo-1", full.started_ms);
    empty.title = full.title.clone();
    app.on_net(Net::Msg(Box::new(ServerMsg::Snapshot {
        session_id: "demo-1".into(),
        seq: 0,
        graph: Box::new(empty),
    })));
    let mut deltas = Vec::new();
    for t in &full.tasks {
        let mut header = t.header.clone();
        header.tool_count = 0;
        deltas.push(hooktop_core::Delta::Task { task: header });
        for c in &t.tools {
            deltas.push(hooktop_core::Delta::Tool {
                task_id: t.header.id,
                tool: c.clone(),
            });
        }
    }
    for e in &full.edges {
        deltas.push(hooktop_core::Delta::Edge(*e));
    }
    app.on_net(Net::Msg(Box::new(ServerMsg::Deltas {
        session_id: "demo-1".into(),
        seq: 9,
        deltas,
    })));
    let g = app.graph.as_ref().unwrap();
    assert_eq!(g.tasks.len(), full.tasks.len());
    assert_eq!(app.members().len(), hooktop_core::view::party(&full).len());
}

#[test]
fn server_messages_deserialize() {
    let g = crate::demo::demo_graph("demo-1");
    let json = serde_json::json!({ "type": "snapshot", "session_id": "demo-1", "seq": 3, "graph": g });
    let msg: ServerMsg = serde_json::from_value(json).unwrap();
    assert!(matches!(msg, ServerMsg::Snapshot { .. }));
    let d: ServerMsg =
        serde_json::from_str(r#"{"type":"deltas","session_id":"x","seq":2,"deltas":[{"op":"edge","from":0,"to":1}]}"#)
            .unwrap();
    assert!(matches!(d, ServerMsg::Deltas { .. }));
}

#[test]
fn themes_parse_and_cycle() {
    assert_eq!(ThemeName::parse("Ayu Dark"), Some(ThemeName::Dark));
    assert_eq!(ThemeName::parse("mirage"), None, "only dark and light");
    assert_eq!(ThemeName::parse("ayu-light"), Some(ThemeName::Light));
    assert_eq!(ThemeName::Light.next(), ThemeName::Dark);
    assert_eq!(ThemeName::Dark.next(), ThemeName::Light);
    // 256-colour fallback keeps the ayu look: accent yellow, error red.
    assert_eq!(super::theme::nearest_256(0xe6, 0xb4, 0x50), 179);
    assert_eq!(super::theme::nearest_256(0x0d, 0x10, 0x17), 232);
    let t = ThemeName::Dark.theme().to_256();
    assert!(matches!(t.accent, ratatui::style::Color::Indexed(_)));
    assert_eq!(ui::ellipsize("hello world", 6), "hello…");
}

#[test]
fn graph_view_lays_out_and_walks_the_dag() {
    let mut app = demo_app();
    key(&mut app, KeyCode::Char('o'));
    assert_eq!(app.view, super::app::View::Graph);
    let lay = app.layout();
    let g = app.graph.clone().unwrap();
    // Every main-thread task is on lane 0; the Follower sits below its sender.
    let follower = g
        .tasks
        .iter()
        .find(|t| t.header.kind == hooktop_core::TaskKind::Subagent)
        .unwrap();
    let node = lay.get(follower.header.id).unwrap();
    assert_eq!(node.lane, 1);
    let parent = lay.get(node.parent.expect("spawned by a turn")).unwrap();
    assert_eq!(parent.lane, 0);
    assert!(node.y > parent.y + parent.h);
    // No overlapping boxes.
    for a in &lay.nodes {
        for b in &lay.nodes {
            if a.id != b.id && a.lane == b.lane {
                assert!(a.x + super::dag::NODE_W <= b.x || b.x + super::dag::NODE_W <= a.x);
            }
        }
    }

    // Walk: first node, right to the spawning turn, down to the Follower.
    key(&mut app, KeyCode::Char('g'));
    assert_eq!(app.graph_sel, lay.first());
    while app.graph_sel != Some(parent.id) {
        let before = app.graph_sel;
        key(&mut app, KeyCode::Right);
        assert_ne!(app.graph_sel, before, "right keeps moving along the main lane");
    }
    key(&mut app, KeyCode::Enter);
    assert!(app.graph_open.contains(&parent.id), "enter shows subtasks");
    assert!(app.layout().get(parent.id).unwrap().h > 4);
    key(&mut app, KeyCode::Down);
    assert_eq!(app.graph_sel, Some(follower.header.id));
    key(&mut app, KeyCode::Up);
    assert_eq!(app.graph_sel, Some(parent.id));

    let s = render(&mut app, 170, 44);
    for want in ["Graph · tasks → subtasks", "Explorer", "▼", "▶", "Turn 1"] {
        assert!(s.contains(want), "missing {want:?} in\n{s}");
    }
    // Details follow the graph selection.
    assert_eq!(app.selected_row().map(|r| r.task), Some(parent.id));
    key(&mut app, KeyCode::Char('o'));
    assert_eq!(app.view, super::app::View::Stats);
}

#[test]
fn timeline_shows_cpu_ram_and_tool_mix() {
    let mut app = demo_app();
    assert!(
        !app.graph.as_ref().unwrap().samples.is_empty(),
        "the demo records samples"
    );
    let s = render(&mut app, 190, 50);
    for want in ["CPU", "peak", "RAM", "Timeline", "Graph", "Statistics"] {
        assert!(s.contains(want), "missing {want:?} in\n{s}");
    }
    // Without samples the charts explain why.
    app.graph.as_mut().unwrap().samples.clear();
    let s = render(&mut app, 190, 50);
    assert!(s.contains("No CPU or RAM readings"), "{s}");
}

#[test]
fn statistics_tab_ranks_the_run() {
    let mut app = demo_app();
    key(&mut app, KeyCode::Char('s'));
    assert_eq!(app.view, super::app::View::Stats);
    let s = render(&mut app, 190, 50);
    for want in [
        "Wall time",
        "Tool calls",
        "Tools · calls and time",
        "Peak CPU",
        "Peak RAM",
        "avg ",
        "Where the time went",
        "Thinking",
        "Waiting",
        "Slowest tool calls",
        "Longest prompts",
        "RAM eaters",
        "CPU hogs",
        "Busiest processes",
        "Followers",
        "cargo",
        "Cost · API prices",
        "Cost by model",
        "opus-5-5",
        "haiku-5-5",
    ] {
        assert!(s.contains(want), "missing {want:?} in\n{s}");
    }
    // o cycles through all three tabs.
    key(&mut app, KeyCode::Char('o'));
    assert_eq!(app.view, super::app::View::Timeline);
    key(&mut app, KeyCode::Char('O'));
    assert_eq!(app.view, super::app::View::Stats);
}

#[test]
fn automated_prompts_are_counted_apart() {
    let mut app = demo_app();
    app.automated_prompts = vec!["Add a regression".into()];
    key(&mut app, KeyCode::Char('s'));
    let s = render(&mut app, 190, 50);
    assert!(s.contains("1 automated"), "{s}");
    assert!(!s.contains("Add a regre"), "not among the longest prompts:\n{s}");
}

#[test]
fn removing_a_connection_asks_first() {
    let mut app = demo_app();
    app.outbox.clear();
    key(&mut app, KeyCode::Char('x'));
    assert!(matches!(app.confirm, Some(super::app::Confirm::Remove { .. })));
    assert!(render(&mut app, 160, 40).contains("Remove"));
    key(&mut app, KeyCode::Char('n'));
    assert!(
        app.confirm.is_none() && app.outbox.is_empty(),
        "anything but y keeps it"
    );

    key(&mut app, KeyCode::Char('x'));
    key(&mut app, KeyCode::Char('y'));
    assert!(
        app.outbox
            .iter()
            .any(|m| m.contains("\"remove\"") && m.contains("demo-1"))
    );
    app.on_net(Net::Msg(Box::new(ServerMsg::Removed {
        session_ids: vec!["demo-1".into()],
    })));
    assert!(app.sessions.is_empty());
    assert!(app.current.is_none() && app.graph.is_none());
}

#[test]
fn follow_jumps_to_the_working_connection() {
    let g = crate::demo::demo_graph("demo-1");
    let mut idle = g.summary();
    idle.status = hooktop_core::SessionStatus::Idle;
    let mut busy = g.summary();
    busy.session_id = "other".into();
    busy.status = hooktop_core::SessionStatus::Running;
    busy.updated_ms = idle.updated_ms - 10;
    let mut app = App::new(4317, Mode::Embedded, ThemeName::Dark);
    app.on_net(Net::Up);
    app.on_net(Net::Msg(Box::new(ServerMsg::Hello {
        version: "t".into(),
        sessions: vec![idle.clone()],
    })));
    assert_eq!(app.current.as_deref(), Some("demo-1"));
    app.on_net(Net::Msg(Box::new(ServerMsg::Sessions {
        sessions: vec![busy.clone()],
    })));
    assert_eq!(
        app.current.as_deref(),
        Some("other"),
        "following moves to where Claude works"
    );

    // Picking a connection by hand turns follow off.
    key(&mut app, KeyCode::Char('1'));
    key(&mut app, KeyCode::Up);
    assert_eq!(app.current.as_deref(), Some("demo-1"));
    assert!(!app.follow);
    let picked = app.current.clone();
    app.on_net(Net::Msg(Box::new(ServerMsg::Sessions { sessions: vec![busy] })));
    assert_eq!(app.current, picked);
}

#[test]
fn costs_show_in_the_timeline() {
    let mut app = demo_app();
    let g = app.graph.clone().unwrap();
    assert!(!g.usage.is_empty(), "the demo records token usage");
    assert!(
        g.usage.iter().all(|u| u.task.is_some()),
        "every call is matched to a task"
    );
    let s = render(&mut app, 190, 50);
    assert!(s.contains("Cost"), "history has a cost column:\n{s}");
    assert!(
        s.contains(" for ") && s.contains("tokens"),
        "header shows the total:\n{s}"
    );
    // Details of a prompt break the tokens down.
    key(&mut app, KeyCode::Char('g'));
    let s = render(&mut app, 190, 50);
    assert!(s.contains("cache read") && s.contains("at API prices"), "{s}");
}
