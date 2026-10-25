//! The parser for a scanned non-inference request to a cloud provider (`CloudPassthrough`): it reads no
//! counts or timings out of the response, but keeps the response text so the DLP response scan sees it
//! and the inspector can show it. Anything that arrives is an answer; nothing at all is `None`.

use super::{Parsed, StreamParser, TextSink, WallClock};

#[derive(Debug, Default)]
pub struct RawTextParser {
    text: TextSink,
    any: bool,
}

impl RawTextParser {
    pub fn new() -> Self {
        Self::default()
    }
}

impl StreamParser for RawTextParser {
    fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.any = true;
        self.text.push(&String::from_utf8_lossy(chunk));
    }

    fn take_text_deltas(&mut self) -> Vec<String> {
        self.text.take()
    }

    fn awaiting_trailer(&self) -> bool {
        false
    }

    fn finalize(mut self: Box<Self>, wall: &WallClock) -> Option<Parsed> {
        if !self.any {
            return None;
        }
        let (text, text_truncated) = std::mem::take(&mut self.text).into_parts();
        Some(
            Parsed {
                text,
                text_truncated,
                ..Default::default()
            }
            .with_wall_timings(wall, false),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsers::parse_all;

    #[test]
    fn the_raw_parser_keeps_the_text_and_reports_no_counts() {
        // Gemini's answer to `:countTokens`, which the gateway forwards as cloud passthrough.
        let body = include_bytes!("../../../../fixtures/gemini/counttokens.json");
        let parsed = parse_all(Box::new(RawTextParser::new()), body, 5).unwrap();
        assert_eq!(parsed.text.as_bytes(), &body[..]);
        assert_eq!(parsed.prompt_tokens, None);
        assert_eq!(parsed.gen_tokens, None);
        assert_eq!(parsed.stop_reason, None);
        assert_eq!(parsed.ttft_ms, None);
        assert!(parsed.timings_estimated);
        assert!(parse_all(Box::new(RawTextParser::new()), b"", 5).is_none());
    }
}
