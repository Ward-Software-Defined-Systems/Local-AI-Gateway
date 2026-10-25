//! Embedding responses: one JSON object, no text. OpenAI-compatible (`/v1/embeddings`, LM Studio
//! `/api/v0/embeddings`) reports `usage.prompt_tokens`; Ollama's `/api/embed` reports `prompt_eval_count`
//! and `total_duration` (older `/api/embeddings` reports neither, so the record keeps no counts); Gemini's
//! `:embedContent` reports `usageMetadata.promptTokenCount` beside the vector (`fixtures/gemini`).

use serde::Deserialize;
use serde_json::Value;

use super::{Parsed, SINGLE_BODY_CAP, StreamParser, WallClock};
use crate::rejection::in_stream_error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    OpenAi,
    Ollama,
    Gemini,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Body {
    model: Option<String>,
    usage: Option<Usage>,
    prompt_eval_count: Option<u64>,
    total_duration: Option<u64>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Usage {
    prompt_tokens: Option<u64>,
}

/// Gemini's usage object on an embedding answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct UsageMetadata {
    prompt_token_count: Option<u64>,
}

pub struct EmbeddingsParser {
    flavor: Flavor,
    buf: Vec<u8>,
    overflow: bool,
}

impl EmbeddingsParser {
    pub fn new(flavor: Flavor) -> Self {
        Self {
            flavor,
            buf: Vec::new(),
            overflow: false,
        }
    }
}

impl StreamParser for EmbeddingsParser {
    fn push(&mut self, chunk: &[u8]) {
        if self.buf.len() + chunk.len() <= SINGLE_BODY_CAP {
            self.buf.extend_from_slice(chunk);
        } else {
            self.overflow = true;
        }
    }

    fn take_text_deltas(&mut self) -> Vec<String> {
        Vec::new()
    }

    fn awaiting_trailer(&self) -> bool {
        false
    }

    fn finalize(self: Box<Self>, wall: &WallClock) -> Option<Parsed> {
        if self.overflow {
            return None;
        }
        let value: Value = serde_json::from_slice(&self.buf).ok()?;
        if let Some(message) = in_stream_error(&value) {
            return Some(Parsed {
                in_stream_error: Some(message),
                total_ms: wall.total_ms(),
                timings_estimated: true,
                ..Default::default()
            });
        }
        let body: Body = serde_json::from_value(value).ok()?;
        let mut parsed = Parsed {
            model: body.model.filter(|m| !m.is_empty()),
            gen_tokens: Some(0),
            stop_reason: Some("embeddings".to_string()),
            ..Default::default()
        };
        match self.flavor {
            Flavor::OpenAi => {
                parsed.prompt_tokens = body.usage.and_then(|u| u.prompt_tokens);
                parsed = parsed.with_wall_timings(wall, false);
            }
            Flavor::Ollama => {
                parsed.prompt_tokens = body.prompt_eval_count;
                parsed.total_ms = body
                    .total_duration
                    .map(|ns| ns as f64 / 1_000_000.0)
                    .unwrap_or_else(|| wall.total_ms());
                parsed.timings_estimated = body.total_duration.is_none();
            }
            Flavor::Gemini => {
                parsed.prompt_tokens = body.usage_metadata.and_then(|u| u.prompt_token_count);
                parsed = parsed.with_wall_timings(wall, false);
            }
        }
        Some(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::parse_all;

    #[test]
    fn fixture_lmstudio_embeddings_report_the_model_and_prompt_tokens() {
        const V1: &[u8] = include_bytes!("../../../../fixtures/lmstudio/v1-embeddings.json");
        const V0: &[u8] = include_bytes!("../../../../fixtures/lmstudio/api-v0-embeddings.json");
        for body in [V1, V0] {
            let parsed =
                parse_all(Box::new(EmbeddingsParser::new(Flavor::OpenAi)), body, 4096).unwrap();
            assert_eq!(
                parsed.model.as_deref(),
                Some("text-embedding-nomic-embed-text-v1.5")
            );
            // LM Studio 0.4.25 reports zero prompt tokens for embeddings; the count is kept as sent.
            assert_eq!(parsed.prompt_tokens, Some(0), "{parsed:?}");
        }
    }

    #[test]
    fn openai_and_ollama_embedding_bodies_report_prompt_tokens() {
        let openai = br#"{"object":"list","data":[{"object":"embedding","embedding":[0.1,0.2],"index":0}],"model":"text-embedding-nomic-embed-text-v1.5","usage":{"prompt_tokens":4,"total_tokens":4}}"#;
        let parsed =
            parse_all(Box::new(EmbeddingsParser::new(Flavor::OpenAi)), openai, 16).unwrap();
        assert_eq!(
            parsed.model.as_deref(),
            Some("text-embedding-nomic-embed-text-v1.5")
        );
        assert_eq!(parsed.prompt_tokens, Some(4));
        assert_eq!(parsed.gen_tokens, Some(0));
        assert_eq!(parsed.stop_reason.as_deref(), Some("embeddings"));
        assert!(parsed.timings_estimated);
        let ollama = br#"{"model":"nomic-embed-text","embeddings":[[0.1,0.2]],"total_duration":14143917,"load_duration":1019500,"prompt_eval_count":8}"#;
        let parsed =
            parse_all(Box::new(EmbeddingsParser::new(Flavor::Ollama)), ollama, 16).unwrap();
        assert_eq!(parsed.prompt_tokens, Some(8));
        assert!((parsed.total_ms - 14.143917).abs() < 1e-9);
        assert!(!parsed.timings_estimated);
        let legacy = br#"{"embedding":[0.1,0.2]}"#;
        let parsed =
            parse_all(Box::new(EmbeddingsParser::new(Flavor::Ollama)), legacy, 16).unwrap();
        assert_eq!(parsed.prompt_tokens, None);
        assert!(
            parse_all(
                Box::new(EmbeddingsParser::new(Flavor::OpenAi)),
                b"not json",
                16
            )
            .is_none()
        );
        let error = br#"{"error":{"message":"model not loaded"}}"#;
        let parsed = parse_all(Box::new(EmbeddingsParser::new(Flavor::OpenAi)), error, 16).unwrap();
        assert_eq!(parsed.in_stream_error.as_deref(), Some("model not loaded"));
    }

    /// `gemini-embedding-2` answering `:embedContent` (16 dimensions asked for): the vector and a
    /// `usageMetadata` with the prompt size, no model name.
    #[test]
    fn fixture_gemini_embeddings_report_prompt_tokens() {
        const EMBED: &[u8] = include_bytes!("../../../../fixtures/gemini/embedcontent.json");
        let parsed =
            parse_all(Box::new(EmbeddingsParser::new(Flavor::Gemini)), EMBED, 4096).unwrap();
        assert_eq!(
            parsed.prompt_tokens,
            Some(3),
            "usageMetadata.promptTokenCount"
        );
        assert_eq!(parsed.gen_tokens, Some(0));
        assert_eq!(parsed.stop_reason.as_deref(), Some("embeddings"));
        assert_eq!(parsed.model, None, "the answer does not name the model");
        assert!(parsed.timings_estimated);
        // The documented minimum, a vector alone, keeps no count.
        let bare = br#"{"embedding":{"values":[0.013,-0.008,0.021]}}"#;
        let parsed = parse_all(Box::new(EmbeddingsParser::new(Flavor::Gemini)), bare, 16).unwrap();
        assert_eq!(parsed.prompt_tokens, None);
        assert_eq!(parsed.gen_tokens, Some(0));
        let error =
            br#"{"error":{"code":400,"message":"API key not valid.","status":"INVALID_ARGUMENT"}}"#;
        let parsed = parse_all(Box::new(EmbeddingsParser::new(Flavor::Gemini)), error, 16).unwrap();
        assert_eq!(
            parsed.in_stream_error.as_deref(),
            Some("API key not valid.")
        );
    }
}
