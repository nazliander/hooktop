# hooktop

<img src="docs/knight.png" width="96" align="right" alt="A pixel-art knight, hooktop's mascot">

**See what Claude Code is really doing: every tool call, every subagent, and what it costs your machine, live in your terminal.**

hooktop is for developers who code with agents and want to look under the hood. When Claude Code runs, it reads files, greps, edits, runs your tests and sends off subagents, often faster than you can follow. hooktop shows all of it as it happens, in a nutshell:

- **Tool calls:** which tools Claude used, on what, in what order, and how long each one took.
- **Subagents:** who Claude sent off to do what, and how their work fits into the session.
- **Hardware:** the CPU and memory used by Claude Code and everything it started, matched to the tool call that caused it.

It's one small Rust binary that runs on your machine. Nothing leaves your computer.

![hooktop's timeline: CPU and RAM charts over a session's history](docs/timeline.png)

## Quick start

```sh
brew install nazliander/tap/hooktop   # or, from source: cargo install --locked --path crates/hooktop-server
hooktop hooks install                 # point Claude Code at hooktop (backs up settings.json first)
hooktop                               # open the terminal UI
```

Start `claude` in another terminal and the session shows up on the left. To look around first, run `hooktop --demo`.

Building from source needs Rust 1.88 or newer.

## The cast

- **🦊 Watcher** is the Boss: Claude's main agent.
- **♞ Followers** are the subagents Watcher sends out (Explorer, Planner, Squire, …).
- **Connections** are your Claude Code sessions: open while Claude works or waits for you, and closed when the session ends or after an hour of quiet. A closed connection reopens if Claude picks it up again.

## How it works

```mermaid
flowchart LR
    CC["Claude Code"] -- "hooks" --> S["hooktop server<br/>:4317"]
    SM["CPU/RAM sampler"] -. "reads" .-> CC
    SM --> S
    S --> DB[("SQLite<br/>event log")]
    S -- "live updates" --> TUI["Terminal UI"]
```

Claude Code sends each event (a prompt, a tool call, a subagent starting or stopping) to hooktop. hooktop builds a graph per session, saves the raw events, and measures the CPU and memory of Claude Code and everything it starts. The terminal UI shows it all live. The technical details are in [docs/architecture](docs/architecture/README.md).

## Three tabs

Press `o` to move between them.

- **Timeline:** CPU and RAM charts, the party (Watcher and its Followers), and the history of prompts, turns and tool calls on one time axis.
- **Graph:** tasks as nodes and edges; Enter on a node lists its tool calls.
- **Statistics:** totals, the slowest tool calls and prompts, the biggest RAM and CPU users, and how long each Follower ran.

| Graph | Statistics |
|---|---|
| ![Graph view](docs/graph.png) | ![Statistics view](docs/stats.png) |

## Keys

| Key | What it does |
|---|---|
| `o` / `O`, `s` | Next / previous tab, Statistics |
| `↑ ↓ → ←` or `j k l h` | Move, open and close |
| `Enter` | Open a row or node; on a tool call that sent a Follower, jump to it |
| `Esc` | Back to Watcher |
| `f` | Follow the connection Claude is working in |
| `x` / `X` | Remove this connection / all closed and quiet ones (asks first) |
| `e` / `c` | Open / close everything |
| `n` | Show or hide notifications |
| `d`, `[` | Show or hide details, connections |
| `t` | Theme: Ayu Dark, Mirage, Light, Terminal |
| `?` / `q` | Help / quit |

## Commands

| Command | What it does |
|---|---|
| `hooktop` | The terminal UI (attaches to a running hooktop, or runs one while open) |
| `hooktop serve` | Server only, to record in the background. Stops after 1 hour unused (`--idle-timeout 30m`, or `off`) |
| `hooktop --close-after 30m` | Close quiet connections sooner (default 1 hour, `off` to keep them open); works for `hooktop serve` too |
| `brew services start hooktop` | Runs the server at login and keeps it running |
| `hooktop --demo` | Play a made-up session |
| `hooktop hooks install` / `uninstall` | Add or remove hooktop's hooks |
| `hooktop export <id>` / `hooktop serve --replay <file>` | Export a session as JSONL / play one back |
| `hooktop token rotate` | Make a new token; a running hooktop and your hooks switch over by themselves |
| `hooktop reset` | Delete stored history (stop hooktop first) |

Run `hooktop --help` for every flag.

## Security

hooktop only listens on your own machine, and every request needs a secret token that `hooktop hooks install` gives to Claude Code. Requests from web pages are refused, so a site you visit can't read your sessions or add fake ones. hooktop's files are readable only by you. Details are in [docs/architecture/security.md](docs/architecture/security.md).

## Contributing

Issues and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). Please report security problems privately, as described in [SECURITY.md](SECURITY.md).

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo run -- --demo --no-store
```

To release, bump `version` in `Cargo.toml` and push a matching tag (`git tag v0.1.0 && git push origin v0.1.0`). GitHub Actions builds macOS and Linux binaries and updates the Homebrew tap.

## License

The code is Apache-2.0: see [LICENSE](LICENSE) and [NOTICE](NOTICE). The knight artwork is © Nazli Ander, licensed [CC BY 4.0](docs/ARTWORK.md).
