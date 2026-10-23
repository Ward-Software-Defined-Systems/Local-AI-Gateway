//! Error bodies: the one-line summary kept on a failed record, and LM Studio's context-overflow refusal
//! parsed into the prompt size and context length so the feed can show `rejected (ctx N)`. The message
//! patterns are LMS-Monitor's, which read them from the server log; here they arrive as a 4xx or 5xx body
//! or as an error frame inside an open 200 stream. LM Studio 0.4.25 sends the "tokens to keep" wording
//! with no numbers at all, so the pipeline takes the context length from the model list.

use serde_json::Value;

/// A server's error message is cut here before it is kept.
pub const ERROR_MESSAGE_MAX: usize = 256;

/// The message of an error body: `error.message` when that is a string, `error` itself when it is one,
/// otherwise the first line of the text. Cut at `ERROR_MESSAGE_MAX` on a character boundary.
pub fn error_summary(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let message = match serde_json::from_str::<Value>(text.trim()) {
        Ok(json) => in_stream_error(&json).or_else(|| {
            json.get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        Err(_) => text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string),
    }?;
    let message = message.trim();
    (!message.is_empty()).then(|| truncate(message, ERROR_MESSAGE_MAX).to_string())
}

/// The message of an error frame, or `None` for anything else. An OpenAI-compatible server can answer
/// inside an open 200 stream with one `{"error": {"message": "…"}}` frame and close; LM Studio does so for
/// a prompt over its context length. The message is `error.message` when that is a string, `error` itself
/// when it is one, otherwise the `error` value as JSON; a null `error` is no error.
pub fn in_stream_error(frame: &Value) -> Option<String> {
    let error = frame.as_object()?.get("error")?;
    let message = match error {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        Value::Object(fields) => match fields.get("message") {
            Some(Value::String(s)) => s.clone(),
            _ => error.to_string(),
        },
        other => other.to_string(),
    };
    Some(truncate(&message, 1024).to_string())
}

/// `(input_tokens, context_length)` when `message` is one of the engines' context-overflow errors.
pub fn parse_overflow(message: &str) -> Option<(Option<u64>, Option<u64>)> {
    // MLX engine: "Input does not fit in context length. The input has 359277 tokens, but the context
    // length only supports 262144 tokens."
    if message.contains("Input does not fit in context length") {
        return Some((
            number_before_tokens(message, "The input has "),
            number_before_tokens(message, "only supports "),
        ));
    }
    // llama.cpp engine (Linux llmster): "request (94920 tokens) exceeds the available context size
    // (8192 tokens), try increasing it", sometimes wrapped in an "Engine protocol predict request
    // returned 400: {json}" line.
    if message.contains("exceeds the available context size") {
        return Some((
            number_before_tokens(message, "request ("),
            number_before_tokens(message, "available context size ("),
        ));
    }
    // Any engine, when the overflow policy cannot keep the start of the prompt: "(n_keep: 13055 >=
    // n_ctx: 4096)"; n_keep is not the whole prompt, so only the context length is taken. LM Studio
    // 0.4.25 (MLX) sends this wording without the numbers (fixtures `*-context-overflow.*`).
    if message.contains("tokens to keep from the initial prompt is greater than the context length")
    {
        return Some((None, number_after(message, "n_ctx: ")));
    }
    None
}

/// The number right after `label`, provided ` tokens` follows it, so a message cut mid-number cannot yield
/// a smaller one.
fn number_before_tokens(message: &str, label: &str) -> Option<u64> {
    let (number, rest) = digits_after(message, label)?;
    rest.starts_with(" tokens").then(|| number.parse().ok())?
}

/// The number right after `label`, provided something other than a digit follows it.
fn number_after(message: &str, label: &str) -> Option<u64> {
    let (number, rest) = digits_after(message, label)?;
    (!rest.is_empty()).then(|| number.parse().ok())?
}

fn digits_after<'a>(message: &'a str, label: &str) -> Option<(&'a str, &'a str)> {
    let (_, after) = message.split_once(label)?;
    let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    (digits > 0).then(|| after.split_at(digits))
}

/// At most `max` bytes, cut on a character boundary.
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_ollama_error_bodies_summarise_to_their_message() {
        let native = include_bytes!("../../../fixtures/ollama/api-embed-400-unsupported.json");
        assert_eq!(
            error_summary(native).as_deref(),
            Some("input after truncation exceeds maximum context length")
        );
        let legacy = include_bytes!("../../../fixtures/ollama/api-embeddings-500-unsupported.json");
        assert_eq!(
            error_summary(legacy).as_deref(),
            Some("this model does not support embeddings")
        );
        let openai = include_bytes!("../../../fixtures/ollama/v1-embeddings-400-unsupported.json");
        assert_eq!(
            error_summary(openai).as_deref(),
            Some("input after truncation exceeds maximum context length")
        );
    }

    #[test]
    fn fixture_lmstudio_overflow_bodies_are_rejections_without_numbers() {
        // LM Studio 0.4.25 (MLX), a 6000-token prompt into a 2048-token context: 400 on the OpenAI and
        // native chat and completion routes, 500 on /v1/messages and /v1/responses, one wording for all.
        let bodies: [&[u8]; 5] = [
            include_bytes!("../../../fixtures/lmstudio/v1-chat-400-context-overflow.json"),
            include_bytes!("../../../fixtures/lmstudio/api-v0-chat-400-context-overflow.json"),
            include_bytes!("../../../fixtures/lmstudio/v1-completions-400-context-overflow.json"),
            include_bytes!("../../../fixtures/lmstudio/v1-messages-500-context-overflow.json"),
            include_bytes!("../../../fixtures/lmstudio/v1-responses-500-context-overflow.json"),
        ];
        for body in bodies {
            let summary = error_summary(body).unwrap();
            assert!(
                summary.starts_with(
                    "The number of tokens to keep from the initial prompt is greater than the context length"
                ),
                "{summary}"
            );
            assert_eq!(parse_overflow(&summary), Some((None, None)));
        }
    }

    const MLX: &str = "Input does not fit in context length. The input has 359277 tokens, but the context length only supports 262144 tokens.";
    const LLAMA: &str = "request (94920 tokens) exceeds the available context size (8192 tokens), try increasing it";

    #[test]
    fn overflow_messages_yield_their_numbers() {
        assert_eq!(parse_overflow(MLX), Some((Some(359_277), Some(262_144))));
        assert_eq!(parse_overflow(LLAMA), Some((Some(94_920), Some(8192))));
        assert_eq!(
            parse_overflow(
                "Engine protocol predict request returned 400: {\"error\":{\"message\":\"request (94920 tokens) exceeds the available context size (8192 tokens), try increasing it\"}}"
            ),
            Some((Some(94_920), Some(8192)))
        );
        assert_eq!(
            parse_overflow(
                "the number of tokens to keep from the initial prompt is greater than the context length (n_keep: 13055 >= n_ctx: 4096)"
            ),
            Some((None, Some(4096)))
        );
        assert_eq!(
            parse_overflow("Input does not fit in context length. The input has 3592"),
            Some((None, None)),
            "a cut-off number is not a number"
        );
        assert_eq!(parse_overflow("model 'qwen9' not found"), None);
    }

    #[test]
    fn error_summaries_come_from_json_or_the_first_text_line() {
        assert_eq!(
            error_summary(br#"{"error":{"message":"too many requests","type":"api_error"}}"#)
                .as_deref(),
            Some("too many requests")
        );
        assert_eq!(
            error_summary(br#"{"error":"model 'qwen9' not found"}"#).as_deref(),
            Some("model 'qwen9' not found")
        );
        assert_eq!(
            error_summary(br#"{"message":"bad things"}"#).as_deref(),
            Some("bad things")
        );
        assert_eq!(
            error_summary(b"\n  upstream unreachable\nmore").as_deref(),
            Some("upstream unreachable")
        );
        assert_eq!(error_summary(b""), None);
        assert_eq!(error_summary(br#"{"error": null}"#), None);
        let long = format!(
            "{{\"error\":\"{}\u{e9}\"}}",
            "x".repeat(ERROR_MESSAGE_MAX - 1)
        );
        assert_eq!(
            error_summary(long.as_bytes()).unwrap().len(),
            ERROR_MESSAGE_MAX - 1,
            "cut on a character boundary"
        );
    }

    #[test]
    fn in_stream_errors_follow_the_shapes_servers_use() {
        use serde_json::json;
        assert_eq!(
            in_stream_error(&json!({"error": {"message": MLX}})).as_deref(),
            Some(MLX)
        );
        assert_eq!(
            in_stream_error(&json!({"error": "slot busy"})).as_deref(),
            Some("slot busy")
        );
        assert_eq!(
            in_stream_error(&json!({"error": {"code": 503, "type": "server_error"}})).as_deref(),
            Some(r#"{"code":503,"type":"server_error"}"#)
        );
        assert_eq!(in_stream_error(&json!({"error": null})), None);
        assert_eq!(
            in_stream_error(&json!({"choices": [], "usage": {"total_tokens": 9}})),
            None
        );
        assert_eq!(in_stream_error(&json!("error")), None);
    }
}
