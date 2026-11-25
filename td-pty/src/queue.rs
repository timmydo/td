//! The bounded keyboard-input queue between a terminal's encoders and the
//! PTY writer, which drains it through `Input`.

use std::collections::VecDeque;

/// The keyboard-input ceiling. A sequence is admitted whole or not at all.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// The bounded keyboard-input queue between the adapter and the PTY writer.
///
/// A sequence is admitted whole or dropped whole: half a `CSI` arriving at the
/// child would be worse than the key never having been pressed, so an
/// overflowing queue rings the visual bell instead of truncating.
pub struct InputQueue {
    bytes: VecDeque<u8>,
    capacity: usize,
    dropped: bool,
}

impl Default for InputQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl InputQueue {
    pub fn new() -> InputQueue {
        InputQueue::with_capacity(MAX_INPUT_BYTES)
    }

    pub fn with_capacity(capacity: usize) -> InputQueue {
        InputQueue {
            bytes: VecDeque::with_capacity(capacity.min(4096)),
            capacity,
            dropped: false,
        }
    }

    /// `false` when the whole sequence was dropped, which also marks the bell.
    pub fn push(&mut self, sequence: &[u8]) -> bool {
        if sequence.is_empty() {
            return true;
        }
        let admitted = self
            .bytes
            .len()
            .checked_add(sequence.len())
            .is_some_and(|total| total <= self.capacity);
        if !admitted {
            self.dropped = true;
            return false;
        }
        self.bytes.extend(sequence.iter().copied());
        true
    }

    /// The next bytes to write, still owned by the queue. The writer consumes
    /// what it actually wrote: draining first would lose the remainder when a
    /// write fails partway, and those bytes are keystrokes with nowhere to
    /// come back from.
    pub fn front(&mut self, limit: usize) -> &[u8] {
        self.bytes.make_contiguous();
        let count = limit.min(self.bytes.len());
        self.bytes.as_slices().0.get(..count).unwrap_or(&[])
    }

    pub fn consume(&mut self, count: usize) {
        self.bytes.drain(..count.min(self.bytes.len()));
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Whether a sequence has been dropped since this was last asked.
    pub fn take_dropped(&mut self) -> bool {
        std::mem::take(&mut self.dropped)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn the_input_queue_admits_or_drops_a_sequence_whole() {
        let mut queue = InputQueue::with_capacity(4);
        assert!(queue.push(b"ab"));
        assert!(queue.push(b"cd"));
        assert!(!queue.push(b"e"));
        assert_eq!(queue.len(), 4);
        assert!(queue.take_dropped());
        assert!(!queue.take_dropped());
        assert_eq!(queue.front(3), b"abc");
        // Looking is not taking: a writer that fails partway leaves the rest.
        assert_eq!(queue.len(), 4);
        queue.consume(3);
        assert_eq!(queue.len(), 1);
        // Space freed by the writer admits the next sequence.
        assert!(queue.push(b"xyz"));
        assert_eq!(queue.front(usize::MAX), b"dxyz");
        queue.consume(usize::MAX);
        assert!(queue.is_empty());
        assert_eq!(queue.front(4), b"");
        assert!(queue.push(b""));
    }

    #[test]
    fn a_sequence_larger_than_the_queue_is_refused_not_split() {
        let mut queue = InputQueue::with_capacity(2);
        assert!(!queue.push(b"\x1b[24~"));
        assert!(queue.is_empty());
        assert!(queue.take_dropped());
    }
}
