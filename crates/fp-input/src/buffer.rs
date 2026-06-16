//! Input ring buffer for frame history.
//!
//! Stores the last `BUFFER_SIZE` (60) frames of input so that the command
//! matcher can scan backward through recent history to detect special-move
//! sequences.

use crate::state::InputState;
use serde::{Deserialize, Serialize};

/// Maximum number of frames stored in the ring buffer.
const BUFFER_SIZE: usize = 60;

/// A serializable snapshot of an [`InputBuffer`]'s contents (replay / rollback,
/// #38).
///
/// Captures the live frames **oldest-first** (so it is layout-independent of the
/// ring's internal `head`), letting [`InputBuffer::restore_snapshot`] rebuild an
/// equivalent buffer by replaying the pushes. Used by `fp-engine`'s whole-Match
/// save-state to restore each player's command-recognition history exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputBufferSnapshot {
    /// The stored frames, **oldest first** (index `len-1` is the most recent).
    /// At most [`BUFFER_SIZE`] entries.
    frames: Vec<InputState>,
}

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

    /// Captures the buffer's contents as a serializable
    /// [`InputBufferSnapshot`] (replay / rollback, #38).
    ///
    /// The frames are emitted **oldest-first** so the snapshot is independent of
    /// the ring's internal write head; [`restore_snapshot`](Self::restore_snapshot)
    /// rebuilds an equivalent buffer from it.
    #[must_use]
    pub fn snapshot(&self) -> InputBufferSnapshot {
        // Walk from the oldest stored frame (offset `count-1`) to the newest
        // (offset 0), so a replay of pushes reproduces the same ring contents.
        let mut frames = Vec::with_capacity(self.count);
        for ago in (0..self.count).rev() {
            if let Some(s) = self.get(ago) {
                frames.push(*s);
            }
        }
        InputBufferSnapshot { frames }
    }

    /// Restores the buffer from an [`InputBufferSnapshot`].
    ///
    /// Clears the buffer and re-pushes the snapshot's frames oldest-first, so the
    /// resulting buffer is equivalent to the one the snapshot was taken from. A
    /// snapshot carrying more than [`BUFFER_SIZE`] frames is tolerated — the
    /// excess oldest frames are simply evicted by the ring, exactly as if they had
    /// been pushed live (never panics).
    pub fn restore_snapshot(&mut self, snap: &InputBufferSnapshot) {
        self.frames = [InputState::default(); BUFFER_SIZE];
        self.head = 0;
        self.count = 0;
        for frame in &snap.frames {
            self.push(*frame);
        }
    }

    /// Returns `true` if a **fresh up-press** (the rising edge of the up
    /// direction) occurred within the last `window` frames — the input-layer
    /// primitive behind the jump buffer (T075).
    ///
    /// A jump should still come out if the player tapped up a few frames *before*
    /// they could act (e.g. during landing recovery). Rather than require up to be
    /// held on the exact actionable frame, the command matcher scans this short
    /// window for an unconsumed up-press. The edge (a frame whose `up` is set while
    /// the immediately preceding frame's `up` was not) is detected so a single tap
    /// counts once and a continuously-held up does not silently re-arm forever.
    ///
    /// `window` is clamped to the number of stored frames; `0` always returns
    /// `false`. The scan is over the most-recent `window` frames (offsets
    /// `0..window`), so it is independent of the ring's internal layout and is
    /// fully deterministic.
    #[must_use]
    pub fn up_pressed_within(&self, window: usize) -> bool {
        for ago in 0..window.min(self.count) {
            let Some(frame) = self.get(ago) else { break };
            if !frame.direction.up {
                continue;
            }
            // `up` is held at this frame; it is a *fresh* press only if the
            // immediately older frame did not also hold up. A missing older
            // frame (the oldest stored input) counts as a press, matching the
            // matcher's first-frame press convention.
            match self.get(ago + 1) {
                Some(prev) if prev.direction.up => continue,
                _ => return true,
            }
        }
        false
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

    // -- Proctor: additional buffer-layer coverage ------------------------

    #[test]
    fn empty_buffer_invariants() {
        let buf = InputBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        // Any get on an empty buffer is None, never a panic or stale default.
        assert!(buf.get(0).is_none());
        assert!(buf.get(1).is_none());
        assert!(buf.get(usize::MAX).is_none());
    }

    #[test]
    fn default_matches_new() {
        // `Default` must produce the same empty buffer as `new()`.
        let buf = InputBuffer::default();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert!(buf.get(0).is_none());
    }

    #[test]
    fn single_push_boundary() {
        let mut buf = InputBuffer::new();
        let mut s = InputState::default();
        s.set_button(Button::C, true);
        buf.push(s);

        assert!(!buf.is_empty());
        assert_eq!(buf.len(), 1);
        // Offset 0 is the only valid frame; offset 1 is one-past-the-end.
        assert!(buf.get(0).unwrap().button(Button::C));
        assert!(buf.get(1).is_none());
    }

    #[test]
    fn ordering_is_lifo_by_offset() {
        // Frame N pushed last must be at offset 0; ordering must be strictly
        // most-recent-first as the matcher relies on this for backward scans.
        let mut buf = InputBuffer::new();
        // Encode a distinct, recognizable payload per frame via Start button on
        // even frames only.
        for i in 0..5u8 {
            let mut s = InputState::default();
            s.set_button(Button::Start, i % 2 == 0);
            buf.push(s);
        }
        // Pushed i = 0,1,2,3,4. Most recent (offset 0) is i=4 (even => Start).
        assert!(buf.get(0).unwrap().button(Button::Start)); // i=4
        assert!(!buf.get(1).unwrap().button(Button::Start)); // i=3
        assert!(buf.get(2).unwrap().button(Button::Start)); // i=2
        assert!(!buf.get(3).unwrap().button(Button::Start)); // i=1
        assert!(buf.get(4).unwrap().button(Button::Start)); // i=0
    }

    #[test]
    fn exact_capacity_then_overflow_evicts_oldest() {
        let mut buf = InputBuffer::new();
        // Push exactly 60: oldest (i=0) is reachable at offset 59.
        for i in 0..60u32 {
            let s = InputState {
                direction: Direction {
                    up: i == 0, // mark only the very first frame
                    ..Default::default()
                },
                ..Default::default()
            };
            buf.push(s);
        }
        assert_eq!(buf.len(), 60);
        assert!(buf.get(59).unwrap().direction.up, "oldest frame still i=0");

        // One more push evicts i=0; the frame at offset 59 is now i=1 (no up).
        buf.push(InputState::default());
        assert_eq!(buf.len(), 60, "len saturates at capacity");
        assert!(
            !buf.get(59).unwrap().direction.up,
            "oldest frame i=0 must have been evicted"
        );
        assert!(buf.get(60).is_none());
    }

    // -- T075: jump-buffer primitive --------------------------------------

    /// Helper: push an up-held or neutral frame.
    fn push_up(buf: &mut InputBuffer, up: bool) {
        buf.push(InputState {
            direction: Direction {
                up,
                ..Default::default()
            },
            ..Default::default()
        });
    }

    #[test]
    fn up_pressed_within_detects_recent_edge() {
        let mut buf = InputBuffer::new();
        // up-press 3 frames ago, then released and neutral since.
        push_up(&mut buf, false); // older
        push_up(&mut buf, true); // <- the fresh press (rising edge)
        push_up(&mut buf, false);
        push_up(&mut buf, false); // most recent (offset 0)

        // The press sits at offset 2; a 4-frame window catches it.
        assert!(buf.up_pressed_within(4));
        // A window too short to reach offset 2 must miss it.
        assert!(!buf.up_pressed_within(2));
    }

    #[test]
    fn up_pressed_within_zero_window_is_false() {
        let mut buf = InputBuffer::new();
        push_up(&mut buf, true);
        assert!(!buf.up_pressed_within(0), "zero window never fires");
    }

    #[test]
    fn up_pressed_within_requires_rising_edge_not_sustained_hold() {
        // A continuously-held up across the whole window is a single press at its
        // start: still detected (the start IS a rising edge), but a hold that
        // began *before* the window does not count as a fresh press inside it.
        let mut buf = InputBuffer::new();
        // Many neutral frames, then up held for a long time starting well before
        // the scan window.
        for _ in 0..10 {
            push_up(&mut buf, false);
        }
        push_up(&mut buf, true); // edge, far in the past
        for _ in 0..6 {
            push_up(&mut buf, true); // sustained hold up to and including now
        }
        // The edge is 6 frames ago; a 3-frame window sees only sustained hold
        // (no edge inside it) => false.
        assert!(
            !buf.up_pressed_within(3),
            "a hold that began before the window is not a fresh press"
        );
        // A window long enough to reach the edge does fire.
        assert!(buf.up_pressed_within(8));
    }

    #[test]
    fn up_pressed_within_no_up_is_false() {
        let mut buf = InputBuffer::new();
        for _ in 0..5 {
            push_up(&mut buf, false);
        }
        assert!(!buf.up_pressed_within(60));
    }

    #[test]
    fn up_pressed_within_clamps_to_stored_frames() {
        // Asking for a window larger than the buffer must not panic and must
        // still find an edge within the stored frames.
        let mut buf = InputBuffer::new();
        push_up(&mut buf, true);
        assert!(buf.up_pressed_within(usize::MAX));
    }
}
