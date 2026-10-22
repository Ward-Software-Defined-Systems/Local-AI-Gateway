//! Ollama's native `/api/chat` and `/api/generate`: NDJSON when streaming (the default), one JSON object
//! otherwise. Every object may carry `message.content` or `response` text; the terminal object has
//! `done: true` with the engine's counts and nanosecond durations, from which tokens per second
//! (`eval_count / eval_duration`) and TTFT (`prompt_eval_duration + load_duration`) follow. A `done_reason`
//! of `load` or `unload` marks a model load call, not an inference. A body that ends without a trailing
//! newline is still parsed at finalize (Ollama-Monitor dropped it). An `{"error": …}` line inside a 2xx
//! stream is reported, not mistaken for a chunk.

use serde::Deserialize;
use serde_json::Value;

use super::line_buffer::LineBuffer;
use super::{Parsed, SINGLE_BODY_CAP, StreamParser, TextSink, WallClock};
use crate::rejection::in_stream_error;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Chunk {
    model: Option<String>,
    done: bool,
    done_reason: Option<String>,
    eval_count: Option<u64>,
    eval_duration: Option<u64>,
    prompt_eval_count: Option<u64>,
    prompt_eval_duration: Option<u64>,
    total_duration: Option<u64>,
    load_duration: Option<u64>,
    message: Option<Message>,
    response: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Message {
    content: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Terminal {
    prompt_tokens: u64,
    gen_tokens: u64,
    eval_duration_ns: u64,
    prompt_eval_duration_ns: u64,
    load_duration_ns: u64,
    total_duration_ns: u64,
    done_reason: String,
}

pub struct OllamaParser {
    streaming: bool,
    lines: LineBuffer,
    single: Vec<u8>,
    single_overflow: bool,
    model: Option<String>,
    terminal: Option<Terminal>,
    chunks: u64,
    error: Option<String>,
    text: TextSink,
}

impl OllamaParser {
    pub fn new(streaming: bool) -> Self {
        Self {
            streaming,
            lines: LineBuffer::default(),
            single: Vec::new(),
            single_overflow: false,
            model: None,
            terminal: None,
            chunks: 0,
            error: None,
            text: TextSink::default(),
        }
    }

    fn take_object(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if let Some(message) = in_stream_error(&value) {
            self.error.get_or_insert(message);
            return;
        }
        let Ok(chunk) = serde_json::from_value::<Chunk>(value) else {
            return;
        };
        if let Some(model) = chunk.model.as_deref().filter(|m| !m.is_empty()) {
            self.model.get_or_insert_with(|| model.to_string());
        }
        if let Some(content) = chunk.message.as_ref().and_then(|m| m.content.as_deref()) {
            self.text.push(content);
        }
        if let Some(response) = chunk.response.as_deref() {
            self.text.push(response);
        }
        self.chunks += 1;
        if chunk.done || chunk.eval_count.is_some() {
            self.terminal = Some(Terminal {
                prompt_tokens: chunk.prompt_eval_count.unwrap_or(0),
                gen_tokens: chunk.eval_count.unwrap_or(0),
                eval_duration_ns: chunk.eval_duration.unwrap_or(0),
                prompt_eval_duration_ns: chunk.prompt_eval_duration.unwrap_or(0),
                load_duration_ns: chunk.load_duration.unwrap_or(0),
                total_duration_ns: chunk.total_duration.unwrap_or(0),
                done_reason: chunk.done_reason.unwrap_or_else(|| "stop".to_string()),
            });
        }
    }
}

fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

impl StreamParser for OllamaParser {
    fn push(&mut self, chunk: &[u8]) {
        if self.streaming {
            self.lines.push(chunk);
            while let Some(line) = self.lines.next_line() {
                self.take_object(&line);
            }
        } else if self.single.len() + chunk.len() <= SINGLE_BODY_CAP {
            self.single.extend_from_slice(chunk);
        } else {
            self.single_overflow = true;
        }
    }

    fn take_text_deltas(&mut self) -> Vec<String> {
        self.text.take()
    }

    fn awaiting_trailer(&self) -> bool {
        false
    }

    fn finalize(mut self: Box<Self>, wall: &WallClock) -> Option<Parsed> {
        if self.streaming {
            if let Some(tail) = self.lines.take_tail() {
                self.take_object(&tail);
            }
        } else if !self.single_overflow {
            let body = std::mem::take(&mut self.single);
            self.take_object(&String::from_utf8_lossy(&body));
        }
        let (text, text_truncated) = std::mem::take(&mut self.text).into_parts();
        if self.terminal.is_none()
            && self.model.is_none()
            && self.chunks == 0
            && self.error.is_none()
        {
            return None;
        }
        let mut parsed = Parsed {
            model: self.model.clone(),
            in_stream_error: self.error.clone(),
            text,
            text_truncated,
            ..Default::default()
        };
        match &self.terminal {
            Some(t) => {
                parsed.prompt_tokens = Some(t.prompt_tokens);
                parsed.gen_tokens = Some(t.gen_tokens);
                let eval_secs = t.eval_duration_ns as f64 / 1e9;
                parsed.tokens_per_sec = (eval_secs > 0.0).then(|| t.gen_tokens as f64 / eval_secs);
                parsed.ttft_ms = Some(ns_to_ms(t.prompt_eval_duration_ns + t.load_duration_ns));
                parsed.total_ms = if t.total_duration_ns > 0 {
                    ns_to_ms(t.total_duration_ns)
                } else {
                    wall.total_ms()
                };
                parsed.timings_estimated = false;
                parsed.stop_reason = Some(t.done_reason.clone());
                parsed.is_load_or_unload = matches!(t.done_reason.as_str(), "load" | "unload");
                Some(parsed)
            }
            None => {
                // The stream was cut before its terminal line: keep what the chunk count says.
                parsed.gen_tokens = Some(self.chunks.saturating_sub(0));
                parsed.tokens_estimated = true;
                parsed.stop_reason = Some("unknown".to_string());
                Some(parsed.with_wall_timings(wall, self.streaming))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::parse_all;

    const STREAM: &[u8] = include_bytes!("../../../../fixtures/ollama/ollama-chat-stream.jsonl");
    const SINGLE: &[u8] = include_bytes!("../../../../fixtures/ollama/ollama-chat-nonstream.json");

    #[test]
    fn fixture_stream_yields_engine_timings_and_text() {
        let parsed = parse_all(Box::new(OllamaParser::new(true)), STREAM, 64).expect("stats");
        assert_eq!(parsed.model.as_deref(), Some("qwen3:14b"));
        assert!(parsed.prompt_tokens.unwrap() > 0);
        assert!(parsed.gen_tokens.unwrap() > 0);
        assert!(parsed.tokens_per_sec.unwrap() > 0.0);
        assert!(!parsed.timings_estimated);
        assert!(!parsed.tokens_estimated);
        assert!(!parsed.text.is_empty(), "message.content is reassembled");
        assert!(!parsed.is_load_or_unload);
        assert!(parsed.in_stream_error.is_none());
    }

    #[test]
    fn fixture_single_object_parses_with_or_without_a_trailing_newline() {
        let parsed = parse_all(Box::new(OllamaParser::new(false)), SINGLE, 64).expect("stats");
        assert_eq!(parsed.model.as_deref(), Some("qwen3:14b"));
        assert!(parsed.gen_tokens.unwrap() > 0);
        // The same body treated as a stream (no newline at all) parses at finalize.
        let trimmed: Vec<u8> = SINGLE.iter().copied().filter(|b| *b != b'\n').collect();
        let parsed = parse_all(Box::new(OllamaParser::new(true)), &trimmed, 64).expect("stats");
        assert!(parsed.gen_tokens.unwrap() > 0);
        assert!(!parsed.timings_estimated);
    }

    #[test]
    fn terminal_line_numbers_are_converted() {
        let body = concat!(
            r#"{"model":"qwen3:14b","done":false,"message":{"role":"assistant","content":"hi"}}"#,
            "\n",
            r#"{"model":"qwen3:14b","done":true,"done_reason":"stop","total_duration":2000000000,"load_duration":100000000,"prompt_eval_count":12,"prompt_eval_duration":300000000,"eval_count":80,"eval_duration":1500000000}"#,
            "\n"
        );
        let parsed = parse_all(Box::new(OllamaParser::new(true)), body.as_bytes(), 16).unwrap();
        assert_eq!(parsed.prompt_tokens, Some(12));
        assert_eq!(parsed.gen_tokens, Some(80));
        assert!((parsed.tokens_per_sec.unwrap() - 80.0 / 1.5).abs() < 1e-6);
        assert!((parsed.total_ms - 2000.0).abs() < 1e-6);
        assert!((parsed.ttft_ms.unwrap() - 400.0).abs() < 1e-6);
        assert_eq!(parsed.stop_reason.as_deref(), Some("stop"));
        assert_eq!(parsed.text, "hi");
    }

    #[test]
    fn load_and_unload_calls_are_flagged() {
        let load = "{\"model\":\"qwen3:14b\",\"created_at\":\"2026-10-04T15:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done_reason\":\"load\",\"done\":true}\n";
        let parsed = parse_all(Box::new(OllamaParser::new(true)), load.as_bytes(), 64).unwrap();
        assert!(parsed.is_load_or_unload);
        let unload = r#"{"model":"qwen3:14b","created_at":"2026-10-04T15:00:00Z","response":"","done":true,"done_reason":"unload"}"#;
        let parsed = parse_all(Box::new(OllamaParser::new(false)), unload.as_bytes(), 64).unwrap();
        assert!(parsed.is_load_or_unload);
    }

    #[test]
    fn a_cut_stream_is_an_estimate_and_an_error_line_is_reported() {
        let cut = r#"{"model":"qwen3:14b","done":false,"message":{"content":"par"}}"#;
        let parsed = parse_all(Box::new(OllamaParser::new(true)), cut.as_bytes(), 64).unwrap();
        assert!(parsed.tokens_estimated);
        assert_eq!(parsed.gen_tokens, Some(1));
        assert_eq!(parsed.stop_reason.as_deref(), Some("unknown"));
        assert!(parsed.timings_estimated);
        let error = "{\"error\":\"model requires more system memory\"}\n";
        let parsed = parse_all(Box::new(OllamaParser::new(true)), error.as_bytes(), 64).unwrap();
        assert_eq!(
            parsed.in_stream_error.as_deref(),
            Some("model requires more system memory")
        );
        assert!(parse_all(Box::new(OllamaParser::new(true)), b"", 64).is_none());
    }
}
