//! OpenAI-compatible chat and completions on either backend, and LM Studio's native `/api/v0` variants,
//! which share the wire shape and add a `stats` object (`tokens_per_second`, `time_to_first_token`,
//! `generation_time`, `stop_reason`). Streaming is SSE: `delta.content` (chat) or `text` (completions)
//! shards, a `finish_reason` on the last choice frame, then, when the server sends it, a usage frame with
//! empty `choices` and `[DONE]`. Ollama's `:cloud` relay drops the usage frame (ollama/ollama#15169), so a
//! stream that finishes without usage is recorded with the chunk count as an estimate. The server may also
//! answer with one `{"error": …}` frame inside the open stream and close.

use serde::Deserialize;
use serde_json::Value;

use super::sse_frames::FrameReader;
use super::{Parsed, SINGLE_BODY_CAP, StreamParser, TextSink, WallClock};
use crate::rejection::in_stream_error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Chat,
    Completions,
    /// LM Studio `/api/v0/chat/completions` and `/api/v0/completions`: OpenAI shape plus `stats`.
    LmStudio,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Frame {
    model: Option<String>,
    choices: Option<Vec<Choice>>,
    usage: Option<Usage>,
    stats: Option<Stats>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Choice {
    finish_reason: Option<String>,
    delta: Option<Delta>,
    message: Option<Delta>,
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Delta {
    content: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(default)]
struct Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

/// LM Studio's engine statistics; seconds, as the REST API reports them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
struct Stats {
    tokens_per_second: Option<f64>,
    time_to_first_token: Option<f64>,
    generation_time: Option<f64>,
    stop_reason: Option<String>,
}

pub struct OpenAiParser {
    flavor: Flavor,
    streaming: bool,
    frames: FrameReader,
    single: Vec<u8>,
    single_overflow: bool,
    model: Option<String>,
    usage: Option<Usage>,
    stats: Option<Stats>,
    finish_reason: Option<String>,
    delta_count: u64,
    error: Option<String>,
    text: TextSink,
}

impl OpenAiParser {
    pub fn new(flavor: Flavor, streaming: bool) -> Self {
        Self {
            flavor,
            streaming,
            frames: FrameReader::default(),
            single: Vec::new(),
            single_overflow: false,
            model: None,
            usage: None,
            stats: None,
            finish_reason: None,
            delta_count: 0,
            error: None,
            text: TextSink::default(),
        }
    }

    fn take_json(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() || text == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if let Some(message) = in_stream_error(&value) {
            self.error.get_or_insert(message);
            return;
        }
        let Ok(frame) = serde_json::from_value::<Frame>(value) else {
            return;
        };
        if let Some(model) = frame.model.filter(|m| !m.is_empty()) {
            self.model.get_or_insert(model);
        }
        if let Some(choices) = frame.choices {
            if !choices.is_empty() {
                self.delta_count += 1;
            }
            for choice in choices {
                let content = choice
                    .delta
                    .as_ref()
                    .and_then(|d| d.content.as_deref())
                    .or_else(|| choice.message.as_ref().and_then(|m| m.content.as_deref()))
                    .or(choice.text.as_deref());
                if let Some(content) = content {
                    self.text.push(content);
                }
                if let Some(reason) = choice.finish_reason {
                    self.finish_reason = Some(reason);
                }
            }
        }
        if let Some(usage) = frame.usage {
            self.usage = Some(usage);
        }
        if let Some(stats) = frame.stats {
            self.stats = Some(stats);
        }
    }
}

impl StreamParser for OpenAiParser {
    fn push(&mut self, chunk: &[u8]) {
        if self.streaming {
            self.frames.push(chunk);
            while let Some(frame) = self.frames.next_frame() {
                self.take_json(&frame.data);
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
        self.streaming
            && self.finish_reason.is_some()
            && self.usage.is_none()
            && self.error.is_none()
    }

    fn finalize(mut self: Box<Self>, wall: &WallClock) -> Option<Parsed> {
        if self.streaming {
            if let Some(frame) = self.frames.finish() {
                self.take_json(&frame.data);
            }
        } else if !self.single_overflow {
            let body = std::mem::take(&mut self.single);
            self.take_json(&String::from_utf8_lossy(&body));
        }
        if self.model.is_none()
            && self.usage.is_none()
            && self.delta_count == 0
            && self.error.is_none()
        {
            return None;
        }
        let (text, text_truncated) = std::mem::take(&mut self.text).into_parts();
        let mut parsed = Parsed {
            model: self.model.clone(),
            in_stream_error: self.error.clone(),
            text,
            text_truncated,
            ..Default::default()
        };
        match self.usage {
            Some(usage) => {
                parsed.prompt_tokens = Some(usage.prompt_tokens);
                parsed.gen_tokens = Some(usage.completion_tokens);
            }
            None => {
                // No usage frame: the chunk count stands in for the generated tokens, the prompt is unknown.
                parsed.gen_tokens = Some(self.delta_count);
                parsed.tokens_estimated = true;
            }
        }
        parsed.stop_reason = match self.flavor {
            Flavor::LmStudio => self
                .stats
                .as_ref()
                .and_then(|s| s.stop_reason.clone())
                .or_else(|| self.finish_reason.clone()),
            _ => self.finish_reason.clone(),
        }
        .or_else(|| (self.error.is_none() && self.streaming).then(|| "unknown".to_string()));
        match self
            .stats
            .as_ref()
            .filter(|s| s.tokens_per_second.is_some() || s.time_to_first_token.is_some())
        {
            Some(stats) => {
                parsed.tokens_per_sec = stats
                    .tokens_per_second
                    .filter(|v| v.is_finite() && *v > 0.0);
                parsed.ttft_ms = stats.time_to_first_token.map(|s| s * 1000.0);
                parsed.total_ms = match (stats.time_to_first_token, stats.generation_time) {
                    (Some(ttft), Some(generation)) => (ttft + generation) * 1000.0,
                    _ => wall.total_ms(),
                };
                parsed.timings_estimated = false;
                Some(parsed)
            }
            None => Some(parsed.with_wall_timings(wall, self.streaming)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::parse_all;

    const SSE: &[u8] = include_bytes!("../../../../fixtures/ollama/openai-compat-sse.txt");
    const SSE_NO_USAGE: &[u8] =
        include_bytes!("../../../../fixtures/ollama/openai-compat-sse-cloud-nousage.txt");
    const SINGLE: &[u8] =
        include_bytes!("../../../../fixtures/ollama/openai-compat-nonstream.json");

    #[test]
    fn fixture_sse_keeps_finish_reason_across_frames_and_reads_usage() {
        for chunk in [1, 7, 64, 4096] {
            let parsed = parse_all(Box::new(OpenAiParser::new(Flavor::Chat, true)), SSE, chunk)
                .expect("stats");
            assert_eq!(parsed.model.as_deref(), Some("qwen3:14b"));
            assert_eq!(parsed.prompt_tokens, Some(11));
            assert_eq!(parsed.gen_tokens, Some(5));
            assert_eq!(
                parsed.stop_reason.as_deref(),
                Some("length"),
                "finish_reason lives in an earlier frame than usage"
            );
            assert!(!parsed.tokens_estimated);
            assert!(parsed.timings_estimated, "wall clock");
            assert!(parsed.ttft_ms.is_some());
            assert!(parsed.tokens_per_sec.unwrap() > 0.0);
        }
    }

    #[test]
    fn fixture_sse_without_usage_falls_back_to_the_chunk_count() {
        let parsed = parse_all(
            Box::new(OpenAiParser::new(Flavor::Chat, true)),
            SSE_NO_USAGE,
            64,
        )
        .expect("stats");
        assert!(parsed.tokens_estimated);
        assert_eq!(parsed.prompt_tokens, None);
        assert!(parsed.gen_tokens.unwrap() > 0);
        assert_eq!(parsed.stop_reason.as_deref(), Some("length"));
    }

    #[test]
    fn fixture_single_extracts_usage_and_finish_reason() {
        let parsed =
            parse_all(Box::new(OpenAiParser::new(Flavor::Chat, false)), SINGLE, 64).expect("stats");
        assert_eq!(parsed.model.as_deref(), Some("qwen3:14b"));
        assert_eq!(parsed.prompt_tokens, Some(11));
        assert_eq!(parsed.gen_tokens, Some(5));
        assert_eq!(parsed.stop_reason.as_deref(), Some("length"));
        assert_eq!(parsed.ttft_ms, None);
        assert!(parsed.tokens_per_sec.is_some());
    }

    #[test]
    fn awaiting_trailer_is_only_between_finish_reason_and_usage() {
        let mut parser = OpenAiParser::new(Flavor::Chat, true);
        parser.push(b"data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        assert!(!parser.awaiting_trailer(), "still generating");
        parser.push(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
        assert!(parser.awaiting_trailer());
        parser.push(b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7}}\n\ndata: [DONE]\n\n");
        assert!(!parser.awaiting_trailer());
        assert_eq!(parser.take_text_deltas(), vec!["hi".to_string()]);
        assert!(!OpenAiParser::new(Flavor::Chat, false).awaiting_trailer());
    }

    #[test]
    fn a_stream_cut_mid_generation_is_an_estimate_with_unknown_stop() {
        let body = b"data: {\"model\":\"deepseek-v4-pro\",\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let parsed = parse_all(Box::new(OpenAiParser::new(Flavor::Chat, true)), body, 64).unwrap();
        assert!(parsed.tokens_estimated);
        assert_eq!(parsed.gen_tokens, Some(1));
        assert_eq!(parsed.stop_reason.as_deref(), Some("unknown"));
        assert_eq!(parsed.text, "partial");
        assert!(
            parse_all(
                Box::new(OpenAiParser::new(Flavor::Chat, true)),
                b"data: [DONE]\n\n",
                64
            )
            .is_none()
        );
    }

    #[test]
    fn an_error_frame_inside_the_stream_is_reported() {
        let body = b"data: {\"error\":{\"message\":\"Input does not fit in context length. The input has 359277 tokens, but the context length only supports 262144 tokens.\"}}\n\n";
        let parsed = parse_all(Box::new(OpenAiParser::new(Flavor::Chat, true)), body, 64).unwrap();
        assert!(
            parsed
                .in_stream_error
                .as_deref()
                .unwrap()
                .starts_with("Input does not fit")
        );
        assert_eq!(parsed.stop_reason, None);
    }

    #[test]
    fn completions_text_and_lm_studio_stats_are_read() {
        let body = b"data: {\"model\":\"m\",\"choices\":[{\"text\":\"The\",\"index\":0}]}\n\ndata: {\"choices\":[{\"text\":\" end\",\"finish_reason\":\"stop\"}]}\n\n";
        let parsed = parse_all(
            Box::new(OpenAiParser::new(Flavor::Completions, true)),
            body,
            9,
        )
        .unwrap();
        assert_eq!(parsed.text, "The end");
        // The shape LM Studio documents for /api/v0/chat/completions (synthetic sample).
        let lms = br#"{"id":"chatcmpl-1","object":"chat.completion","model":"qwen3.8-27b","choices":[{"index":0,"message":{"role":"assistant","content":"Hello!"},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":3,"total_tokens":23},"stats":{"tokens_per_second":96.48,"time_to_first_token":0.33,"generation_time":0.031,"stop_reason":"eosFound"},"model_info":{"arch":"qwen3"},"runtime":{"name":"mlx-llm"}}"#;
        let parsed = parse_all(
            Box::new(OpenAiParser::new(Flavor::LmStudio, false)),
            lms,
            64,
        )
        .unwrap();
        assert_eq!(parsed.model.as_deref(), Some("qwen3.8-27b"));
        assert_eq!(parsed.prompt_tokens, Some(20));
        assert_eq!(parsed.gen_tokens, Some(3));
        assert_eq!(
            parsed.stop_reason.as_deref(),
            Some("eosFound"),
            "stats.stop_reason wins for LM Studio"
        );
        assert!(!parsed.timings_estimated, "engine stats are authoritative");
        assert!((parsed.tokens_per_sec.unwrap() - 96.48).abs() < 1e-9);
        assert!((parsed.ttft_ms.unwrap() - 330.0).abs() < 1e-9);
        assert!((parsed.total_ms - 361.0).abs() < 1e-6);
        assert_eq!(parsed.text, "Hello!");
    }
}
