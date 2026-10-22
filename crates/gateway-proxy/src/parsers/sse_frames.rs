//! Server-sent event framing over `LineBuffer`: `event:` and `data:` lines accumulate until a blank line
//! ends the frame; several `data:` lines join with `\n`; comment lines (`:`) and unknown fields are
//! ignored. CRLF line endings work because the line buffer strips the `\r`. A final frame without its
//! blank line is flushed by `finish`.

use super::line_buffer::LineBuffer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct FrameReader {
    lines: LineBuffer,
    event: Option<String>,
    data: Vec<String>,
}

impl FrameReader {
    pub fn push(&mut self, chunk: &[u8]) {
        self.lines.push(chunk);
    }

    /// The next complete frame, or `None` until one is.
    pub fn next_frame(&mut self) -> Option<SseFrame> {
        while let Some(line) = self.lines.next_line() {
            if let Some(frame) = self.take_line(&line) {
                return Some(frame);
            }
        }
        None
    }

    /// Flushes a trailing frame that never got its blank line.
    pub fn finish(&mut self) -> Option<SseFrame> {
        if let Some(tail) = self.lines.take_tail()
            && let Some(frame) = self.take_line(&tail)
        {
            return Some(frame);
        }
        self.flush()
    }

    fn take_line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            return self.flush();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => self.data.push(value.to_string()),
            "event" => self.event = Some(value.to_string()),
            _ => {}
        }
        None
    }

    fn flush(&mut self) -> Option<SseFrame> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        let frame = SseFrame {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
        };
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames_of(body: &[u8], chunk: usize) -> Vec<SseFrame> {
        let mut reader = FrameReader::default();
        let mut out = Vec::new();
        for piece in body.chunks(chunk) {
            reader.push(piece);
            while let Some(frame) = reader.next_frame() {
                out.push(frame);
            }
        }
        out.extend(reader.finish());
        out
    }

    #[test]
    fn frames_split_on_blank_lines_with_lf_or_crlf_and_join_multiline_data() {
        let body = b"event: message_start\r\ndata: {\"a\":1}\r\n\r\n: keep-alive\n\ndata: line one\ndata: line two\n\ndata: [DONE]\n\n";
        for chunk in [1, 7, 64, 1024] {
            let frames = frames_of(body, chunk);
            assert_eq!(
                frames,
                vec![
                    SseFrame {
                        event: Some("message_start".into()),
                        data: "{\"a\":1}".into()
                    },
                    SseFrame {
                        event: None,
                        data: "line one\nline two".into()
                    },
                    SseFrame {
                        event: None,
                        data: "[DONE]".into()
                    },
                ],
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn a_final_frame_without_a_blank_line_is_flushed_by_finish() {
        let frames = frames_of(b"data: a\n\ndata: b", 3);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].data, "b");
        assert!(frames_of(b"", 1).is_empty());
        assert!(frames_of(b"\n\n\n", 1).is_empty());
    }
}
