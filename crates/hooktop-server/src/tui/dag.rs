//! The node-and-edge view: prompts and turns run left to right on the main
//! lane, and each Follower drops into a lane below the task that sent it.
//! A node expands to list its subtasks (tool calls) inside its box.

use std::collections::{HashMap, HashSet};

use hooktop_core::view::{self, agent_name};
use hooktop_core::{SessionGraph, Status, Task, TaskId, TaskKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::theme::Theme;
use super::ui::{FOLLOWER_ICON, ellipsize};

pub const NODE_W: u16 = 26;
const COL_GAP: u16 = 6;
const LANE_GAP: u16 = 3;
const MAX_TOOLS: usize = 12;

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: TaskId,
    pub col: u16,
    pub lane: u16,
    /// The task that spawned this one (Followers only).
    pub parent: Option<TaskId>,
    pub open: bool,
    /// Canvas position and size, filled in by [`layout`].
    pub x: u16,
    pub y: u16,
    pub h: u16,
}

#[derive(Debug, Default)]
pub struct Layout {
    pub nodes: Vec<Node>,
    pub width: u16,
    pub height: u16,
}

impl Layout {
    pub fn get(&self, id: TaskId) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// The node to the left (`dx < 0`) or right in the same lane.
    pub fn beside(&self, id: TaskId, dx: i32) -> Option<TaskId> {
        let n = self.get(id)?;
        let same = self.nodes.iter().filter(|m| m.lane == n.lane);
        if dx < 0 {
            same.filter(|m| m.col < n.col).max_by_key(|m| m.col).map(|m| m.id)
        } else {
            same.filter(|m| m.col > n.col).min_by_key(|m| m.col).map(|m| m.id)
        }
    }

    /// Down: the first Follower this node sent, else the nearest node in
    /// the next lane. Up: whoever sent this one, else the nearest above.
    pub fn vertical(&self, id: TaskId, down: bool) -> Option<TaskId> {
        let n = self.get(id)?;
        if down {
            if let Some(c) = self.nodes.iter().filter(|m| m.parent == Some(id)).min_by_key(|m| m.col) {
                return Some(c.id);
            }
        } else if let Some(p) = n.parent {
            return Some(p);
        }
        let lane = if down { n.lane + 1 } else { n.lane.checked_sub(1)? };
        self.nodes
            .iter()
            .filter(|m| m.lane == lane)
            .min_by_key(|m| (m.col as i32 - n.col as i32).abs())
            .map(|m| m.id)
    }

    pub fn first(&self) -> Option<TaskId> {
        self.nodes
            .iter()
            .filter(|n| n.lane == 0)
            .min_by_key(|n| n.col)
            .map(|n| n.id)
    }

    pub fn newest(&self) -> Option<TaskId> {
        self.nodes.iter().map(|n| n.id).max()
    }
}

fn is_notice(t: &Task) -> bool {
    t.header.kind == TaskKind::Prompt && matches!(t.header.label.as_str(), "Notification" | "System")
}

fn node_height(t: &Task, open: bool) -> u16 {
    if !open || t.tools.is_empty() {
        return 4;
    }
    let shown = t.tools.len().min(MAX_TOOLS) + usize::from(t.tools.len() > MAX_TOOLS);
    5 + shown as u16
}

/// Place every task on the canvas.
pub fn layout(g: &SessionGraph, open: &HashSet<TaskId>, hide_notices: bool) -> Layout {
    let mut nodes: Vec<Node> = Vec::new();
    let mut col = 0;
    for t in g.tasks.iter().filter(|t| t.header.kind != TaskKind::Subagent) {
        if hide_notices && is_notice(t) {
            continue;
        }
        nodes.push(Node {
            id: t.header.id,
            col,
            lane: 0,
            parent: None,
            open: open.contains(&t.header.id),
            x: 0,
            y: 0,
            h: 0,
        });
        col += 1;
    }
    // Followers: under whoever sent them, in the first free column.
    let mut next_free: HashMap<u16, u16> = HashMap::new();
    for t in g.tasks.iter().filter(|t| t.header.kind == TaskKind::Subagent) {
        let id = t.header.id;
        let parent = g
            .edges
            .iter()
            .filter(|e| e.to == id)
            .map(|e| e.from)
            .find(|from| nodes.iter().any(|n| n.id == *from));
        let (pcol, lane) = match parent.and_then(|p| nodes.iter().find(|n| n.id == p)) {
            Some(p) => (p.col, p.lane + 1),
            None => (0, 1),
        };
        let free = next_free.entry(lane).or_insert(0);
        let c = pcol.max(*free);
        *free = c + 1;
        nodes.push(Node {
            id,
            col: c,
            lane,
            parent,
            open: open.contains(&id),
            x: 0,
            y: 0,
            h: 0,
        });
    }

    let lanes = nodes.iter().map(|n| n.lane).max().map_or(0, |l| l + 1);
    let mut lane_h = vec![4u16; lanes as usize];
    for n in &mut nodes {
        n.h = node_height(&g.tasks[n.id as usize], n.open);
        let h = &mut lane_h[n.lane as usize];
        *h = (*h).max(n.h);
    }
    let mut lane_y = vec![0u16; lanes as usize];
    for l in 1..lanes as usize {
        lane_y[l] = lane_y[l - 1] + lane_h[l - 1] + LANE_GAP;
    }
    let mut width = 0;
    for n in &mut nodes {
        n.x = 1 + n.col * (NODE_W + COL_GAP);
        n.y = lane_y[n.lane as usize];
        width = width.max(n.x + NODE_W + 1);
    }
    let height = lanes
        .checked_sub(1)
        .map_or(0, |l| lane_y[l as usize] + lane_h[l as usize]);
    Layout { nodes, width, height }
}

/// A clipped drawing surface: canvas coordinates in, screen cells out.
struct Canvas<'a> {
    buf: &'a mut Buffer,
    area: Rect,
    sx: u16,
    sy: u16,
}

impl Canvas<'_> {
    fn put(&mut self, x: u16, y: u16, s: &str, style: Style) {
        if y < self.sy || y - self.sy >= self.area.height {
            return;
        }
        let row = self.area.y + (y - self.sy);
        let mut cx = x as i32 - self.sx as i32;
        for g in s.chars() {
            let w = unicode_width::UnicodeWidthChar::width(g).unwrap_or(0) as i32;
            if cx >= 0 && cx + w <= self.area.width as i32 {
                let mut tmp = [0u8; 4];
                self.buf
                    .set_stringn(self.area.x + cx as u16, row, g.encode_utf8(&mut tmp), w as usize, style);
            }
            cx += w;
        }
    }

    fn hline(&mut self, x0: u16, x1: u16, y: u16, s: &str, style: Style) {
        for x in x0..x1 {
            self.put(x, y, s, style);
        }
    }

    fn vline(&mut self, x: u16, y0: u16, y1: u16, style: Style) {
        for y in y0..y1 {
            self.put(x, y, "│", style);
        }
    }
}

fn status_glyph(t: &Theme, s: Status) -> (&'static str, Style) {
    match s {
        Status::Running => ("●", Style::new().fg(t.accent)),
        Status::Done => ("✓", Style::new().fg(t.ok)),
        Status::Failed => ("✗", Style::new().fg(t.err)),
        Status::Interrupted => ("◌", t.faint()),
    }
}

fn tone(t: &Theme, task: &Task) -> ratatui::style::Color {
    match task.header.status {
        Status::Failed => t.err,
        Status::Running => t.accent,
        _ => match task.header.kind {
            TaskKind::Prompt if is_notice(task) => t.faint,
            TaskKind::Prompt => t.prompt,
            TaskKind::Turn => t.turn,
            TaskKind::Subagent => t.sub,
        },
    }
}

fn heading(task: &Task) -> String {
    let h = &task.header;
    match h.kind {
        TaskKind::Prompt => match h.label.as_str() {
            "Command" => "/ Command".into(),
            "Prompt" => "❯ Prompt".into(),
            other => other.to_string(),
        },
        TaskKind::Turn => h.label.clone(),
        TaskKind::Subagent => format!("{FOLLOWER_ICON} {}", agent_name(h.agent_type.as_deref())),
    }
}

/// Draw the graph into `area`, scrolled so the canvas point (`sx`, `sy`)
/// is the top-left corner.
#[allow(clippy::too_many_arguments)]
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    t: &Theme,
    g: &SessionGraph,
    lay: &Layout,
    selected: Option<TaskId>,
    (sx, sy): (u16, u16),
    now: i64,
) {
    let mut c = Canvas { buf, area, sx, sy };
    let edge = Style::new().fg(t.line);

    // Edges first so boxes sit on top. Main lane: left to right.
    let mut main: Vec<&Node> = lay.nodes.iter().filter(|n| n.lane == 0).collect();
    main.sort_by_key(|n| n.col);
    for w in main.windows(2) {
        let (a, b) = (w[0], w[1]);
        let y = a.y + 1;
        c.hline(a.x + NODE_W, b.x - 1, y, "─", edge);
        c.put(b.x - 1, y, "▶", edge);
    }
    // Spawns: down from the sender, across, into the Follower.
    let spawns: Vec<(&Node, &Node)> = lay
        .nodes
        .iter()
        .filter_map(|n| Some((lay.get(n.parent?)?, n)))
        .collect();
    for (p, n) in &spawns {
        let x0 = p.x + 3;
        let x1 = n.x + 3;
        let ym = n.y - 1;
        let follower = Style::new().fg(t.sub);
        c.vline(x0, p.y + p.h, ym, follower);
        if x1 > x0 {
            c.hline(x0 + 1, x1, ym, "─", follower);
            c.put(x0, ym, "╰", follower);
        }
    }
    for (p, n) in &spawns {
        c.put(n.x + 3, n.y - 1, "▼", Style::new().fg(t.sub));
        c.put(p.x + 3, p.y + p.h - 1, "┬", Style::new().fg(t.sub));
    }

    for n in &lay.nodes {
        let task = &g.tasks[n.id as usize];
        let sel = selected == Some(n.id);
        let col = tone(t, task);
        let border = if sel {
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(col)
        };
        let (tl, tr, bl, br, hz, vt) = if sel {
            ("┏", "┓", "┗", "┛", "━", "┃")
        } else {
            ("╭", "╮", "╰", "╯", "─", "│")
        };
        let (x, y, w, h) = (n.x, n.y, NODE_W, n.h);
        c.put(x, y, tl, border);
        c.hline(x + 1, x + w - 1, y, hz, border);
        c.put(x + w - 1, y, tr, border);
        for yy in y + 1..y + h - 1 {
            c.put(x, yy, vt, border);
            c.hline(x + 1, x + w - 1, yy, " ", Style::new());
            c.put(x + w - 1, yy, vt, border);
        }
        c.put(x, y + h - 1, bl, border);
        c.hline(x + 1, x + w - 1, y + h - 1, hz, border);
        c.put(x + w - 1, y + h - 1, br, border);
        // Keep spawn connectors visible on the sender's bottom edge.
        if lay.nodes.iter().any(|m| m.parent == Some(n.id)) {
            c.put(x + 3, y + h - 1, "┬", Style::new().fg(t.sub));
        }

        let inner = (w - 4) as usize;
        let h0 = &task.header;
        let fold = if task.tools.is_empty() {
            " "
        } else if n.open {
            "▾"
        } else {
            "▸"
        };
        let (glyph, gstyle) = status_glyph(t, h0.status);
        let end = h0.ended_ms.unwrap_or(if h0.status == Status::Running {
            now
        } else {
            h0.started_ms
        });
        let dur = view::duration(end - h0.started_ms);
        let head = ellipsize(&heading(task), inner.saturating_sub(dur.len() + 5));
        c.put(x + 2, y + 1, fold, t.faint());
        c.put(x + 4, y + 1, &head, Style::new().fg(col).add_modifier(Modifier::BOLD));
        let right = x + w - 2 - dur.len() as u16 - 2;
        c.put(right, y + 1, &dur, t.muted());
        c.put(x + w - 3, y + 1, glyph, gstyle);
        c.put(x + 2, y + 2, &ellipsize(&h0.title, inner), t.muted());

        if n.open && !task.tools.is_empty() {
            c.put(x, y + 3, if sel { "┣" } else { "├" }, border);
            c.hline(x + 1, x + w - 1, y + 3, "┄", Style::new().fg(t.line));
            c.put(x + w - 1, y + 3, if sel { "┫" } else { "┤" }, border);
            for (i, call) in task.tools.iter().take(MAX_TOOLS).enumerate() {
                let yy = y + 4 + i as u16;
                let (g2, s2) = status_glyph(t, call.status);
                c.put(x + 2, yy, "●", Style::new().fg(t.tool(&call.name)));
                let name = view::tool_name(&call.name);
                c.put(x + 4, yy, name, Style::new().add_modifier(Modifier::BOLD));
                let used = name.chars().count() + 1;
                let room = inner.saturating_sub(used + 4);
                let target = match call.spawned_task.and_then(|s| g.task(s)) {
                    Some(sub) => format!("{FOLLOWER_ICON} {}", agent_name(sub.header.agent_type.as_deref())),
                    None => view::first_line(&call.target).to_string(),
                };
                c.put(x + 4 + used as u16, yy, &ellipsize(&target, room), t.muted());
                c.put(x + w - 3, yy, g2, s2);
            }
            if task.tools.len() > MAX_TOOLS {
                let yy = y + 4 + MAX_TOOLS as u16;
                c.put(
                    x + 2,
                    yy,
                    &format!("… {} more", task.tools.len() - MAX_TOOLS),
                    t.faint(),
                );
            }
        }
    }
}

/// Scroll so `node` is fully on screen, moving as little as possible.
pub fn scroll_to(node: &Node, view: Rect, (sx, sy): (u16, u16)) -> (u16, u16) {
    let fit = |pos: u16, len: u16, start: u16, size: u16| -> u16 {
        if size == 0 {
            return start;
        }
        if pos < start {
            pos.saturating_sub(1)
        } else if pos + len > start + size {
            (pos + len + 1).saturating_sub(size).min(pos.saturating_sub(1))
        } else {
            start
        }
    };
    let top = node.y.saturating_sub(1); // room for an incoming arrow
    (
        fit(node.x, NODE_W, sx, view.width),
        fit(top, node.h + 1, sy, view.height),
    )
}
