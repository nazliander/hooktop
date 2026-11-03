# Changelog

Add a line under **Unreleased** with each change worth telling users about. `packaging/release.sh <version>` turns that section into the release, and the release workflow uses it as the GitHub release notes.

## Unreleased

## 0.1.4 - 2026-11-03

- Statistics rows open what's behind them: `Tab` picks a panel, `Enter` shows every call of a tool, the longest thinking stretches with what came before and after, where a model's cost went, or a call's full details, and `Enter` again finds it in the Timeline
- A simpler top bar: the tabs and the link status. The port, mode and version moved into the help (`?`)

## 0.1.3 - 2026-11-03

- Tool calls that never got a finish (Esc, a denied permission, a follower cut off) stop when their turn or follower ends, instead of growing for hours. Sessions already recorded are corrected when the server restarts
- CPU is measured from each process's CPU time and capped at the machine's cores, so no more 4000% spikes, and it reads as cores (`4.0 cores`)
- CPU and RAM tiles show the latest reading while Claude works and the peak once it stops
- Statistics: where the time went (thinking, tools, waiting for you), one Tools panel with calls and time, repeated commands grouped as `×N`, and `Agent` calls left out of the slowest list
- `automated_prompts` in the new `config.json` (or `--automated-prompt`) keeps prompts from your own tooling out of your prompt counts

## 0.1.2 - 2026-11-03

- CPU/RAM rankings mark readings that aren't a call's alone: `≈` shared (another call ran at the same time) and `↳` borrowed (the call was too short for a reading, so it uses the next one)
- A little padding inside panels, between panes and at the window's edges
- The slowest-calls table keeps call names readable in narrow terminals
- README images and links work on crates.io

## 0.1.1 - 2026-11-02

- Published to crates.io: `cargo install --locked hooktop`
- `hooktop --version` shows the build commit for crates.io installs
- Releases publish to GitHub, Homebrew and crates.io together

## 0.1.0 - 2026-11-02

- First release: live timeline, graph and statistics of Claude Code's tool calls, subagents, CPU/RAM and token costs
