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
    /// LM Studio's thinking shards (`reasoning_content`); Ollama's OpenAI layer calls them `reasoning`.
    reasoning_content: Option<String>,
    reasoning: Option<String>,
}

impl Delta {
    /// Thinking first, then the visible content: both are reply text for scanning and capture.
    fn texts(&self) -> impl Iterator<Item = &str> {
        self.reasoning_content
            .as_deref()
            .into_iter()
            .chain(self.reasoning.as_deref())
            .chain(self.content.as_deref())
    }
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
                if let Some(delta) = choice.delta.as_ref().or(choice.message.as_ref()) {
                    for text in delta.texts() {
                        self.text.push(text);
                    }
                }
                if let Some(text) = choice.text.as_deref() {
                    self.text.push(text);
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
    const LMS_V1_SSE_NO_OPTS: &[u8] =
        include_bytes!("../../../../fixtures/lmstudio/v1-chat-sse-without-stream-options.txt");
    const LMS_V1_SSE_OPTS: &[u8] =
        include_bytes!("../../../../fixtures/lmstudio/v1-chat-sse-with-stream-options.txt");
    const LMS_V1_SINGLE: &[u8] =
        include_bytes!("../../../../fixtures/lmstudio/v1-chat-nonstream.json");
    const LMS_V0_SSE: &[u8] =
        include_bytes!("../../../../fixtures/lmstudio/api-v0-chat-stream.txt");
    const LMS_V0_SSE_OPTS: &[u8] =
        include_bytes!("../../../../fixtures/lmstudio/api-v0-chat-stream-with-stream-options.txt");
    const LMS_V0_SINGLE: &[u8] =
        include_bytes!("../../../../fixtures/lmstudio/api-v0-chat-nonstream.json");
    const LMS_V0_COMPLETIONS_SSE: &[u8] =
        include_bytes!("../../../../fixtures/lmstudio/api-v0-completions-stream.txt");
    const LMS_V1_COMPLETIONS_SSE: &[u8] =
        include_bytes!("../../../../fixtures/lmstudio/v1-completions-sse.txt");
    const OLLAMA_V1_COMPLETIONS_SSE: &[u8] =
        include_bytes!("../../../../fixtures/ollama/v1-completions-sse.txt");
    const THINKING: &str = "Here's a thinking process:\n\n";

    /// LM Studio 0.4.25: a `/v1/chat/completions` stream carries no usage frame unless the request asks
    /// for it, which is why the gateway adds `stream_options.include_usage`.
    #[test]
    fn fixture_lmstudio_v1_chat_stream_reports_usage_only_with_stream_options() {
        let without = parse_all(
            Box::new(OpenAiParser::new(Flavor::Chat, true)),
            LMS_V1_SSE_NO_OPTS,
            97,
        )
        .unwrap();
        assert!(without.tokens_estimated);
        assert_eq!(without.prompt_tokens, None);
        assert_eq!(without.gen_tokens, Some(8), "eight choice frames stand in");
        assert_eq!(without.stop_reason.as_deref(), Some("length"));
        assert_eq!(without.model.as_deref(), Some("qwen/qwen3.6-27b"));
        assert_eq!(without.text, THINKING, "reasoning shards are reply text");
        let with = parse_all(
            Box::new(OpenAiParser::new(Flavor::Chat, true)),
            LMS_V1_SSE_OPTS,
            97,
        )
        .unwrap();
        assert_eq!((with.prompt_tokens, with.gen_tokens), (Some(12), Some(7)));
        assert!(!with.tokens_estimated);
        assert!(with.timings_estimated, "the /v1 stream has no engine stats");
        assert_eq!(with.stop_reason.as_deref(), Some("length"));
        assert_eq!(with.text, THINKING);
        let single = parse_all(
            Box::new(OpenAiParser::new(Flavor::Chat, false)),
            LMS_V1_SINGLE,
            64,
        )
        .unwrap();
        assert_eq!(
            (single.prompt_tokens, single.gen_tokens),
            (Some(12), Some(7))
        );
        assert_eq!(single.stop_reason.as_deref(), Some("length"));
        assert!(
            single.timings_estimated,
            "an empty stats object is not a timing"
        );
        assert_eq!(single.text, THINKING);
    }

    /// LM Studio's native chat endpoint streams the same frames as `/v1` (no stats, usage only when asked)
    /// and puts the engine stats on the non-streaming object.
    #[test]
    fn fixture_lmstudio_native_chat_stream_and_single() {
        let plain = parse_all(
            Box::new(OpenAiParser::new(Flavor::LmStudio, true)),
            LMS_V0_SSE,
            64,
        )
        .unwrap();
        assert!(plain.tokens_estimated);
        assert!(plain.timings_estimated);
        let with = parse_all(
            Box::new(OpenAiParser::new(Flavor::LmStudio, true)),
            LMS_V0_SSE_OPTS,
            64,
        )
        .unwrap();
        assert_eq!((with.prompt_tokens, with.gen_tokens), (Some(12), Some(7)));
        assert!(!with.tokens_estimated);
        assert!(with.timings_estimated);
        let single = parse_all(
            Box::new(OpenAiParser::new(Flavor::LmStudio, false)),
            LMS_V0_SINGLE,
            64,
        )
        .unwrap();
        assert_eq!(
            (single.prompt_tokens, single.gen_tokens),
            (Some(12), Some(7))
        );
        assert_eq!(
            single.stop_reason.as_deref(),
            Some("maxPredictedTokensReached")
        );
        assert!(!single.timings_estimated);
        assert!((single.tokens_per_sec.unwrap() - 19.444444444444446).abs() < 1e-9);
        assert!((single.ttft_ms.unwrap() - 211.0).abs() < 1e-9);
        assert!(
            (single.total_ms - 571.0).abs() < 1e-6,
            "{}",
            single.total_ms
        );
        assert_eq!(single.text, THINKING);
    }

    /// Completions streams differ from chat: LM Studio puts `usage` (and a draft-token `stats` object
    /// with no timings) on the final frame unasked; Ollama sends no usage.
    #[test]
    fn fixture_completions_streams_carry_inline_usage_on_lmstudio_and_none_on_ollama() {
        for (flavor, body) in [
            (Flavor::Completions, LMS_V1_COMPLETIONS_SSE),
            (Flavor::LmStudio, LMS_V0_COMPLETIONS_SSE),
        ] {
            let parsed = parse_all(Box::new(OpenAiParser::new(flavor, true)), body, 50).unwrap();
            assert_eq!(
                (parsed.prompt_tokens, parsed.gen_tokens),
                (Some(2), Some(7))
            );
            assert!(!parsed.tokens_estimated);
            assert!(
                parsed.timings_estimated,
                "draft-token counts are not timings"
            );
            assert_eq!(parsed.stop_reason.as_deref(), Some("length"));
            assert!(!parsed.text.is_empty());
        }
        let ollama = parse_all(
            Box::new(OpenAiParser::new(Flavor::Completions, true)),
            OLLAMA_V1_COMPLETIONS_SSE,
            50,
        )
        .unwrap();
        assert!(ollama.tokens_estimated);
        assert_eq!(ollama.gen_tokens, Some(6));
        assert_eq!(ollama.stop_reason.as_deref(), Some("length"));
        assert_eq!(ollama.model.as_deref(), Some("gemma4:12b-mlx"));
    }
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
    fn fixture_lmstudio_chat_stream_overflow_is_an_error_event() {
        // A 200 stream whose only frame is `event: error`; LM Studio 0.4.25 refusing an oversized prompt.
        let body = include_bytes!("../../../../fixtures/lmstudio/v1-chat-sse-context-overflow.txt");
        let parsed = parse_all(Box::new(OpenAiParser::new(Flavor::Chat, true)), body, 32).unwrap();
        let message = parsed.in_stream_error.as_deref().unwrap();
        assert!(
            message.starts_with("The number of tokens to keep"),
            "{message}"
        );
        assert_eq!(
            crate::rejection::parse_overflow(message),
            Some((None, None))
        );
        assert_eq!(parsed.stop_reason, None);
        assert!(parsed.text.is_empty());
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
