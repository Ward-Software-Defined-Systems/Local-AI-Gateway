//! Reverse proxy in front of LM Studio and Ollama, and the gateway to Anthropic, Gemini and OpenAI.
//!
//! A fixed listener forwards every request to one backend; the unified listener picks the backend per
//! request (`route`). Inference requests are buffered (capped), scanned, optionally rewritten
//! (`stream_options.include_usage`, and on the unified listener the model an alias resolved to) and
//! forwarded; their responses are teed through a format-specific `StreamParser` for token counts, timings
//! and the reply text, captured for the inspector, and streamed to the client with backpressure. Everything
//! else passes through untouched, except that a body bound for a cloud provider is always scanned first.
//! Ported and generalised from Ollama-Monitor's `proxy.rs` and `parser.rs`.

pub mod capture;
pub mod error;
pub mod http;
pub mod listener;
pub mod parsers;
pub mod pipeline;
pub mod rejection;
pub mod route;
pub mod router;
pub mod scan;
pub mod tunnel;

pub use listener::{
    BackendUpstream, CapturePolicy, GATEWAY_HEADER, GATEWAY_VERSION, ListenerConfig, ListenerMode,
    ProxyContext, ProxyState, TargetFlags, bind, serve,
};
pub use scan::{Decision, NoopScanner, RequestScanner, ScanContext, ScanOutcome};

#[cfg(test)]
mod tests;
