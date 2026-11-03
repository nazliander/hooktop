# Terminal UI

The TUI lives in `hooktop-server/src/tui/` and is built on ratatui with the crossterm backend.

| Module | Role |
|---|---|
| `mod.rs` | Startup (attach or embed), the event loop |
| `client.rs` | WebSocket client with reconnects; the `ServerMsg` types |
| `app.rs` | All state and key handling, with no terminal I/O, so it's unit-tested directly |
| `ui.rs` | Drawing: title bar, connections, the three tabs, details, help |
| `dag.rs` | Graph view layout, drawing and navigation |
| `theme.rs` | Ayu Dark and Ayu Light, with a 256-colour fallback |

## Attach or embed

```mermaid
flowchart TD
    start(["hooktop"]) --> tty{"stdout is a terminal?"}
    tty -- no --> serve["run as hooktop serve"]
    tty -- yes --> port{"something listening on :4317?"}
    port -- yes --> attach["attach: TUI only<br/>(--demo is ignored)"]
    port -- no --> embed["embed: start the server in-process,<br/>stop it on quit"]
    attach --> ws["connect ws://127.0.0.1:4317/ws"]
    embed --> ws
```

Either way the TUI is a plain WebSocket client, so it behaves the same against `hooktop serve`, `brew services` or its own embedded server. It sends the token from `<data dir>/hooktop/token` (or `HOOKTOP_TOKEN`), and at startup checks that Claude Code's hooks carry that token, showing a footer warning if not. Logs go to `hooktop.log` next to the event store, never to the terminal.

## Event loop

```mermaid
flowchart LR
    IN["input thread<br/>crossterm poll/read"] -- "AppEvent::Term" --> CH(("mpsc"))
    NET["WS client task"] -- "AppEvent::Net" --> CH
    TICK["250 ms tick"] --> LOOP
    CH --> LOOP["event loop"]
    LOOP -- "on_key / on_net" --> APP["App"]
    LOOP -- "outbox" --> NET
    LOOP -- "draw" --> UI["ui::draw"]
```

Each iteration draws once, then waits for an event or the tick. Everything already queued is drained before the next draw, so a burst of updates costs one frame. The tick keeps live durations moving. Messages for the server (subscribe, remove) are queued in `App::outbox` and sent by the loop.

## State worth knowing

- **Follow.** On by default. It switches to the connection Claude is working in when the current one isn't working, keeps the history cursor on the newest row, and selects the newest node in the graph. Picking a connection by hand, or moving the cursor up, turns it off; `f` or `G` turns it back on.
- **Auto-expand.** The running prompt and turn, and the latest of each, stay open unless you folded them by hand. Ones hooktop opened itself close again once they're done.
- **Stable cursor.** Rows have keys (`t<task>` or `c<task>:<tool_use_id>`), so the cursor stays on the same row as new rows arrive.
- **Confirmations.** `x` and `X` set `App::confirm`; the footer asks, and only `y` or Enter sends the removal.

## Timeline tab

- **CPU and RAM:** ratatui `Chart`s with braille line datasets over the session's span. Two extra vertical datasets mark the selected row's start and end.
- **Party:** a `List` from `view::party`.
- **History:** a `Table` from `view::history_rows`. The timeline column takes about 45% of the spare width, under an axis of `0s`, the midpoint and the end. Bars use the tool's colour and turn red on failure.

## Graph tab

`dag::layout` places nodes on a grid:

- Prompts and turns go on lane 0, one column each, in creation order (notifications are skipped while hidden).
- A Follower goes one lane below whoever spawned it, at the spawner's column or the next free column in that lane, so siblings sit side by side.
- Lane heights are the tallest node in the lane; a node opened with Enter grows by one row per tool call (up to 12, then "… n more").
- Nodes are 26 cells wide with 6-cell gaps, and lanes are 3 rows apart.

`dag::render` draws onto the frame buffer through a clipped canvas with a scroll offset. Edges are drawn first and boxes on top: `─▶` between main-lane nodes, and `│ ╰─▼` from a spawner's bottom edge to its Follower. The selected node gets a heavy border, and the view scrolls just enough to keep it fully visible.

Navigation: ←/→ move along a lane, ↓ goes to the first Follower a node sent (or the nearest node below), ↑ to the spawner (or the nearest node above).

## Statistics tab

Drawn from `hooktop_core::stats::stats_with(g, now, 8, pricing, options)`, where the options carry the automated-prompt prefixes from `config.json` and `--automated-prompt`.

- **Tiles** (six on wide terminals, four on narrow). While Claude works, the CPU and RAM tiles show the latest reading with the peak underneath; once it stops, they show the peak with the average underneath, since the last reading is stale. CPU is in cores. Prompts leaves out automated ones and counts them underneath.
- **Tools · calls and time**: one bar per tool by calls, with total time and failures.
- **Where the time went** splits the wall time three ways, which add up to it. *Tools* is the time at least one tool call ran (the union, so parallel calls count once; calls that start a Follower don't count, the Follower's own calls do). *Thinking* is the rest of the time a prompt, turn or Follower was running. *Waiting* is everything else.
- **Rankings** (slowest calls, RAM eaters, CPU hogs) group calls with the same tool and target into one row with a `×N` count, showing the biggest. The slowest list leaves out `Agent`/`Task` calls: their time is the Follower's whole run, shown under Followers. RAM and CPU rows carry the `≈` shared and `↳` borrowed marks described in [resources.md](resources.md).
- Bar lists use eighth-block precision; the rankings are tables.

### Drilling in

`Tab` / `←→` move between panels (`drill::Panel`, in reading order), `↑↓` between rows. `Enter` opens a `drill::Drill` over the tab: facts, a note when the numbers need explaining, and a list of items. `Enter` on an item calls `App::jump_to`, which opens the item's prompt and turn (or switches to its Follower), selects its row in the Timeline and keeps follow off; `s` goes back. Drills are built from `Stats` on every frame, so they stay live.

| Panel | Opens |
|---|---|
| Tools | every call of that tool, slowest first |
| Where the time went | *Thinking*: the longest stretches with no tool running (`Stats::thinking`), each with the same agent's call before and after it and the tokens and cost of model calls that ended inside it. *Tools*: the slowest calls. *Waiting*: the longest pauses before a prompt (`Stats::waiting`). |
| Cost by model | the prompts, turns and Followers that used the model, most expensive first |
| Slowest, RAM eaters, CPU hogs | the call: who, where, status, times, CPU/RAM with the reading's mark explained, the turn's cost, its error or output; and every run of the same command |
| Longest prompts | the prompt's turns |
| Followers | the Follower's calls |

hooktop only sees the timing and token counts of the model's work, never its text, so a thinking stretch shows where and how much, not what.

A call still marked running counts up to now only while the session is working. In a session that's waiting or closed, it stops at the last activity; the graph also closes such calls as interrupted when their turn or Follower ends (see [events-and-graph.md](events-and-graph.md)).

## Themes

Two themes, Ayu Dark and Ayu Light, from the official ayu palette; `t` switches between them. The background is never painted, so the terminal's own shows through. Ayu Light is the default when the terminal reports a light background (`COLORFGBG`), Ayu Dark otherwise; `--theme` or `HOOKTOP_THEME` chooses. Terminals without 24-bit colour (no `COLORTERM=truecolor`) get each ayu colour as its nearest match in the standard 256-colour palette, so the look carries over.

## Tests

`tui/tests.rs` drives `App` with fake server messages and key presses, and renders frames with ratatui's `TestBackend` at several sizes. The demo session (`demo::demo_graph`) gives a deterministic graph with synthetic samples.
