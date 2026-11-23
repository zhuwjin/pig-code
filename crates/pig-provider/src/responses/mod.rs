//! OpenAI Responses wire format (`POST {base}/responses`): stateless
//! full-history replay (`store: false`, no `previous_response_id` — the
//! rollout/fork story requires self-contained input, same choice codex-rs
//! makes), SSE events dispatched by the payload's own `type` (the `event:`
//! line is redundant, same decision as the Anthropic path), live deltas for
//! rendering while `response.output_item.done` carries the authoritative
//! items (tool calls complete — codex-rs pattern: argument deltas are ignored
//! and the whole function_call item is taken from the done event).
//!
//! Reasoning replay is an ordered native projection: done items capture item
//! IDs, summary/content parts, and encrypted_content. Official OpenAI streaming
//! requests include encrypted reasoning when it is enabled, and same-origin
//! continuations replay it. Official store:false continuations drop plaintext-
//! only reasoning items; compatible endpoints keep the plaintext fallback.
//! Delta-only compatible streams synthesize that fallback while done items stay
//! authoritative whenever present.
//!
//! Tool results carrying images (ReadMediaFile) send `output` as an array of
//! INPUT-style items — [{input_text}, {input_image}] — the shape all three
//! audited implementations agree on (kimi-code's default lowering, the Vercel
//! AI SDK's convertFunctionToolResultOutput, codex-rs's content-items form).

mod live;
mod request;
mod search;
mod stream;
mod websocket;

#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod tests;

pub(crate) use request::build_responses_body;
pub use request::responses_web_search_tool;
pub(crate) use stream::{stream_responses, usage_fields};
pub use websocket::{RESPONSES_WS_BETA_HEADER, ResponsesWsSession, responses_ws_url};
pub(crate) use websocket::stream_responses_ws;
