//! Input ring buffer for frame history.
//!
//! Stores the last [`BUFFER_SIZE`] frames of input so that the command matcher
//! can scan backward through recent history to detect special-move sequences.

use crate::state::InputState;

/// Maximum number of frames stored in the ring buffer.
const BUFFER_SIZE: usize = 60;

/// Ring buffer storing the last 60 frames of input.
///
/// New frames are pushed each tick via [`InputBuffer::push`]. Older frames can
/// be retrieved with [`InputBuffer::get`], where `0` is the most recent frame
/// and higher values go further into the past.
pub struct InputBuffer {
    /// Fixed-size storage for input frames.
    frames: [InputState; BUFFER_SIZE],
    /// Index where the next frame will be written.
    head: usize,
    /// Number of frames currently stored (up to `BUFFER_SIZE`).
    count: usize,
}

impl InputBuffer {
    /// Creates a new empty input buffer with default (neutral) input states.
    pub fn new() -> Self {
        Self {
            frames: [InputState::default(); BUFFER_SIZE],
            head: 0,
            count: 0,
        }
    }

    /// Pushes a new input frame into the buffer.
    ///
    /// Called once per game tick. When the buffer is full, the oldest frame is
    /// overwritten.
    pub fn push(&mut self, state: InputState) {
        self.frames[self.head] = state;
        self.head = (self.head + 1) % BUFFER_SIZE;
        if self.count < BUFFER_SIZE {
            self.count += 1;
        }
    }

    /// Returns the input state from `frames_ago` frames in the past.
    ///
    /// `0` returns the most recently pushed frame. Returns `None` if
    /// `frames_ago` exceeds the number of stored frames.
    pub fn get(&self, frames_ago: usize) -> Option<&InputState> {
        if frames_ago >= self.count {
            return None;
        }
        // head points to the *next* write slot, so the most recent frame
        // is at (head - 1). Going `frames_ago` further back:
        let idx = (self.head + BUFFER_SIZE - 1 - frames_ago) % BUFFER_SIZE;
        Some(&self.frames[idx])
    }

    /// Returns the number of frames currently stored in the buffer.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns `true` if no frames have been pushed yet.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Button, Direction};

    #[test]
    fn push_and_get() {
        let mut buf = InputBuffer::new();
        let mut state = InputState::default();
        state.set_button(Button::A, true);
        buf.push(state);

        let retrieved = buf.get(0).unwrap();
        assert!(retrieved.button(Button::A));
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn buffer_wrapping() {
        let mut buf = InputBuffer::new();
        // Push 70 frames, each with a different direction pattern
        for i in 0..70u8 {
            let state = InputState {
                direction: Direction {
                    up: i % 2 == 0,
                    ..Default::default()
                },
                ..Default::default()
            };
            buf.push(state);
        }
        // Buffer should be full at 60
        assert_eq!(buf.len(), 60);
        // Most recent frame is i=69 (odd), so up should be false
        assert!(!buf.get(0).unwrap().direction.up);
        // One frame ago is i=68 (even), so up should be true
        assert!(buf.get(1).unwrap().direction.up);
        // The oldest accessible frame is 59 frames ago (i=10, even)
        assert!(buf.get(59).unwrap().direction.up);
    }

    #[test]
    fn get_out_of_range() {
        let mut buf = InputBuffer::new();
        // Fill the buffer completely
        for _ in 0..60 {
            buf.push(InputState::default());
        }
        assert_eq!(buf.len(), 60);
        // Frame 60 ago is out of range (valid range is 0..59)
        assert!(buf.get(60).is_none());
    }
}
