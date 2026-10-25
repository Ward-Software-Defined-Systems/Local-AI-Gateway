//! Stream parsers: one per API format, fed the upstream response chunk by chunk, producing token counts,
//! timings, the stop reason and the reassembled reply text. A parser never changes what the client
//! receives and never fails a request; anything it cannot parse ends as `None` or an estimate.
//!
//! Formats with engine timings (Ollama's NDJSON terminal line, LM Studio's `stats`) report them as
//! authoritative. The others get the gateway's wall clock: TTFT is the time to the first response byte,
//! and tokens per second divides the generated tokens by the window from that byte to the end, floored at
//! 50 ms. Those are flagged `timings_estimated`. Token counts are flagged `tokens_estimated` when they are
//! inferred from chunk counts because the server sent no usage.

pub mod anthropic;
pub mod embeddings;
pub mod gemini;
pub mod line_buffer;
pub mod ollama;
pub mod openai;
pub mod raw;
pub mod responses;
pub mod sse_frames;

use std::time::Instant;

use gateway_core::types::ApiFormat;

/// Reassembled reply text kept per response, for scanning and capture.
pub const TEXT_CAP: usize = 4 * 1024 * 1024;
/// A non-streaming body is buffered up to this for parsing.
pub const SINGLE_BODY_CAP: usize = 64 * 1024 * 1024;
/// The generation window never reads shorter than this, so a two-token reply has a sane rate.
pub const MIN_GEN_WINDOW_MS: f64 = 50.0;

/// The gateway's view of a response's timing.
#[derive(Debug, Clone, Copy)]
pub struct WallClock {
    pub started: Instant,
    pub first_byte: Option<Instant>,
    pub ended: Instant,
}

impl WallClock {
    pub fn ttft_ms(&self) -> Option<f64> {
        self.first_byte
            .map(|t| t.saturating_duration_since(self.started).as_secs_f64() * 1000.0)
    }

    pub fn total_ms(&self) -> f64 {
        self.ended
            .saturating_duration_since(self.started)
            .as_secs_f64()
            * 1000.0
    }

    pub fn gen_window_ms(&self) -> f64 {
        let from = self.first_byte.unwrap_or(self.started);
        (self.ended.saturating_duration_since(from).as_secs_f64() * 1000.0).max(MIN_GEN_WINDOW_MS)
    }
}

/// What a parser knows at the end of a response.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Parsed {
    pub model: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub gen_tokens: Option<u64>,
    pub tokens_estimated: bool,
    pub ttft_ms: Option<f64>,
    pub total_ms: f64,
    pub tokens_per_sec: Option<f64>,
    pub timings_estimated: bool,
    pub stop_reason: Option<String>,
    /// An error the server sent inside a 2xx body or stream.
    pub in_stream_error: Option<String>,
    /// An Ollama model load or unload call, not an inference.
    pub is_load_or_unload: bool,
    pub text: String,
    pub text_truncated: bool,
}

impl Parsed {
    /// Fills the timing fields from the wall clock for formats without engine timings.
    pub fn with_wall_timings(mut self, wall: &WallClock, streamed: bool) -> Self {
        let total = wall.total_ms();
        let generated = self.gen_tokens.filter(|g| *g > 0).map(|g| g as f64);
        if streamed {
            self.ttft_ms = wall.ttft_ms();
            self.tokens_per_sec = generated.map(|g| g / (wall.gen_window_ms() / 1000.0));
        } else {
            self.ttft_ms = None;
            self.tokens_per_sec = generated.map(|g| g / (total.max(1.0) / 1000.0));
        }
        self.total_ms = total;
        self.timings_estimated = true;
        self
    }
}

pub trait StreamParser: Send {
    fn push(&mut self, chunk: &[u8]);
    /// Reply text that arrived since the last call, for incremental scanning.
    fn take_text_deltas(&mut self) -> Vec<String>;
    /// The stream has logically finished but the frame with the token counts has not arrived: the proxy
    /// keeps draining upstream briefly when the client hangs up here.
    fn awaiting_trailer(&self) -> bool;
    fn finalize(self: Box<Self>, wall: &WallClock) -> Option<Parsed>;
}

/// Reply text accumulator shared by the parsers: the full text up to `TEXT_CAP`, plus the deltas not yet
/// handed to the scanner.
#[derive(Debug, Default)]
pub struct TextSink {
    text: String,
    truncated: bool,
    pending: Vec<String>,
    pending_bytes: usize,
}

impl TextSink {
    pub fn push(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if self.text.len() + delta.len() <= TEXT_CAP {
            self.text.push_str(delta);
        } else {
            self.truncated = true;
        }
        if self.pending_bytes + delta.len() <= TEXT_CAP {
            self.pending_bytes += delta.len();
            self.pending.push(delta.to_string());
        }
    }

    pub fn take(&mut self) -> Vec<String> {
        self.pending_bytes = 0;
        std::mem::take(&mut self.pending)
    }

    pub fn into_parts(self) -> (String, bool) {
        (self.text, self.truncated)
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Picks the parser for a 2xx response from the request's format, the response `Content-Type` and whether
/// the request asked to stream (the fallback when the content type says nothing).
pub fn select(
    format: ApiFormat,
    content_type: Option<&str>,
    request_streamed: bool,
) -> Option<Box<dyn StreamParser>> {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let sse = ct.contains("text/event-stream");
    let ndjson = ct.contains("application/x-ndjson") || ct.contains("application/jsonl");
    let json = ct.contains("application/json");
    let streamed = sse || (!json && request_streamed);
    Some(match format {
        ApiFormat::Passthrough => return None,
        // Scanned, stored, never interpreted: the raw parser only keeps the text for the DLP scan.
        ApiFormat::CloudPassthrough => Box::new(raw::RawTextParser::new()),
        // Ollama streams by default: NDJSON or no content type means a stream; plain JSON is one object.
        ApiFormat::OllamaNative => Box::new(ollama::OllamaParser::new(ndjson || !json)),
        ApiFormat::OllamaEmbeddings => Box::new(embeddings::EmbeddingsParser::new(
            embeddings::Flavor::Ollama,
        )),
        ApiFormat::OpenAiChat => {
            Box::new(openai::OpenAiParser::new(openai::Flavor::Chat, streamed))
        }
        ApiFormat::OpenAiCompletions => Box::new(openai::OpenAiParser::new(
            openai::Flavor::Completions,
            streamed,
        )),
        ApiFormat::LmStudioNative => Box::new(openai::OpenAiParser::new(
            openai::Flavor::LmStudio,
            streamed,
        )),
        ApiFormat::OpenAiEmbeddings | ApiFormat::LmStudioEmbeddings => Box::new(
            embeddings::EmbeddingsParser::new(embeddings::Flavor::OpenAi),
        ),
        ApiFormat::OpenAiResponses => Box::new(responses::ResponsesParser::new(streamed)),
        ApiFormat::AnthropicMessages => Box::new(anthropic::AnthropicParser::new(streamed)),
        // `streamGenerateContent` without `alt=sse` is a JSON array written chunk by chunk.
        ApiFormat::GeminiGenerate => Box::new(gemini::GeminiParser::new(if sse {
            gemini::Mode::Sse
        } else if request_streamed {
            gemini::Mode::Array
        } else {
            gemini::Mode::Single
        })),
        ApiFormat::GeminiEmbed => Box::new(embeddings::EmbeddingsParser::new(
            embeddings::Flavor::Gemini,
        )),
    })
}

/// Test helper: feeds `body` in small chunks and finalises with a synthetic clock.
#[cfg(test)]
pub(crate) fn parse_all(
    mut parser: Box<dyn StreamParser>,
    body: &[u8],
    chunk: usize,
) -> Option<Parsed> {
    let started = Instant::now();
    for piece in body.chunks(chunk.max(1)) {
        parser.push(piece);
    }
    let wall = WallClock {
        started,
        first_byte: Some(started + std::time::Duration::from_millis(100)),
        ended: started + std::time::Duration::from_millis(1100),
    };
    parser.finalize(&wall)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_clock_floors_the_generation_window_and_measures_ttft() {
        let started = Instant::now();
        let wall = WallClock {
            started,
            first_byte: Some(started + std::time::Duration::from_millis(250)),
            ended: started + std::time::Duration::from_millis(260),
        };
        assert!((wall.ttft_ms().unwrap() - 250.0).abs() < 1e-6);
        assert_eq!(wall.gen_window_ms(), MIN_GEN_WINDOW_MS);
        let parsed = Parsed {
            gen_tokens: Some(10),
            ..Default::default()
        }
        .with_wall_timings(&wall, true);
        assert!(parsed.timings_estimated);
        assert!(
            (parsed.tokens_per_sec.unwrap() - 200.0).abs() < 1e-6,
            "10 tokens over the 50 ms floor"
        );
        let single = Parsed {
            gen_tokens: Some(26),
            ..Default::default()
        }
        .with_wall_timings(&wall, false);
        assert_eq!(
            single.ttft_ms, None,
            "non-streaming has no first-token time"
        );
        assert!(
            (single.tokens_per_sec.unwrap() - 100.0).abs() < 1e-6,
            "26 tokens over 260 ms end to end"
        );
    }

    #[test]
    fn the_text_sink_caps_and_hands_out_deltas_once() {
        let mut sink = TextSink::default();
        sink.push("hel");
        sink.push("lo");
        assert_eq!(sink.take(), vec!["hel".to_string(), "lo".to_string()]);
        assert!(sink.take().is_empty());
        let (text, truncated) = sink.into_parts();
        assert_eq!(text, "hello");
        assert!(!truncated);
    }

    #[test]
    fn selection_follows_format_and_content_type() {
        assert!(select(ApiFormat::Passthrough, Some("application/json"), false).is_none());
        for format in ApiFormat::ALL.into_iter().filter(|f| f.is_inference()) {
            assert!(select(format, None, true).is_some(), "{format}");
        }
    }
}
