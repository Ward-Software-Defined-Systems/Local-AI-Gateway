//! Google's Interactions API: `POST /v1beta/interactions`, which the agent application uses for image
//! generation. Without `stream`, the answer is one interaction object: `model`, `status`, `usage`
//! (`total_input_tokens`, `total_output_tokens`, `total_thought_tokens`, `total_tokens`, each with a
//! per-modality breakdown) and the model's work as `steps[]` (a `thought` step carrying only a signature,
//! a `model_output` step whose `content[]` parts are `text` or `image` with base64 `data`). With
//! `"stream": true` the answer is SSE: each frame's JSON names its `event_type`: `interaction.created`,
//! `step.start`, `step.delta` (a `delta` of type `text`, `image`, `thought_summary` with `content`,
//! `thought_signature` or `arguments_delta`), `step.stop`, `interaction.completed` with the final object
//! and its `usage`, `error`, then `done` with `[DONE]`. Output is counted like Gemini's other API: the
//! reply plus its reasoning. Text parts and thought summaries are reply text for the scanner; image bytes
//! are not. The shapes come from a live `gemini-3-pro-image` answer and Google's streaming documentation;
//! there is no capture in `fixtures/` yet.

use serde::Deserialize;
use serde_json::Value;

use super::sse_frames::FrameReader;
use super::{Parsed, SINGLE_BODY_CAP, StreamParser, TextSink, WallClock};
use crate::rejection::in_stream_error;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Interaction {
    model: Option<String>,
    status: Option<String>,
    usage: Option<Usage>,
    steps: Vec<Step>,
    outputs: Vec<Step>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Step {
    content: Vec<ContentPart>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ContentPart {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(default)]
struct Usage {
    total_input_tokens: Option<u64>,
    total_output_tokens: Option<u64>,
    total_thought_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

impl Usage {
    /// The reply and its reasoning, the way the other APIs count output; the total minus the input when
    /// neither count is present.
    fn generated(&self) -> Option<u64> {
        match (self.total_output_tokens, self.total_thought_tokens) {
            (None, None) => match (self.total_tokens, self.total_input_tokens) {
                (Some(total), Some(input)) => Some(total.saturating_sub(input)),
                _ => None,
            },
            (o, t) => Some(o.unwrap_or(0) + t.unwrap_or(0)),
        }
    }
}

/// One SSE frame's JSON.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Event {
    event_type: Option<String>,
    interaction: Option<Interaction>,
    delta: Option<Delta>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Delta {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    /// A `thought_summary` delta wraps its text in a content part.
    content: Option<ContentPart>,
}

pub struct InteractionsParser {
    streamed: bool,
    frames: FrameReader,
    buffered: Vec<u8>,
    overflow: bool,
    model: Option<String>,
    status: Option<String>,
    usage: Option<Usage>,
    /// Text deltas seen, the fallback output count when the stream ends without usage.
    deltas: u64,
    /// A step finished; the completion event with the counts is normally next.
    step_stopped: bool,
    completed: bool,
    error: Option<String>,
    text: TextSink,
}

impl InteractionsParser {
    pub fn new(streamed: bool) -> Self {
        Self {
            streamed,
            frames: FrameReader::default(),
            buffered: Vec::new(),
            overflow: false,
            model: None,
            status: None,
            usage: None,
            deltas: 0,
            step_stopped: false,
            completed: false,
            error: None,
            text: TextSink::default(),
        }
    }

    /// `final_status` is false for the `interaction.created` event, whose `in_progress` is not an outcome.
    fn take_interaction(&mut self, interaction: Interaction, final_status: bool) {
        if let Some(model) = interaction.model.filter(|m| !m.is_empty()) {
            self.model = Some(model);
        }
        if final_status && let Some(status) = interaction.status.filter(|s| !s.is_empty()) {
            self.status = Some(status);
        }
        if let Some(usage) = interaction.usage {
            self.usage = Some(usage);
        }
        for step in interaction.steps.into_iter().chain(interaction.outputs) {
            for part in step.content {
                if part.kind == "text"
                    && let Some(text) = &part.text
                {
                    self.text.push(text);
                }
            }
        }
    }

    fn take_frame(&mut self, data: &str) {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(message) = in_stream_error(&value) {
            self.error.get_or_insert(message);
            return;
        }
        let Ok(event) = serde_json::from_value::<Event>(value) else {
            return;
        };
        let completed = event.event_type.as_deref() == Some("interaction.completed");
        if completed {
            self.completed = true;
        } else if event.event_type.as_deref() == Some("step.stop") {
            self.step_stopped = true;
        }
        if let Some(interaction) = event.interaction {
            self.take_interaction(interaction, completed);
        }
        if let Some(delta) = event.delta {
            let text = match delta.kind.as_str() {
                "text" => delta.text,
                "thought_summary" => delta.content.and_then(|c| c.text),
                _ => None,
            };
            if let Some(text) = text {
                self.deltas += 1;
                self.text.push(&text);
            }
        }
    }

    fn take_single(&mut self, body: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return;
        };
        if let Some(message) = in_stream_error(&value) {
            self.error.get_or_insert(message);
            return;
        }
        if let Ok(interaction) = serde_json::from_value::<Interaction>(value) {
            self.take_interaction(interaction, true);
        }
    }
}

impl StreamParser for InteractionsParser {
    fn push(&mut self, chunk: &[u8]) {
        if self.streamed {
            self.frames.push(chunk);
            while let Some(frame) = self.frames.next_frame() {
                self.take_frame(&frame.data);
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

    /// A step has finished and the completion event with the counts has not arrived.
    fn awaiting_trailer(&self) -> bool {
        self.streamed && self.step_stopped && !self.completed && self.error.is_none()
    }

    fn finalize(mut self: Box<Self>, wall: &WallClock) -> Option<Parsed> {
        if self.streamed {
            if let Some(frame) = self.frames.finish() {
                self.take_frame(&frame.data);
            }
        } else if !self.overflow {
            let body = std::mem::take(&mut self.buffered);
            self.take_single(&body);
        }
        if self.model.is_none()
            && self.usage.is_none()
            && self.deltas == 0
            && self.status.is_none()
            && self.error.is_none()
        {
            return None;
        }
        let (text, text_truncated) = std::mem::take(&mut self.text).into_parts();
        let generated = self.usage.and_then(|u| u.generated());
        let streamed = self.streamed;
        let parsed = Parsed {
            model: self.model.clone(),
            prompt_tokens: self.usage.and_then(|u| u.total_input_tokens),
            gen_tokens: generated.or((self.deltas > 0).then_some(self.deltas)),
            tokens_estimated: generated.is_none(),
            stop_reason: self
                .status
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

    /// The structure of a live `gemini-3-pro-image` answer to a text-to-image request, with the signature
    /// and the image bytes cut to a few characters and a text part added to show the reply text.
    const IMAGE_ANSWER: &str = r#"{"id":"v1_abc","status":"completed","usage":{"total_tokens":1610,"total_input_tokens":95,"input_tokens_by_modality":[{"modality":"text","tokens":95}],"total_cached_tokens":0,"total_output_tokens":1276,"output_tokens_by_modality":[{"modality":"image","tokens":1120}],"total_tool_use_tokens":0,"total_thought_tokens":239},"created":"2026-10-25T19:17:09Z","updated":"2026-10-25T19:17:09Z","service_tier":"standard","steps":[{"signature":"c2ln","type":"thought"},{"content":[{"text":"Here is the lighthouse.","type":"text"},{"data":"/9j/4AAQ","mime_type":"image/jpeg","type":"image"}],"type":"model_output"}],"object":"interaction","model":"gemini-3-pro-image"}"#;

    /// Google's documented streaming sequence for a text answer.
    const STREAM: &str = "event: interaction.created\ndata: {\"interaction\":{\"id\":\"v1_abc\",\"model\":\"gemini-3.5-flash-lite\",\"status\":\"in_progress\"},\"event_type\":\"interaction.created\"}\n\n\
event: step.start\ndata: {\"index\":0,\"step\":{\"type\":\"thought\"},\"event_type\":\"step.start\"}\n\n\
event: step.delta\ndata: {\"index\":0,\"delta\":{\"type\":\"thought_summary\",\"content\":{\"type\":\"text\",\"text\":\"Greeting. \"}},\"event_type\":\"step.delta\"}\n\n\
event: step.delta\ndata: {\"index\":0,\"delta\":{\"type\":\"thought_signature\",\"signature\":\"c2ln\"},\"event_type\":\"step.delta\"}\n\n\
event: step.stop\ndata: {\"index\":0,\"event_type\":\"step.stop\"}\n\n\
event: step.start\ndata: {\"index\":1,\"step\":{\"type\":\"model_output\"},\"event_type\":\"step.start\"}\n\n\
event: step.delta\ndata: {\"index\":1,\"delta\":{\"type\":\"text\",\"text\":\"Hi \"},\"event_type\":\"step.delta\"}\n\n\
event: step.delta\ndata: {\"index\":1,\"delta\":{\"type\":\"text\",\"text\":\"there!\"},\"event_type\":\"step.delta\"}\n\n\
event: step.stop\ndata: {\"index\":1,\"event_type\":\"step.stop\"}\n\n\
event: interaction.completed\ndata: {\"interaction\":{\"id\":\"v1_abc\",\"status\":\"completed\",\"usage\":{\"total_tokens\":346,\"total_input_tokens\":11,\"total_output_tokens\":90,\"total_thought_tokens\":245},\"object\":\"interaction\",\"model\":\"gemini-3.5-flash-lite\"},\"event_type\":\"interaction.completed\"}\n\n\
event: done\ndata: [DONE]\n\n";

    #[test]
    fn a_single_interaction_counts_input_output_and_thoughts_and_keeps_the_text() {
        for chunk in [1, 7, 64, 4096] {
            let parsed = parse_all(
                Box::new(InteractionsParser::new(false)),
                IMAGE_ANSWER.as_bytes(),
                chunk,
            )
            .unwrap();
            assert_eq!(parsed.model.as_deref(), Some("gemini-3-pro-image"));
            assert_eq!(parsed.prompt_tokens, Some(95));
            assert_eq!(parsed.gen_tokens, Some(1276 + 239), "output plus thoughts");
            assert!(!parsed.tokens_estimated);
            assert_eq!(parsed.stop_reason.as_deref(), Some("completed"));
            assert_eq!(
                parsed.text, "Here is the lighthouse.",
                "image bytes are not text"
            );
            assert_eq!(
                parsed.ttft_ms, None,
                "a single object has no first-token time"
            );
            assert!(parsed.timings_estimated);
            assert_eq!(parsed.in_stream_error, None);
        }
    }

    #[test]
    fn a_stream_joins_text_and_summaries_and_takes_the_counts_from_the_completion() {
        for chunk in [1, 7, 64, 4096] {
            let parsed = parse_all(
                Box::new(InteractionsParser::new(true)),
                STREAM.as_bytes(),
                chunk,
            )
            .unwrap();
            assert_eq!(parsed.model.as_deref(), Some("gemini-3.5-flash-lite"));
            assert_eq!(parsed.prompt_tokens, Some(11));
            assert_eq!(parsed.gen_tokens, Some(90 + 245));
            assert!(!parsed.tokens_estimated);
            assert_eq!(parsed.stop_reason.as_deref(), Some("completed"));
            assert_eq!(parsed.text, "Greeting. Hi there!");
            assert!(parsed.ttft_ms.is_some());
        }
    }

    #[test]
    fn a_cut_stream_waits_for_the_completion_then_estimates() {
        let cut = STREAM.split("event: interaction.completed").next().unwrap();
        let mut parser = InteractionsParser::new(true);
        parser.push(cut.as_bytes());
        assert!(
            parser.awaiting_trailer(),
            "a step stopped and the counts have not arrived"
        );
        assert_eq!(parser.take_text_deltas().join(""), "Greeting. Hi there!");
        let wall = WallClock {
            started: std::time::Instant::now(),
            first_byte: None,
            ended: std::time::Instant::now(),
        };
        let parsed = Box::new(parser).finalize(&wall).unwrap();
        assert_eq!(parsed.gen_tokens, Some(3), "three text deltas");
        assert!(parsed.tokens_estimated);
        assert_eq!(parsed.prompt_tokens, None);
        assert_eq!(parsed.stop_reason.as_deref(), Some("unknown"));

        let mut fresh = InteractionsParser::new(true);
        assert!(!fresh.awaiting_trailer(), "nothing seen yet");
        fresh.push(STREAM.as_bytes());
        assert!(!fresh.awaiting_trailer(), "the completion arrived");
    }

    #[test]
    fn errors_are_reported_and_nothing_is_none() {
        let frame = "event: error\ndata: {\"error\":{\"message\":\"Deadline expired before operation could complete.\",\"code\":\"gateway_timeout\"},\"event_type\":\"error\"}\n\n";
        let parsed = parse_all(
            Box::new(InteractionsParser::new(true)),
            frame.as_bytes(),
            16,
        )
        .unwrap();
        assert_eq!(
            parsed.in_stream_error.as_deref(),
            Some("Deadline expired before operation could complete.")
        );
        assert_eq!(parsed.stop_reason, None);
        let single = r#"{"error":{"message":"The value 'image/png' is not supported for 'response_format.mime_type'.","code":"invalid_request"}}"#;
        let parsed = parse_all(
            Box::new(InteractionsParser::new(false)),
            single.as_bytes(),
            16,
        )
        .unwrap();
        assert!(
            parsed
                .in_stream_error
                .unwrap()
                .starts_with("The value 'image/png'")
        );
        assert!(parse_all(Box::new(InteractionsParser::new(false)), b"", 16).is_none());
        assert!(parse_all(Box::new(InteractionsParser::new(true)), b"\n\n", 16).is_none());
    }
}
