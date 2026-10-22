//! The OpenAI Responses API (`/v1/responses`), served by both backends. Streaming is SSE with typed events
//! (`event:` line and/or `type` in the data): `response.created` carries the model;
//! `response.output_text.delta` carries text; `response.output_item.done` ends an item;
//! `response.completed` (or `response.incomplete`) carries `response.usage.input_tokens/output_tokens`
//! and `response.status`; `error` and `response.failed` carry refusals. Non-streaming is the response
//! object itself. Shapes follow the public API; both backends' real streams are to be captured as
//! fixtures (Phase 1) to confirm them.

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
    delta: Option<String>,
    response: Option<ResponseObject>,
    // The non-streaming object's own fields.
    model: Option<String>,
    status: Option<String>,
    usage: Option<Usage>,
    output: Option<Vec<OutputItem>>,
    incomplete_details: Option<Incomplete>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ResponseObject {
    model: Option<String>,
    status: Option<String>,
    usage: Option<Usage>,
    output: Option<Vec<OutputItem>>,
    incomplete_details: Option<Incomplete>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(default)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Incomplete {
    reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OutputItem {
    #[serde(rename = "type")]
    kind: Option<String>,
    content: Option<Vec<ContentPart>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ContentPart {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

pub struct ResponsesParser {
    streaming: bool,
    frames: FrameReader,
    single: Vec<u8>,
    single_overflow: bool,
    model: Option<String>,
    usage: Option<Usage>,
    status: Option<String>,
    incomplete_reason: Option<String>,
    deltas: u64,
    item_done: bool,
    completed: bool,
    error: Option<String>,
    text: TextSink,
}

impl ResponsesParser {
    pub fn new(streaming: bool) -> Self {
        Self {
            streaming,
            frames: FrameReader::default(),
            single: Vec::new(),
            single_overflow: false,
            model: None,
            usage: None,
            status: None,
            incomplete_reason: None,
            deltas: 0,
            item_done: false,
            completed: false,
            error: None,
            text: TextSink::default(),
        }
    }

    fn take_response_object(
        &mut self,
        model: Option<String>,
        status: Option<String>,
        usage: Option<Usage>,
        output: Option<Vec<OutputItem>>,
        incomplete: Option<Incomplete>,
        final_object: bool,
    ) {
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            self.model.get_or_insert(model);
        }
        if let Some(usage) = usage.filter(|u| u.input_tokens.is_some() || u.output_tokens.is_some())
        {
            self.usage = Some(usage);
        }
        // Only a final object says how the response ended; `response.created` carries `in_progress`.
        if final_object {
            if let Some(status) = status {
                self.status = Some(status);
            }
            if let Some(reason) = incomplete.and_then(|i| i.reason) {
                self.incomplete_reason = Some(reason);
            }
        }
        if final_object && self.text.is_empty() {
            for item in output.unwrap_or_default() {
                if item.kind.as_deref() == Some("message") {
                    for part in item.content.unwrap_or_default() {
                        if part.kind.as_deref() == Some("output_text")
                            && let Some(text) = &part.text
                        {
                            self.text.push(text);
                        }
                    }
                }
            }
        }
    }

    fn take_json(&mut self, event_name: Option<&str>, text: &str) {
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
        let Ok(event) = serde_json::from_value::<Event>(value) else {
            return;
        };
        let kind = event
            .kind
            .as_deref()
            .or(event_name)
            .unwrap_or("")
            .to_string();
        match kind.as_str() {
            "response.output_text.delta" => {
                if let Some(delta) = &event.delta {
                    self.text.push(delta);
                }
                self.deltas += 1;
            }
            "response.output_item.done"
            | "response.output_text.done"
            | "response.content_part.done" => {
                self.item_done = true;
            }
            "response.failed" => {
                self.error
                    .get_or_insert_with(|| "response failed".to_string());
                self.completed = true;
            }
            "response.created"
            | "response.in_progress"
            | "response.completed"
            | "response.incomplete" => {
                let is_final =
                    matches!(kind.as_str(), "response.completed" | "response.incomplete");
                if let Some(response) = event.response {
                    self.take_response_object(
                        response.model,
                        response.status,
                        response.usage,
                        response.output,
                        response.incomplete_details,
                        is_final,
                    );
                }
                if is_final {
                    self.completed = true;
                }
            }
            "response" | "" => {
                // The non-streaming response object.
                let final_object = event.response.is_none();
                match event.response {
                    Some(response) => self.take_response_object(
                        response.model,
                        response.status,
                        response.usage,
                        response.output,
                        response.incomplete_details,
                        true,
                    ),
                    None => self.take_response_object(
                        event.model,
                        event.status,
                        event.usage,
                        event.output,
                        event.incomplete_details,
                        final_object,
                    ),
                }
                self.completed = true;
            }
            _ => {}
        }
    }
}

impl StreamParser for ResponsesParser {
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

    /// The output items are done but `response.completed`, which carries the usage, has not arrived.
    fn awaiting_trailer(&self) -> bool {
        self.streaming && self.item_done && !self.completed && self.error.is_none()
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
        if self.model.is_none() && self.deltas == 0 && self.usage.is_none() && self.error.is_none()
        {
            return None;
        }
        let (text, text_truncated) = std::mem::take(&mut self.text).into_parts();
        let usage = self.usage.unwrap_or_default();
        let stop_reason = match self.status.as_deref() {
            Some("completed") => Some("completed".to_string()),
            Some("incomplete") => Some(
                self.incomplete_reason
                    .clone()
                    .unwrap_or_else(|| "incomplete".to_string()),
            ),
            Some(other) => Some(other.to_string()),
            None => (self.error.is_none() && self.streaming).then(|| "unknown".to_string()),
        };
        let parsed = Parsed {
            model: self.model.clone(),
            prompt_tokens: usage.input_tokens,
            gen_tokens: usage.output_tokens.or(Some(self.deltas)),
            tokens_estimated: usage.output_tokens.is_none(),
            stop_reason,
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

    /// The documented Responses stream shape (synthetic sample).
    const STREAM: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"model\":\"qwen3:14b\",\"status\":\"in_progress\",\"output\":[]}}\n\n\
        event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\n\
        event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"Hel\"}\n\n\
        event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"lo\"}\n\n\
        event: response.output_text.done\ndata: {\"type\":\"response.output_text.done\",\"output_index\":0,\"content_index\":0,\"text\":\"Hello\"}\n\n\
        event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}}\n\n\
        event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"qwen3:14b\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}],\"usage\":{\"input_tokens\":14,\"output_tokens\":2,\"total_tokens\":16}}}\n\n";

    #[test]
    fn the_stream_yields_model_usage_status_and_text_once() {
        for chunk in [1, 13, 256, 8192] {
            let parsed = parse_all(
                Box::new(ResponsesParser::new(true)),
                STREAM.as_bytes(),
                chunk,
            )
            .expect("stats");
            assert_eq!(parsed.model.as_deref(), Some("qwen3:14b"));
            assert_eq!(parsed.prompt_tokens, Some(14));
            assert_eq!(parsed.gen_tokens, Some(2));
            assert!(!parsed.tokens_estimated);
            assert_eq!(parsed.stop_reason.as_deref(), Some("completed"));
            assert_eq!(
                parsed.text, "Hello",
                "deltas are not doubled by the final output"
            );
        }
    }

    #[test]
    fn awaiting_trailer_between_item_done_and_completed_and_cut_streams_estimate() {
        let mut parser = ResponsesParser::new(true);
        let upto = STREAM.split("event: response.completed").next().unwrap();
        parser.push(upto.as_bytes());
        assert!(parser.awaiting_trailer());
        parser.push(&STREAM.as_bytes()[upto.len()..]);
        assert!(!parser.awaiting_trailer());
        let cut = STREAM
            .split("event: response.output_text.done")
            .next()
            .unwrap();
        let parsed = parse_all(Box::new(ResponsesParser::new(true)), cut.as_bytes(), 64).unwrap();
        assert!(parsed.tokens_estimated);
        assert_eq!(parsed.gen_tokens, Some(2));
        assert_eq!(parsed.stop_reason.as_deref(), Some("unknown"));
    }

    #[test]
    fn the_non_streaming_object_incomplete_status_and_errors_parse() {
        let single = br#"{"id":"resp_2","object":"response","model":"qwen3:14b","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Once upon"}]}],"usage":{"input_tokens":5,"output_tokens":3}}"#;
        let parsed = parse_all(Box::new(ResponsesParser::new(false)), single, 64).unwrap();
        assert_eq!(parsed.prompt_tokens, Some(5));
        assert_eq!(parsed.gen_tokens, Some(3));
        assert_eq!(parsed.stop_reason.as_deref(), Some("max_output_tokens"));
        assert_eq!(parsed.text, "Once upon");
        let error = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"model not found\"}}\n\n";
        let parsed = parse_all(Box::new(ResponsesParser::new(true)), error, 64).unwrap();
        assert_eq!(parsed.in_stream_error.as_deref(), Some("model not found"));
        assert!(parse_all(Box::new(ResponsesParser::new(true)), b"", 64).is_none());
    }
}
