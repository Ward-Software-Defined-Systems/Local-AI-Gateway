//! Reverse proxy in front of LM Studio and Ollama.
//!
//! One listener per backend forwards every request byte-for-byte. Inference requests are buffered (capped),
//! scanned, optionally rewritten (`stream_options.include_usage`) and forwarded; their responses are teed
//! through a format-specific `StreamParser` for token counts, timings and the reply text, captured for the
//! inspector, and streamed to the client with backpressure. Everything else, WebSocket upgrades included,
//! passes through untouched. Ported and generalised from Ollama-Monitor's `proxy.rs` and `parser.rs`.

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
    BackendUpstream, CapturePolicy, ListenerConfig, ListenerMode, ProxyContext, ProxyState,
    TargetFlags, bind, serve,
};
pub use scan::{Decision, NoopScanner, RequestScanner, ScanContext, ScanOutcome};

#[cfg(test)]
mod tests;
