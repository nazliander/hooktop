# hooktop-core

The event model, task graph, token pricing and statistics behind [hooktop](https://github.com/nazliander/hooktop), a live terminal view of Claude Code's tool calls, subagents, CPU/RAM use and costs.

It has no I/O: it turns Claude Code hook payloads into a `SessionGraph` and computes stats from it. Most people want the `hooktop` binary instead:

```sh
cargo install --locked hooktop
```

Licensed under Apache-2.0.
