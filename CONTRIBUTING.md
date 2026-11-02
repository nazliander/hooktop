# Contributing

Thanks for wanting to help with hooktop.

## Getting started

You need Rust 1.88 or newer.

```sh
cargo test                                   # everything
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo run -- --demo --no-store               # the TUI against a made-up session
```

`--demo` needs no Claude Code at all. To try your build against real sessions, run `cargo run -- hooks install` (it backs up `~/.claude/settings.json` first), then `cargo run`.

## Where things live

- `crates/hooktop-core`: hook payloads, the session graph, the view model and statistics. Pure code, no I/O.
- `crates/hooktop-server`: the `hooktop` binary: server, storage, CPU/RAM sampling, security checks and the terminal UI.
- `docs/architecture`: how it all fits together. Please update it when you change how something works.

## Pull requests

- Keep each one to one change, with a test where it makes sense. The TUI has rendering tests with ratatui's `TestBackend`.
- CI runs formatting, clippy, the tests, and a build on Rust 1.88.
- Never commit real session data, tokens or `settings.json` files. Use the demo session or made-up payloads in tests.

## Reporting bugs

Open an issue with `hooktop --version`, your OS and terminal, and what you expected. For security problems, see [SECURITY.md](SECURITY.md) instead.
