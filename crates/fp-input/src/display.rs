//! On-screen input display (T064).
//!
//! Turns the rolling [`InputBuffer`] into a compact, human-readable strip of the
//! last ~16 frames of input — the "input history" overlay shown in Training mode
//! and most modern fighting games. Each *row* is a run of identical input frames
//! coalesced into a `(state, repeat)` pair, newest first, so a held direction or
//! a mashed button shows as one row with a repeat count rather than sixteen
//! near-duplicates.
//!
//! Directions are rendered as **numpad notation** (1–9, the fighting-game
//! standard: 1 = down-back, 2 = down, 3 = down-forward, 4 = back, 5 = neutral,
//! 6 = forward, 7 = up-back, 8 = up, 9 = up-forward) and are *facing-relative* —
//! "forward" is always toward the opponent, regardless of which side the player
//! stands on. Buttons are rendered as their MUGEN letters (`A`,`B`,`C`,`X`,`Y`,
//! `Z`). The glyphs are plain ASCII digits and letters so the shipped HUD font
//! (which covers `0-9 A-Z`) can draw them directly, no arrow sprites required.
//!
//! This module is pure (no rendering, no I/O); the app draws the rows it returns.

use crate::buffer::InputBuffer;
use crate::state::{logical_direction, Button, InputState, BUTTON_COUNT};

/// Default number of coalesced rows shown in the input strip (~16 frames of
/// history once runs are collapsed). The app caps the drawn strip to this.
pub const DEFAULT_DISPLAY_ROWS: usize = 16;

/// The six attack buttons, in display order, paired with their MUGEN letter.
///
/// `Start` is intentionally excluded: it is the pause/menu button, not a move
/// input, so it never appears in the move-input strip.
const DISPLAY_BUTTONS: [(Button, char); 6] = [
    (Button::A, 'A'),
    (Button::B, 'B'),
    (Button::C, 'C'),
    (Button::X, 'X'),
    (Button::Y, 'Y'),
    (Button::Z, 'Z'),
];

/// Maps a single [`InputState`] to its facing-relative **numpad direction digit**
/// (1–9), where forward is toward the opponent.
///
/// `facing_right` selects how absolute left/right fold into forward/back (it is
/// the same convention as [`logical_direction`]). A neutral stick is `5`. When
/// both ends of an axis are held (an impossible-on-a-stick but possible-on-a-
/// keyboard SOCD state) the axis is treated as neutral, so the digit is never
/// garbage.
#[must_use]
pub fn numpad_digit(state: &InputState, facing_right: bool) -> char {
    let l = logical_direction(&state.direction, facing_right);
    // Fold opposing presses to neutral on each axis (SOCD-safe).
    let up = l.up && !l.down;
    let down = l.down && !l.up;
    let fwd = l.forward && !l.back;
    let back = l.back && !l.forward;

    // Numpad layout (forward = "right" half of the pad):
    //   7 8 9      up-back   up    up-fwd
    //   4 5 6      back      neut  fwd
    //   1 2 3      dn-back   down  dn-fwd
    match (up, down, fwd, back) {
        (true, _, true, _) => '9',
        (true, _, _, true) => '7',
        (true, _, _, _) => '8',
        (_, true, true, _) => '3',
        (_, true, _, true) => '1',
        (_, true, _, _) => '2',
        (_, _, true, _) => '6',
        (_, _, _, true) => '4',
        _ => '5',
    }
}

/// Returns the lit attack-button letters for a state, in `A,B,C,X,Y,Z` order.
///
/// Excludes `Start`. The order is stable so the same chord always renders the
/// same way.
#[must_use]
pub fn button_glyphs(state: &InputState) -> Vec<char> {
    DISPLAY_BUTTONS
        .iter()
        .filter(|(b, _)| state.button(*b))
        .map(|(_, c)| *c)
        .collect()
}

/// A single coalesced row of the input-display strip: an input frame and how
/// many consecutive frames it was held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputDisplayRow {
    /// The input held for this run.
    pub state: InputState,
    /// Number of consecutive frames the [`state`](Self::state) was held
    /// (always `>= 1`).
    pub repeat: u8,
}

impl InputDisplayRow {
    /// Renders this row to a compact label: the numpad direction digit, then the
    /// lit button letters, then `*N` when the run lasted more than one frame.
    ///
    /// Examples (facing right): a held forward for 4 frames → `"6*4"`; a single
    /// down+A → `"2A"`; neutral for 9 frames → `"5*9"`.
    #[must_use]
    pub fn label(&self, facing_right: bool) -> String {
        let mut s = String::new();
        s.push(numpad_digit(&self.state, facing_right));
        for c in button_glyphs(&self.state) {
            s.push(c);
        }
        if self.repeat > 1 {
            s.push('*');
            s.push_str(&self.repeat.to_string());
        }
        s
    }
}

/// Whether two input frames are display-identical (same direction + same six
/// attack buttons). `Start` is ignored — it does not affect the move strip.
fn same_display(a: &InputState, b: &InputState) -> bool {
    a.direction == b.direction
        && DISPLAY_BUTTONS
            .iter()
            .all(|(btn, _)| a.button(*btn) == b.button(*btn))
}

/// Coalesces the buffer's recent frames into newest-anchored display rows.
///
/// Walks the ring **newest → oldest** (`get(0)` is the most recent frame),
/// collapsing each run of display-identical frames into one
/// [`InputDisplayRow`] carrying the run length. Stops once `max_rows` rows have
/// been produced (so an idle stick that has been neutral for a long time does
/// not crowd out older meaningful inputs) and saturates the per-run `repeat`
/// count at [`u8::MAX`] rather than overflowing.
///
/// The result is ordered newest-first: index `0` is the input being held right
/// now. An empty buffer yields an empty vec.
#[must_use]
pub fn input_display_rows(buf: &InputBuffer, max_rows: usize) -> Vec<InputDisplayRow> {
    let mut rows: Vec<InputDisplayRow> = Vec::new();
    if max_rows == 0 {
        return rows;
    }

    let mut ago = 0usize;
    while ago < buf.len() {
        let Some(state) = buf.get(ago) else { break };
        let state = *state;
        let mut repeat: u8 = 1;
        // Extend the run while the next-older frame is display-identical.
        while ago + 1 < buf.len() {
            match buf.get(ago + 1) {
                Some(next) if same_display(&state, next) => {
                    ago += 1;
                    repeat = repeat.saturating_add(1);
                }
                _ => break,
            }
        }
        rows.push(InputDisplayRow { state, repeat });
        if rows.len() >= max_rows {
            break;
        }
        ago += 1;
    }
    rows
}

// A compile-time check that the button-glyph table stays within the button set.
const _: () = assert!(DISPLAY_BUTTONS.len() < BUTTON_COUNT);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Button, Direction};

    fn dir(up: bool, down: bool, left: bool, right: bool) -> Direction {
        Direction {
            up,
            down,
            left,
            right,
        }
    }

    fn state(d: Direction, buttons: &[Button]) -> InputState {
        let mut s = InputState {
            direction: d,
            ..Default::default()
        };
        for &b in buttons {
            s.set_button(b, true);
        }
        s
    }

    #[test]
    fn numpad_neutral_is_five() {
        assert_eq!(numpad_digit(&InputState::default(), true), '5');
        assert_eq!(numpad_digit(&InputState::default(), false), '5');
    }

    #[test]
    fn numpad_cardinals_facing_right() {
        // Facing right: hardware right = forward (6), left = back (4).
        let right = state(dir(false, false, false, true), &[]);
        let left = state(dir(false, false, true, false), &[]);
        let up = state(dir(true, false, false, false), &[]);
        let down = state(dir(false, true, false, false), &[]);
        assert_eq!(numpad_digit(&right, true), '6');
        assert_eq!(numpad_digit(&left, true), '4');
        assert_eq!(numpad_digit(&up, true), '8');
        assert_eq!(numpad_digit(&down, true), '2');
    }

    #[test]
    fn numpad_forward_is_facing_relative() {
        // The SAME hardware "right" reads as forward (6) facing right but back
        // (4) facing left — forward always points at the opponent.
        let right = state(dir(false, false, false, true), &[]);
        assert_eq!(numpad_digit(&right, true), '6');
        assert_eq!(numpad_digit(&right, false), '4');
    }

    #[test]
    fn numpad_diagonals_facing_right() {
        let df = state(dir(false, true, false, true), &[]); // down + right
        let db = state(dir(false, true, true, false), &[]); // down + left
        let uf = state(dir(true, false, false, true), &[]); // up + right
        let ub = state(dir(true, false, true, false), &[]); // up + left
        assert_eq!(numpad_digit(&df, true), '3');
        assert_eq!(numpad_digit(&db, true), '1');
        assert_eq!(numpad_digit(&uf, true), '9');
        assert_eq!(numpad_digit(&ub, true), '7');
    }

    #[test]
    fn numpad_socd_folds_to_neutral_axis() {
        // Up+down and left+right held at once: each axis cancels, so a
        // simultaneous all-four press reads as neutral (5), never garbage.
        let all = state(dir(true, true, true, true), &[]);
        assert_eq!(numpad_digit(&all, true), '5');
        // Only the vertical axis cancels -> pure horizontal survives.
        let lr_with_fwd = state(dir(true, true, false, true), &[]);
        assert_eq!(numpad_digit(&lr_with_fwd, true), '6');
    }

    #[test]
    fn button_glyphs_in_order_excluding_start() {
        let s = state(
            Direction::default(),
            &[Button::C, Button::A, Button::Start, Button::Z],
        );
        // Order is the canonical A,B,C,X,Y,Z; Start is excluded.
        assert_eq!(button_glyphs(&s), vec!['A', 'C', 'Z']);
    }

    #[test]
    fn label_combines_direction_buttons_and_repeat() {
        let row = InputDisplayRow {
            state: state(dir(false, false, false, true), &[Button::A]),
            repeat: 4,
        };
        // Forward (facing right = 6) + A, held 4 frames.
        assert_eq!(row.label(true), "6A*4");
        // Single frame: no repeat suffix.
        let row1 = InputDisplayRow {
            state: state(dir(false, true, false, false), &[]),
            repeat: 1,
        };
        assert_eq!(row1.label(true), "2");
    }

    #[test]
    fn input_display_rows_empty_buffer() {
        let buf = InputBuffer::new();
        assert!(input_display_rows(&buf, DEFAULT_DISPLAY_ROWS).is_empty());
    }

    #[test]
    fn input_display_rows_max_rows_zero() {
        let mut buf = InputBuffer::new();
        buf.push(InputState::default());
        assert!(input_display_rows(&buf, 0).is_empty());
    }

    #[test]
    fn input_display_rows_coalesces_and_counts() {
        // Scripted fwd,fwd,down,down+X (oldest -> newest), facing right.
        let mut buf = InputBuffer::new();
        let fwd = state(dir(false, false, false, true), &[]);
        let down = state(dir(false, true, false, false), &[]);
        let down_x = state(dir(false, true, false, false), &[Button::X]);
        buf.push(fwd);
        buf.push(fwd);
        buf.push(down);
        buf.push(down);
        buf.push(down_x);

        let rows = input_display_rows(&buf, DEFAULT_DISPLAY_ROWS);
        // Newest-anchored: down+X (1), down (2), fwd (2).
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].repeat, 1);
        assert_eq!(rows[0].label(true), "2X");
        assert_eq!(rows[1].repeat, 2);
        assert_eq!(rows[1].label(true), "2*2");
        assert_eq!(rows[2].repeat, 2);
        assert_eq!(rows[2].label(true), "6*2");
    }

    #[test]
    fn input_display_rows_newest_first() {
        // The first row is always the input held *now* (offset 0).
        let mut buf = InputBuffer::new();
        buf.push(state(dir(false, false, false, true), &[])); // old: fwd
        buf.push(state(dir(false, true, false, false), &[Button::A])); // now: down+A
        let rows = input_display_rows(&buf, DEFAULT_DISPLAY_ROWS);
        assert_eq!(rows[0].label(true), "2A");
        assert_eq!(rows.last().unwrap().label(true), "6");
    }

    #[test]
    fn input_display_rows_respects_max_rows() {
        // Alternating distinct frames so no coalescing happens; cap at 3 rows.
        let mut buf = InputBuffer::new();
        for i in 0..10u8 {
            let d = if i % 2 == 0 {
                dir(false, false, false, true) // fwd
            } else {
                dir(false, false, true, false) // back
            };
            buf.push(state(d, &[]));
        }
        let rows = input_display_rows(&buf, 3);
        assert_eq!(
            rows.len(),
            3,
            "must stop at max_rows even with more history"
        );
    }

    #[test]
    fn input_display_rows_repeat_saturates() {
        // A run far longer than the ring's own capacity must saturate the u8
        // repeat count rather than overflow/panic.
        let mut buf = InputBuffer::new();
        let held = state(dir(false, true, false, false), &[]);
        for _ in 0..60 {
            buf.push(held);
        }
        let rows = input_display_rows(&buf, DEFAULT_DISPLAY_ROWS);
        assert_eq!(rows.len(), 1, "one coalesced run for the whole held buffer");
        // 60 identical frames -> repeat is the buffer length, well under u8::MAX.
        assert_eq!(rows[0].repeat, 60);
    }

    #[test]
    fn button_change_breaks_a_run() {
        // Same direction but a button toggles: that must split the run so the
        // press is visible, not swallowed into the held-direction count.
        let mut buf = InputBuffer::new();
        let down = state(dir(false, true, false, false), &[]);
        let down_a = state(dir(false, true, false, false), &[Button::A]);
        buf.push(down);
        buf.push(down);
        buf.push(down_a); // newest
        let rows = input_display_rows(&buf, DEFAULT_DISPLAY_ROWS);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label(true), "2A"); // the press, newest
        assert_eq!(rows[1].label(true), "2*2"); // prior hold
    }
}
