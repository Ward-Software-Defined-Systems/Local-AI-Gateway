//! Line framing for byte streams cut wherever the network cut them. A boundary can fall inside a line and
//! inside a multi-byte character; decoding each chunk alone would turn the halves into replacement
//! characters. `LineBuffer` keeps bytes until a line is complete and decodes the line. A newline byte is
//! never part of a multi-byte sequence, so a cut there is always safe. Ported from an earlier in-house SSE reader.

/// Read bytes are dropped from the front once this many have piled up.
const COMPACT_AT: usize = 64 * 1024;

#[derive(Debug, Default)]
pub struct LineBuffer {
    pending: Vec<u8>,
    /// Where the unread part of `pending` starts.
    start: usize,
    /// No newline lies between `start` and here.
    scanned: usize,
}

impl LineBuffer {
    pub fn push(&mut self, chunk: &[u8]) {
        if self.start == self.pending.len() {
            self.pending.clear();
            self.start = 0;
            self.scanned = 0;
        } else if self.start >= COMPACT_AT {
            self.pending.drain(..self.start);
            self.scanned -= self.start;
            self.start = 0;
        }
        self.pending.extend_from_slice(chunk);
    }

    /// The next complete line without its `\n` and any `\r` before it. `None` until a line is complete;
    /// an unterminated tail stays for the next chunk. Bytes that are not UTF-8 decode to the replacement
    /// character within their own line.
    pub fn next_line(&mut self) -> Option<String> {
        let found = self.pending[self.scanned..]
            .iter()
            .position(|&b| b == b'\n');
        let Some(offset) = found else {
            self.scanned = self.pending.len();
            return None;
        };
        let newline = self.scanned + offset;
        let mut line = &self.pending[self.start..newline];
        while let [head @ .., b'\r'] = line {
            line = head;
        }
        let line = String::from_utf8_lossy(line).into_owned();
        self.start = newline + 1;
        self.scanned = self.start;
        Some(line)
    }

    /// The unterminated tail, if any, consumed: a body may end without a final newline.
    pub fn take_tail(&mut self) -> Option<String> {
        if self.start >= self.pending.len() {
            return None;
        }
        let mut tail = &self.pending[self.start..];
        while let [head @ .., b'\r'] = tail {
            tail = head;
        }
        let text = String::from_utf8_lossy(tail).into_owned();
        self.start = self.pending.len();
        self.scanned = self.start;
        (!text.is_empty()).then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(chunks: &[&[u8]]) -> Vec<String> {
        let mut buf = LineBuffer::default();
        let mut out = Vec::new();
        for chunk in chunks {
            buf.push(chunk);
            while let Some(line) = buf.next_line() {
                out.push(line);
            }
        }
        out
    }

    #[test]
    fn lines_come_out_whole_however_the_chunks_were_cut() {
        let body = b"data: one\n\ndata: two\r\n: keep-alive\ndata: three\n";
        let want = ["data: one", "", "data: two", ": keep-alive", "data: three"];
        assert_eq!(lines_of(&[body]), want);
        for cut in 0..=body.len() {
            assert_eq!(
                lines_of(&[&body[..cut], &body[cut..]]),
                want,
                "cut at {cut}"
            );
        }
        let bytes: Vec<&[u8]> = body.chunks(1).collect();
        assert_eq!(lines_of(&bytes), want);
    }

    #[test]
    fn a_character_cut_in_two_is_still_one_character() {
        let text = "data: caf\u{e9} \u{2014} \u{6f22} \u{1f600} end";
        let body = format!("{text}\n").into_bytes();
        for cut in 0..=body.len() {
            let got = lines_of(&[&body[..cut], &body[cut..]]);
            assert_eq!(got, [text], "cut at byte {cut}");
        }
    }

    #[test]
    fn an_unterminated_tail_waits_and_can_be_taken() {
        let mut buf = LineBuffer::default();
        buf.push(b"data: par");
        assert_eq!(buf.next_line(), None);
        buf.push(b"tial");
        assert_eq!(buf.next_line(), None);
        assert_eq!(buf.take_tail().as_deref(), Some("data: partial"));
        assert_eq!(buf.take_tail(), None);
        buf.push(b"x\r");
        assert_eq!(buf.take_tail().as_deref(), Some("x"));
    }

    #[test]
    fn a_long_stream_is_compacted_without_losing_a_byte() {
        let mut buf = LineBuffer::default();
        let mut got = 0usize;
        let mut carry = Vec::new();
        for i in 0..20_000usize {
            carry.extend_from_slice(format!("data: line {i:06} {}\n", "x".repeat(21)).as_bytes());
            let take = carry.len().min(7 + i % 13);
            let piece: Vec<u8> = carry.drain(..take).collect();
            buf.push(&piece);
            while let Some(line) = buf.next_line() {
                assert_eq!(line, format!("data: line {got:06} {}", "x".repeat(21)));
                got += 1;
            }
            assert!(
                buf.pending.len() < COMPACT_AT + 4096,
                "buffer grows without bound"
            );
        }
        buf.push(&carry);
        while buf.next_line().is_some() {
            got += 1;
        }
        assert_eq!(got, 20_000);
    }
}
