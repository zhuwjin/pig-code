//! A provider simulating OpenAI Chat Completions + SSE, behavior-aligned with
//! GLM/DeepSeek: the first request returns a Read tool call (arguments in
//! chunks); once tool results are present, it returns reasoning_content + a
//! Markdown text stream. Reused by integration tests, examples, and GUI
//! self-testing.
//!
//! The module is split by responsibility while preserving every public
//! `pig_provider::mock::*` path: `server` owns the HTTP endpoint and dispatch,
//! `scenarios` the trigger vocabulary and canned replies, `streaming` the
//! SSE/event frame builders, and `validation` the request-shape negative
//! oracle.

mod scenarios;
mod server;
mod streaming;
mod validation;

pub use scenarios::*;
pub use server::*;
pub use validation::*;
