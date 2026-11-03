# Changelog

Add a line under **Unreleased** with each change worth telling users about. `packaging/release.sh <version>` turns that section into the release, and the release workflow uses it as the GitHub release notes.

## Unreleased

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
