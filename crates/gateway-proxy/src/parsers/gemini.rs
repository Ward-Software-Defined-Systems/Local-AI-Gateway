//! Google's Gemini API: `/v1beta/models/{model}:generateContent` and `:streamGenerateContent`. Every
//! chunk is a `GenerateContentResponse`: `candidates[].content.parts[].text` (a part marked `thought` is
//! reasoning and counts as reply text, like the other parsers), `candidates[].finishReason` on the last
//! chunk, `usageMetadata` (cumulative, so the last one wins; `thoughtsTokenCount` is reasoning output) and
//! `modelVersion`. Three wire shapes: SSE frames with `?alt=sse`, one JSON object for `generateContent`,
//! and a JSON array written progressively for `streamGenerateContent` without `alt=sse`, which is buffered
//! and parsed at the end. An error is `{"error":{"code","message","status"}}`. Until real captures exist
//! the tests use the documented shapes.

use serde::Deserialize;
use serde_json::Value;

use super::sse_frames::FrameReader;
use super::{Parsed, SINGLE_BODY_CAP, StreamParser, TextSink, WallClock};
use crate::rejection::in_stream_error;

/// How the response arrives, decided from the content type and the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `text/event-stream`: one chunk per `data:` frame.
    Sse,
    /// One JSON object.
    Single,
    /// A JSON array of chunks, parsed once it is complete.
    Array,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Chunk {
    candidates: Vec<Candidate>,
    usage_metadata: Option<Usage>,
    model_version: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Candidate {
    content: Option<Content>,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Content {
    parts: Vec<Part>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Part {
    text: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
struct Usage {
    prompt_token_count: Option<u64>,
    candidates_token_count: Option<u64>,
    thoughts_token_count: Option<u64>,
}

impl Usage {
    /// Everything the model generated: the reply and its reasoning, the way the other APIs count output.
    fn generated(&self) -> Option<u64> {
        match (self.candidates_token_count, self.thoughts_token_count) {
            (None, None) => None,
            (c, t) => Some(c.unwrap_or(0) + t.unwrap_or(0)),
        }
    }
}

pub struct GeminiParser {
    mode: Mode,
    frames: FrameReader,
    buffered: Vec<u8>,
    overflow: bool,
    model: Option<String>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    chunks: u64,
    error: Option<String>,
    text: TextSink,
}

impl GeminiParser {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            frames: FrameReader::default(),
            buffered: Vec::new(),
            overflow: false,
            model: None,
            usage: None,
            finish_reason: None,
            chunks: 0,
            error: None,
            text: TextSink::default(),
        }
    }

    fn streamed(&self) -> bool {
        self.mode != Mode::Single
    }

    fn take_text(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            self.take_value(value);
        }
    }

    fn take_value(&mut self, value: Value) {
        if let Some(message) = in_stream_error(&value) {
            self.error.get_or_insert(message);
            return;
        }
        if let Value::Array(items) = value {
            for item in items {
                self.take_value(item);
            }
            return;
        }
        let Ok(chunk) = serde_json::from_value::<Chunk>(value) else {
            return;
        };
        if let Some(model) = chunk.model_version.filter(|m| !m.is_empty()) {
            self.model.get_or_insert(model);
        }
        if let Some(usage) = chunk.usage_metadata {
            self.usage = Some(usage);
        }
        if !chunk.candidates.is_empty() {
            self.chunks += 1;
        }
        for candidate in chunk.candidates {
            for part in candidate.content.map(|c| c.parts).unwrap_or_default() {
                if let Some(text) = &part.text {
                    self.text.push(text);
                }
            }
            if let Some(reason) = candidate.finish_reason {
                self.finish_reason = Some(reason);
            }
        }
    }
}

impl StreamParser for GeminiParser {
    fn push(&mut self, chunk: &[u8]) {
        if self.mode == Mode::Sse {
            self.frames.push(chunk);
            while let Some(frame) = self.frames.next_frame() {
                self.take_text(&frame.data);
            }
        } else if self.buffered.len() + chunk.len() <= SINGLE_BODY_CAP {
            self.buffered.extend_from_slice(chunk);
        } else {
            self.overflow = true;
        }
    }

    fn take_text_deltas(&mut self) -> Vec<String> {
        self.text.take()
    }

    /// The last chunk named a finish reason but carried no usage; the counts may still be on the way.
    fn awaiting_trailer(&self) -> bool {
        self.mode == Mode::Sse
            && self.finish_reason.is_some()
            && self.usage.is_none()
            && self.error.is_none()
    }

    fn finalize(mut self: Box<Self>, wall: &WallClock) -> Option<Parsed> {
        if self.mode == Mode::Sse {
            if let Some(frame) = self.frames.finish() {
                self.take_text(&frame.data);
            }
        } else if !self.overflow {
            let body = std::mem::take(&mut self.buffered);
            self.take_text(&String::from_utf8_lossy(&body));
        }
        if self.model.is_none() && self.chunks == 0 && self.usage.is_none() && self.error.is_none()
        {
            return None;
        }
        let (text, text_truncated) = std::mem::take(&mut self.text).into_parts();
        let generated = self.usage.and_then(|u| u.generated());
        let streamed = self.streamed();
        let parsed = Parsed {
            model: self.model.clone(),
            prompt_tokens: self.usage.and_then(|u| u.prompt_token_count),
            gen_tokens: generated.or((self.chunks > 0).then_some(self.chunks)),
            tokens_estimated: generated.is_none(),
            stop_reason: self
                .finish_reason
                .clone()
                .or_else(|| (self.error.is_none() && streamed).then(|| "unknown".to_string())),
            in_stream_error: self.error.clone(),
            text,
            text_truncated,
            ..Default::default()
        };
        Some(parsed.with_wall_timings(wall, streamed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::parse_all;

    /// The documented `streamGenerateContent?alt=sse` shape: usage is cumulative, the finish reason comes
    /// with the last chunk, and a reasoning part is marked `thought`.
    const SSE: &str = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Let me think.\",\"thought\":true}],\"role\":\"model\"},\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":7,\"totalTokenCount\":7},\"modelVersion\":\"gemini-3.1-pro-preview\",\"responseId\":\"r1\"}\r\n\r\n\
        data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"},\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":1,\"thoughtsTokenCount\":5,\"totalTokenCount\":13},\"modelVersion\":\"gemini-3.1-pro-preview\",\"responseId\":\"r1\"}\r\n\r\n\
        data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" there\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":5,\"totalTokenCount\":14},\"modelVersion\":\"gemini-3.1-pro-preview\",\"responseId\":\"r1\"}\r\n\r\n";

    const SINGLE: &str = r#"{"candidates":[{"content":{"parts":[{"text":"Hi!"}],"role":"model"},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":3,"totalTokenCount":12},"modelVersion":"gemini-3-flash","responseId":"r2"}"#;

    const ERROR: &str = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT"}}"#;

    #[test]
    fn the_sse_stream_yields_model_cumulative_usage_finish_reason_and_text() {
        for chunk in [1, 7, 64, 4096] {
            let parsed = parse_all(
                Box::new(GeminiParser::new(Mode::Sse)),
                SSE.as_bytes(),
                chunk,
            )
            .expect("stats");
            assert_eq!(parsed.model.as_deref(), Some("gemini-3.1-pro-preview"));
            assert_eq!(parsed.prompt_tokens, Some(7));
            assert_eq!(
                parsed.gen_tokens,
                Some(7),
                "candidates plus thoughts from the last chunk"
            );
            assert!(!parsed.tokens_estimated);
            assert_eq!(parsed.stop_reason.as_deref(), Some("STOP"));
            assert_eq!(parsed.text, "Let me think.Hello there");
            assert!(parsed.timings_estimated);
            assert!(parsed.ttft_ms.is_some());
        }
    }

    #[test]
    fn the_array_stream_parses_at_the_end_and_the_single_object_as_one() {
        let chunks: Vec<&str> = SSE
            .split("\r\n\r\n")
            .filter(|f| !f.is_empty())
            .map(|f| f.trim_start_matches("data: "))
            .collect();
        let array = format!("[\n  {}\n]\n", chunks.join(",\n  "));
        for chunk in [1, 13, 4096] {
            let parsed = parse_all(
                Box::new(GeminiParser::new(Mode::Array)),
                array.as_bytes(),
                chunk,
            )
            .unwrap();
            assert_eq!(parsed.gen_tokens, Some(7));
            assert_eq!(parsed.stop_reason.as_deref(), Some("STOP"));
            assert_eq!(parsed.text, "Let me think.Hello there");
            assert!(
                parsed.ttft_ms.is_some(),
                "an array stream is still a stream"
            );
        }
        let parsed = parse_all(
            Box::new(GeminiParser::new(Mode::Single)),
            SINGLE.as_bytes(),
            16,
        )
        .unwrap();
        assert_eq!(parsed.model.as_deref(), Some("gemini-3-flash"));
        assert_eq!(
            (parsed.prompt_tokens, parsed.gen_tokens),
            (Some(9), Some(3))
        );
        assert_eq!(parsed.stop_reason.as_deref(), Some("STOP"));
        assert_eq!(parsed.text, "Hi!");
        assert_eq!(
            parsed.ttft_ms, None,
            "a single object has no first-byte time"
        );
    }

    #[test]
    fn a_cut_stream_estimates_from_chunks_and_waits_for_usage_after_a_finish_reason() {
        let first = SSE.split("\r\n\r\n").next().unwrap().to_string() + "\r\n\r\n";
        let parsed =
            parse_all(Box::new(GeminiParser::new(Mode::Sse)), first.as_bytes(), 64).unwrap();
        assert_eq!(parsed.prompt_tokens, Some(7));
        assert_eq!(
            parsed.gen_tokens,
            Some(1),
            "one chunk, no candidate count yet"
        );
        assert!(parsed.tokens_estimated);
        assert_eq!(parsed.stop_reason.as_deref(), Some("unknown"));

        let mut parser = GeminiParser::new(Mode::Sse);
        parser.push(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"x\"}]},\"finishReason\":\"STOP\"}]}\r\n\r\n");
        assert!(
            parser.awaiting_trailer(),
            "finish reason seen, no usage yet"
        );
        parser.push(b"data: {\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1}}\r\n\r\n");
        assert!(!parser.awaiting_trailer());
    }

    #[test]
    fn google_errors_are_in_stream_errors_and_nothing_is_none() {
        let parsed = parse_all(
            Box::new(GeminiParser::new(Mode::Single)),
            ERROR.as_bytes(),
            16,
        )
        .unwrap();
        assert_eq!(
            parsed.in_stream_error.as_deref(),
            Some("API key not valid. Please pass a valid API key.")
        );
        assert_eq!(parsed.stop_reason, None);
        let framed = format!("data: {ERROR}\r\n\r\n");
        let parsed = parse_all(
            Box::new(GeminiParser::new(Mode::Sse)),
            framed.as_bytes(),
            16,
        )
        .unwrap();
        assert!(parsed.in_stream_error.is_some());
        assert!(parse_all(Box::new(GeminiParser::new(Mode::Sse)), b"", 16).is_none());
        assert!(parse_all(Box::new(GeminiParser::new(Mode::Single)), b"not json", 16).is_none());
    }
}
