//! Core types for hooktop: Claude Code hook payloads, the normalized event
//! stream, and the per-session task graph that the UI renders.
//!
//! Data flow: [`HookPayload`] (raw JSON from Claude Code) → [`normalize`] →
//! [`Event`] → [`SessionGraph::apply`] → a list of [`Delta`]s that clients
//! apply to their copy of the graph.

pub mod event;
pub mod graph;
pub mod prompt;
pub mod stats;
mod summary;
pub mod view;

pub use event::{Event, EventKind, HookPayload, SAMPLE_HOOK, Sample, normalize};
pub use graph::{
    Delta, Edge, SessionGraph, SessionStatus, SessionSummary, Status, Task, TaskHeader, TaskId, TaskKind, ToolCall,
};
