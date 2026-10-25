//! Google's Gemini API: `/v1beta/models/{model}:generateContent` and `:streamGenerateContent`. Every
//! chunk is a `GenerateContentResponse`: `candidates[].content.parts[].text` (a part marked `thought` is
//! reasoning and counts as reply text, like the other parsers), `candidates[].finishReason` on the last
//! chunk, `usageMetadata` (cumulative, so the last one wins; `thoughtsTokenCount` is reasoning output) and
//! `modelVersion`. Three wire shapes: SSE frames with `?alt=sse`, one JSON object for `generateContent`,
//! and a JSON array written progressively for `streamGenerateContent` without `alt=sse`, which is buffered
//! and parsed at the end. An error is `{"error":{"code","message","status"}}`. The tests run on captures
//! from `gemini-3.5-flash-lite` (`fixtures/gemini`); a thought part in the text, which that model never
//! returned, is tested on the documented shape.

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

    const SSE: &[u8] = include_bytes!("../../../../fixtures/gemini/streamgeneratecontent-sse.txt");
    const SSE_THOUGHTS: &[u8] =
        include_bytes!("../../../../fixtures/gemini/streamgeneratecontent-sse-thoughts.txt");
    const ARRAY: &[u8] =
        include_bytes!("../../../../fixtures/gemini/streamgeneratecontent-array.json");
    const SINGLE: &[u8] =
        include_bytes!("../../../../fixtures/gemini/generatecontent-nonstream.json");
    const NOT_FOUND: &[u8] = include_bytes!("../../../../fixtures/gemini/404-bad-model.json");

    /// `gemini-3.5-flash-lite` answering "Say hi." over `?alt=sse`: two CRLF-framed chunks, cumulative
    /// usage on both, the finish reason on the last one beside an empty text part that carries only a
    /// `thoughtSignature`.
    #[test]
    fn fixture_sse_stream_yields_model_cumulative_usage_finish_reason_and_text() {
        for chunk in [1, 7, 64, 4096] {
            let parsed =
                parse_all(Box::new(GeminiParser::new(Mode::Sse)), SSE, chunk).expect("stats");
            assert_eq!(parsed.model.as_deref(), Some("gemini-3.5-flash-lite"));
            assert_eq!(
                (parsed.prompt_tokens, parsed.gen_tokens),
                (Some(3), Some(2))
            );
            assert!(!parsed.tokens_estimated);
            assert_eq!(parsed.stop_reason.as_deref(), Some("STOP"));
            assert_eq!(parsed.text, "Hi!", "the signature-only part adds no text");
            assert!(parsed.timings_estimated);
            assert!(parsed.ttft_ms.is_some());
        }
    }

    /// The same model at `thinkingLevel: high` with `includeThoughts`: the reasoning is billed as
    /// `thoughtsTokenCount` but no thought part comes back, so the count joins `gen_tokens` while the text
    /// is the answer alone.
    #[test]
    fn fixture_thinking_stream_counts_thoughts_as_output() {
        let parsed = parse_all(Box::new(GeminiParser::new(Mode::Sse)), SSE_THOUGHTS, 97).unwrap();
        assert_eq!(parsed.prompt_tokens, Some(16));
        assert_eq!(
            parsed.gen_tokens,
            Some(227),
            "3 candidate tokens plus 224 thought tokens"
        );
        assert!(!parsed.tokens_estimated);
        assert_eq!(parsed.stop_reason.as_deref(), Some("STOP"));
        assert_eq!(parsed.text, "391");
    }

    /// Without `alt=sse` the stream is a pretty-printed JSON array written progressively, parsed when
    /// complete; `generateContent` is the same object alone.
    #[test]
    fn fixture_array_stream_parses_at_the_end_and_the_single_object_as_one() {
        for chunk in [1, 13, 4096] {
            let parsed = parse_all(Box::new(GeminiParser::new(Mode::Array)), ARRAY, chunk).unwrap();
            assert_eq!(parsed.model.as_deref(), Some("gemini-3.5-flash-lite"));
            assert_eq!(
                (parsed.prompt_tokens, parsed.gen_tokens),
                (Some(3), Some(9))
            );
            assert!(!parsed.tokens_estimated);
            assert_eq!(parsed.stop_reason.as_deref(), Some("STOP"));
            assert_eq!(parsed.text, "Hi! How can I help you today?");
            assert!(
                parsed.ttft_ms.is_some(),
                "an array stream is still a stream"
            );
        }
        let parsed = parse_all(Box::new(GeminiParser::new(Mode::Single)), SINGLE, 16).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("gemini-3.5-flash-lite"));
        assert_eq!(
            (parsed.prompt_tokens, parsed.gen_tokens),
            (Some(3), Some(2))
        );
        assert_eq!(parsed.stop_reason.as_deref(), Some("STOP"));
        assert_eq!(parsed.text, "Hi!");
        assert_eq!(
            parsed.ttft_ms, None,
            "a single object has no first-byte time"
        );
    }

    /// A part marked `thought` (the documented shape when the model returns a reasoning summary; no
    /// capture shows one) is reply text like the other parsers' reasoning.
    #[test]
    fn thought_parts_are_reply_text() {
        let frame = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Let me think.\",\"thought\":true},{\"text\":\"Hello\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":1,\"thoughtsTokenCount\":5,\"totalTokenCount\":13}}\r\n\r\n";
        let parsed = parse_all(Box::new(GeminiParser::new(Mode::Sse)), frame, 64).unwrap();
        assert_eq!(parsed.text, "Let me think.Hello");
        assert_eq!(parsed.gen_tokens, Some(6));
        assert_eq!(parsed.stop_reason.as_deref(), Some("STOP"));
    }

    #[test]
    fn a_cut_stream_keeps_cumulative_counts_estimates_without_them_and_waits_for_usage() {
        // The first real frame alone: its counts are cumulative, so they stand; the stop reason is unknown.
        let end = SSE.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let parsed = parse_all(Box::new(GeminiParser::new(Mode::Sse)), &SSE[..end], 64).unwrap();
        assert_eq!(
            (parsed.prompt_tokens, parsed.gen_tokens),
            (Some(3), Some(2))
        );
        assert!(!parsed.tokens_estimated);
        assert_eq!(parsed.stop_reason.as_deref(), Some("unknown"));

        // A frame without usage (the documented shape; every captured frame carries it): the chunk count
        // stands in.
        let bare = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}],\"role\":\"model\"},\"index\":0}],\"modelVersion\":\"gemini-3.5-flash-lite\"}\r\n\r\n";
        let parsed = parse_all(Box::new(GeminiParser::new(Mode::Sse)), bare, 64).unwrap();
        assert_eq!(parsed.prompt_tokens, None);
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

    /// Google refuses an unknown model with a 404 whose body is the same error object a 200 stream would
    /// carry in a frame.
    #[test]
    fn fixture_google_errors_are_in_stream_errors_and_nothing_is_none() {
        let parsed = parse_all(Box::new(GeminiParser::new(Mode::Single)), NOT_FOUND, 16).unwrap();
        let message = parsed.in_stream_error.as_deref().unwrap();
        assert!(
            message.starts_with("models/gemini-no-such-model is not found"),
            "{message}"
        );
        assert_eq!(parsed.stop_reason, None);
        let compact =
            serde_json::to_string(&serde_json::from_slice::<Value>(NOT_FOUND).unwrap()).unwrap();
        let framed = format!("data: {compact}\r\n\r\n");
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
