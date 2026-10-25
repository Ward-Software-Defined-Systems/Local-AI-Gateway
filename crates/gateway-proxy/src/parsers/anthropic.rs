//! The Anthropic Messages API (`/v1/messages`), served by both backends. Streaming is SSE with named
//! events: `message_start` carries the model and `usage.input_tokens`; `content_block_delta` carries
//! `text_delta` shards; `message_delta` carries `delta.stop_reason` and `usage.output_tokens`;
//! `message_stop` ends it; an `error` event carries a refusal. Non-streaming is one JSON object with
//! `content[].text`, `stop_reason` and `usage`. Both backends serve it (fixtures under
//! `fixtures/lmstudio` and `fixtures/ollama`): Ollama splits the prompt between `input_tokens` and
//! `cache_read_input_tokens`, so the prompt count is their sum, and streams thinking blocks
//! (`thinking_delta`), which count as reply text here.

use serde::Deserialize;
use serde_json::Value;

use super::sse_frames::FrameReader;
use super::{Parsed, SINGLE_BODY_CAP, StreamParser, TextSink, WallClock};
use crate::rejection::in_stream_error;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Event {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<MessageStart>,
    delta: Option<Delta>,
    usage: Option<Usage>,
    // Non-streaming message fields.
    model: Option<String>,
    content: Option<Vec<ContentBlock>>,
    stop_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MessageStart {
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Delta {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
    thinking: Option<String>,
    stop_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(default)]
struct Usage {
    input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl Usage {
    /// The whole prompt: Ollama reports cached tokens separately from `input_tokens`.
    fn prompt_tokens(&self) -> Option<u64> {
        match (
            self.input_tokens,
            self.cache_read_input_tokens,
            self.cache_creation_input_tokens,
        ) {
            (None, None, None) => None,
            (input, read, created) => {
                Some(input.unwrap_or(0) + read.unwrap_or(0) + created.unwrap_or(0))
            }
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
    thinking: Option<String>,
}

pub struct AnthropicParser {
    streaming: bool,
    frames: FrameReader,
    single: Vec<u8>,
    single_overflow: bool,
    model: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    stop_reason: Option<String>,
    deltas: u64,
    block_stopped: bool,
    message_delta_seen: bool,
    error: Option<String>,
    text: TextSink,
}

impl AnthropicParser {
    pub fn new(streaming: bool) -> Self {
        Self {
            streaming,
            frames: FrameReader::default(),
            single: Vec::new(),
            single_overflow: false,
            model: None,
            input_tokens: None,
            output_tokens: None,
            stop_reason: None,
            deltas: 0,
            block_stopped: false,
            message_delta_seen: false,
            error: None,
            text: TextSink::default(),
        }
    }

    fn take_json(&mut self, event_name: Option<&str>, text: &str) {
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
        let Ok(event) = serde_json::from_value::<Event>(value) else {
            return;
        };
        let kind = event.kind.as_deref().or(event_name).unwrap_or("");
        match kind {
            "message_start" => {
                if let Some(start) = &event.message {
                    if let Some(model) = &start.model {
                        self.model.get_or_insert(model.clone());
                    }
                    // `message_start` reports the prompt size; its `output_tokens` is a placeholder (1) until
                    // `message_delta` carries the real count.
                    if let Some(usage) = start.usage {
                        self.input_tokens = usage.prompt_tokens().or(self.input_tokens);
                    }
                }
            }
            "content_block_delta" => {
                if let Some(delta) = &event.delta
                    && delta.kind.as_deref() != Some("input_json_delta")
                    && let Some(text) = delta.text.as_deref().or(delta.thinking.as_deref())
                {
                    self.text.push(text);
                }
                self.deltas += 1;
            }
            "content_block_stop" => self.block_stopped = true,
            "message_delta" => {
                self.message_delta_seen = true;
                if let Some(delta) = &event.delta
                    && let Some(reason) = &delta.stop_reason
                {
                    self.stop_reason = Some(reason.clone());
                }
                if let Some(usage) = event.usage {
                    if let Some(out) = usage.output_tokens {
                        self.output_tokens = Some(out);
                    }
                    if let Some(input) = usage.prompt_tokens() {
                        self.input_tokens = Some(input);
                    }
                }
            }
            "message" => {
                // The non-streaming object.
                if let Some(model) = &event.model {
                    self.model.get_or_insert(model.clone());
                }
                if let Some(usage) = event.usage {
                    self.input_tokens = usage.prompt_tokens();
                    self.output_tokens = usage.output_tokens;
                }
                self.stop_reason = event.stop_reason.clone();
                for block in event.content.unwrap_or_default() {
                    match block.kind.as_deref() {
                        Some("text") => {
                            if let Some(text) = &block.text {
                                self.text.push(text);
                            }
                        }
                        Some("thinking") => {
                            if let Some(thinking) = &block.thinking {
                                self.text.push(thinking);
                            }
                        }
                        _ => {}
                    }
                }
                self.message_delta_seen = true;
            }
            _ => {}
        }
    }
}

impl StreamParser for AnthropicParser {
    fn push(&mut self, chunk: &[u8]) {
        if self.streaming {
            self.frames.push(chunk);
            while let Some(frame) = self.frames.next_frame() {
                self.take_json(frame.event.as_deref(), &frame.data);
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

    /// A content block has ended but the `message_delta` with the counts has not arrived.
    fn awaiting_trailer(&self) -> bool {
        self.streaming && self.block_stopped && !self.message_delta_seen && self.error.is_none()
    }

    fn finalize(mut self: Box<Self>, wall: &WallClock) -> Option<Parsed> {
        if self.streaming {
            if let Some(frame) = self.frames.finish() {
                self.take_json(frame.event.as_deref(), &frame.data);
            }
        } else if !self.single_overflow {
            let body = std::mem::take(&mut self.single);
            self.take_json(None, &String::from_utf8_lossy(&body));
        }
        if self.model.is_none()
            && self.deltas == 0
            && self.input_tokens.is_none()
            && self.error.is_none()
        {
            return None;
        }
        let (text, text_truncated) = std::mem::take(&mut self.text).into_parts();
        let estimated = self.output_tokens.is_none();
        let parsed = Parsed {
            model: self.model.clone(),
            prompt_tokens: self.input_tokens,
            gen_tokens: self.output_tokens.or(Some(self.deltas)),
            tokens_estimated: estimated,
            stop_reason: self.stop_reason.clone().or_else(|| {
                (self.error.is_none() && self.streaming).then(|| "unknown".to_string())
            }),
            in_stream_error: self.error.clone(),
            text,
            text_truncated,
            ..Default::default()
        };
        Some(parsed.with_wall_timings(wall, self.streaming))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::parse_all;

    /// The documented Anthropic stream shape (synthetic sample).
    const STREAM: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"qwen3:14b\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n\
        event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
        event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
        event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n\
        event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
        event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":12}}\n\n\
        event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    #[test]
    fn the_stream_yields_model_usage_stop_reason_and_text() {
        for chunk in [1, 11, 128, 4096] {
            let parsed = parse_all(
                Box::new(AnthropicParser::new(true)),
                STREAM.as_bytes(),
                chunk,
            )
            .expect("stats");
            assert_eq!(parsed.model.as_deref(), Some("qwen3:14b"));
            assert_eq!(parsed.prompt_tokens, Some(25));
            assert_eq!(parsed.gen_tokens, Some(12));
            assert!(!parsed.tokens_estimated);
            assert_eq!(parsed.stop_reason.as_deref(), Some("end_turn"));
            assert_eq!(parsed.text, "Hello there");
            assert!(parsed.timings_estimated);
        }
    }

    #[test]
    fn awaiting_trailer_spans_block_stop_to_message_delta() {
        let mut parser = AnthropicParser::new(true);
        let upto_stop = STREAM.split("event: message_delta").next().unwrap();
        parser.push(upto_stop.as_bytes());
        assert!(parser.awaiting_trailer());
        parser.push(&STREAM.as_bytes()[upto_stop.len()..]);
        assert!(!parser.awaiting_trailer());
    }

    #[test]
    fn a_cut_stream_estimates_output_tokens_from_deltas() {
        let cut = STREAM.split("event: content_block_stop").next().unwrap();
        let parsed = parse_all(Box::new(AnthropicParser::new(true)), cut.as_bytes(), 64).unwrap();
        assert_eq!(parsed.prompt_tokens, Some(25));
        assert_eq!(parsed.gen_tokens, Some(2));
        assert!(parsed.tokens_estimated);
        assert_eq!(parsed.stop_reason.as_deref(), Some("unknown"));
    }

    #[test]
    fn the_non_streaming_message_and_an_error_event_parse() {
        let single = br#"{"id":"msg_1","type":"message","role":"assistant","model":"qwen3:14b","content":[{"type":"text","text":"Hi!"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":9,"output_tokens":3}}"#;
        let parsed = parse_all(Box::new(AnthropicParser::new(false)), single, 64).unwrap();
        assert_eq!(parsed.prompt_tokens, Some(9));
        assert_eq!(parsed.gen_tokens, Some(3));
        assert_eq!(parsed.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(parsed.text, "Hi!");
        assert_eq!(parsed.ttft_ms, None);
        let error = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let parsed = parse_all(Box::new(AnthropicParser::new(true)), error, 64).unwrap();
        assert_eq!(parsed.in_stream_error.as_deref(), Some("Overloaded"));
        assert!(parse_all(Box::new(AnthropicParser::new(true)), b"", 64).is_none());
    }

    const LMS_STREAM: &[u8] = include_bytes!("../../../../fixtures/lmstudio/v1-messages-sse.txt");
    const LMS_SINGLE: &[u8] =
        include_bytes!("../../../../fixtures/lmstudio/v1-messages-nonstream.json");
    const OLLAMA_STREAM: &[u8] = include_bytes!("../../../../fixtures/ollama/v1-messages-sse.txt");
    const OLLAMA_SINGLE: &[u8] =
        include_bytes!("../../../../fixtures/ollama/v1-messages-nonstream.json");
    const ANTHROPIC_STREAM: &[u8] =
        include_bytes!("../../../../fixtures/anthropic/v1-messages-sse.txt");
    const ANTHROPIC_SINGLE: &[u8] =
        include_bytes!("../../../../fixtures/anthropic/v1-messages-nonstream.json");

    /// The real API (`claude-haiku-5-5`): a `ping` event between the content block and its deltas, and a
    /// `message_delta` whose usage repeats `input_tokens` beside the final `output_tokens`.
    #[test]
    fn fixture_anthropic_messages_stream_and_single() {
        for chunk in [1, 7, 77, 4096] {
            let s = parse_all(
                Box::new(AnthropicParser::new(true)),
                ANTHROPIC_STREAM,
                chunk,
            )
            .unwrap();
            assert_eq!(s.model.as_deref(), Some("claude-haiku-5-5"));
            assert_eq!((s.prompt_tokens, s.gen_tokens), (Some(13), Some(14)));
            assert!(!s.tokens_estimated);
            assert_eq!(s.stop_reason.as_deref(), Some("end_turn"));
            assert_eq!(s.text, "Hi there! How can I help you today?");
            assert!(s.ttft_ms.is_some());
        }
        let single =
            parse_all(Box::new(AnthropicParser::new(false)), ANTHROPIC_SINGLE, 64).unwrap();
        assert_eq!(single.model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(
            (single.prompt_tokens, single.gen_tokens),
            (Some(13), Some(14))
        );
        assert_eq!(single.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(single.text, "Hi there! How can I help you today?");
        assert_eq!(single.ttft_ms, None);
    }

    #[test]
    fn fixture_lmstudio_messages_stream_and_single() {
        for chunk in [7, 77, 4096] {
            let s = parse_all(Box::new(AnthropicParser::new(true)), LMS_STREAM, chunk).unwrap();
            assert_eq!(s.model.as_deref(), Some("qwen/qwen3.6-27b"));
            assert_eq!((s.prompt_tokens, s.gen_tokens), (Some(14), Some(7)));
            assert!(!s.tokens_estimated);
            assert_eq!(s.stop_reason.as_deref(), Some("max_tokens"));
            assert_eq!(s.text, "Hi there! How can I help");
        }
        let single = parse_all(Box::new(AnthropicParser::new(false)), LMS_SINGLE, 64).unwrap();
        assert_eq!(
            (single.prompt_tokens, single.gen_tokens),
            (Some(14), Some(3))
        );
        assert_eq!(single.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(single.text, "Hi!");
    }

    #[test]
    fn fixture_ollama_messages_sum_cached_input_tokens_and_keep_thinking_text() {
        let s = parse_all(Box::new(AnthropicParser::new(true)), OLLAMA_STREAM, 77).unwrap();
        assert_eq!(s.model.as_deref(), Some("gemma4:12b-mlx"));
        assert_eq!(
            (s.prompt_tokens, s.gen_tokens),
            (Some(18), Some(8)),
            "message_delta's count replaces message_start's placeholder"
        );
        assert_eq!(s.stop_reason.as_deref(), Some("max_tokens"));
        assert_eq!(s.text, "The user said \"Say");
        let single = parse_all(Box::new(AnthropicParser::new(false)), OLLAMA_SINGLE, 64).unwrap();
        assert_eq!(
            (single.prompt_tokens, single.gen_tokens),
            (Some(18), Some(8)),
            "input_tokens 1 plus cache_read_input_tokens 17"
        );
        assert_eq!(single.stop_reason.as_deref(), Some("max_tokens"));
        assert_eq!(single.text, "The user said \"Say");
    }

    #[test]
    fn fixture_lmstudio_messages_stream_overflow_is_an_error_event() {
        // A 200 stream whose only frame is `event: error`; LM Studio 0.4.25 refusing an oversized prompt.
        let body =
            include_bytes!("../../../../fixtures/lmstudio/v1-messages-sse-context-overflow.txt");
        let parsed =
            crate::parsers::parse_all(Box::new(AnthropicParser::new(true)), body, 32).unwrap();
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
}
