//! What the inspector keeps per response: the raw bytes up to a cap (with a truncated flag) and the chunk
//! timeline, `(offset_ms, bytes)` per chunk, capped by merging adjacent pairs so a very long stream still
//! costs bounded memory.

use std::time::Instant;

/// Timeline entries kept before adjacent pairs are merged.
pub const TIMELINE_CAP: usize = 8192;

#[derive(Debug, Default)]
pub struct BodyCapture {
    cap: usize,
    bytes: Vec<u8>,
    total: u64,
    truncated: bool,
}

impl BodyCapture {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            ..Default::default()
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        if self.bytes.len() + chunk.len() <= self.cap {
            self.bytes.extend_from_slice(chunk);
        } else {
            let room = self.cap.saturating_sub(self.bytes.len());
            self.bytes.extend_from_slice(&chunk[..room]);
            self.truncated = true;
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn into_bytes(self) -> (Vec<u8>, u64, bool) {
        (self.bytes, self.total, self.truncated)
    }
}

#[derive(Debug)]
pub struct Timeline {
    started: Instant,
    entries: Vec<(u32, u32)>,
}

impl Timeline {
    pub fn new(started: Instant) -> Self {
        Self {
            started,
            entries: Vec::new(),
        }
    }

    pub fn push(&mut self, at: Instant, bytes: usize) {
        let offset = at
            .saturating_duration_since(self.started)
            .as_millis()
            .min(u128::from(u32::MAX)) as u32;
        self.entries
            .push((offset, bytes.min(u32::MAX as usize) as u32));
        if self.entries.len() > TIMELINE_CAP {
            self.merge_pairs();
        }
    }

    /// Halves the resolution: each pair becomes one entry at the first's offset with both sizes.
    fn merge_pairs(&mut self) {
        let merged: Vec<(u32, u32)> = self
            .entries
            .chunks(2)
            .map(|pair| {
                (
                    pair[0].0,
                    pair.iter().map(|e| e.1).fold(0u32, u32::saturating_add),
                )
            })
            .collect();
        self.entries = merged;
    }

    pub fn into_entries(self) -> Vec<(u32, u32)> {
        self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_keeps_the_prefix_and_counts_everything() {
        let mut capture = BodyCapture::new(5);
        capture.push(b"abc");
        capture.push(b"defg");
        assert!(capture.truncated());
        assert_eq!(capture.total_bytes(), 7);
        let (bytes, total, truncated) = capture.into_bytes();
        assert_eq!(bytes, b"abcde");
        assert_eq!(total, 7);
        assert!(truncated);
    }

    #[test]
    fn the_timeline_merges_pairs_past_the_cap() {
        let started = Instant::now();
        let mut timeline = Timeline::new(started);
        for i in 0..(TIMELINE_CAP + 1) {
            timeline.push(started + std::time::Duration::from_millis(i as u64), 10);
        }
        assert!(timeline.len() <= TIMELINE_CAP / 2 + 1);
        let entries = timeline.into_entries();
        assert_eq!(
            entries[0],
            (0, 20),
            "two chunks merged into the first's offset"
        );
        let total: u32 = entries.iter().map(|e| e.1).sum();
        assert_eq!(total, (TIMELINE_CAP as u32 + 1) * 10, "no bytes lost");
    }
}
