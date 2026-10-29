//! Context-window behavior: preflight projection, typed overflow
//! classification, one durable compaction + retry per turn, refusal to retry
//! after content was forwarded, summary shrink/fallback, and restart.

#[path = "../common/mod.rs"]
mod common;
mod fixtures;
mod preflight;
mod recovery;
mod server;
mod subagent;
mod summary;
