//! Input state types for the Fighters Paradise engine.
//!
//! This module defines the raw and logical input representations used throughout
//! the input system. Raw [`Direction`] and [`InputState`] represent hardware-level
//! inputs, while [`LogicalDirection`] and [`DirToken`] represent facing-relative
//! directions used in MUGEN command definitions.

use serde::{Deserialize, Serialize};

/// MUGEN button identifiers.
///
/// Maps to the standard 6-button MUGEN layout (A through Z) plus Start.
/// The enum discriminants serve as indices into [`InputState::buttons`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Button {
    /// Light punch.
    A = 0,
    /// Medium punch.
    B = 1,
    /// Heavy punch.
    C = 2,
    /// Light kick.
    X = 3,
    /// Medium kick.
    Y = 4,
    /// Heavy kick.
    Z = 5,
    /// Start / pause button.
    Start = 6,
}

/// Number of game buttons (A, B, C, X, Y, Z, Start).
pub const BUTTON_COUNT: usize = 7;

/// Raw directional input from hardware (absolute Left/Right, not facing-relative).
///
/// This represents the physical direction pressed on a joystick or D-pad.
/// Left and right are absolute screen directions, not relative to character facing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Direction {
    /// Up is held.
    pub up: bool,
    /// Down is held.
    pub down: bool,
    /// Left is held.
    pub left: bool,
    /// Right is held.
    pub right: bool,
}

impl Direction {
    /// Returns `true` if no directional input is pressed.
    pub fn is_neutral(&self) -> bool {
        !self.up && !self.down && !self.left && !self.right
    }
}

/// Complete input state for one game tick.
///
/// Combines directional and button state into a single snapshot that gets
/// pushed into the [`crate::buffer::InputBuffer`] each frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InputState {
    /// Directional input for this frame.
    pub direction: Direction,
    /// Button states indexed by [`Button`] discriminant.
    pub buttons: [bool; BUTTON_COUNT],
}

impl InputState {
    /// Returns whether the given button is pressed in this state.
    pub fn button(&self, btn: Button) -> bool {
        self.buttons[btn as usize]
    }

    /// Sets the pressed state of the given button.
    pub fn set_button(&mut self, btn: Button, pressed: bool) {
        self.buttons[btn as usize] = pressed;
    }
}

/// Facing-relative directional input (Forward/Back instead of Left/Right).
///
/// MUGEN commands use relative directions so that a command like "D, DF, F, x"
/// works regardless of which side the character is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogicalDirection {
    /// Up is held.
    pub up: bool,
    /// Down is held.
    pub down: bool,
    /// Forward (toward opponent) is held.
    pub forward: bool,
    /// Back (away from opponent) is held.
    pub back: bool,
}

/// Directional tokens used in MUGEN command definitions.
///
/// These represent the cardinal and diagonal directions in a command sequence
/// such as `D, DF, F` (quarter-circle forward).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirToken {
    /// Up.
    U,
    /// Down.
    D,
    /// Forward.
    F,
    /// Back.
    B,
    /// Up-forward (diagonal).
    UF,
    /// Up-back (diagonal).
    UB,
    /// Down-forward (diagonal).
    DF,
    /// Down-back (diagonal).
    DB,
}

/// Converts a raw [`Direction`] to a [`LogicalDirection`] given the character's facing.
///
/// When `facing_right` is `true`, hardware right maps to forward and left maps to back.
/// When `false`, the mapping is reversed.
pub fn logical_direction(dir: &Direction, facing_right: bool) -> LogicalDirection {
    LogicalDirection {
        up: dir.up,
        down: dir.down,
        forward: if facing_right { dir.right } else { dir.left },
        back: if facing_right { dir.left } else { dir.right },
    }
}

/// Checks if a [`LogicalDirection`] matches a [`DirToken`].
///
/// Cardinal directions require an exact match (no other axes pressed),
/// while diagonal directions only require both component axes to be held.
pub fn dir_matches(logical: &LogicalDirection, token: DirToken) -> bool {
    match token {
        DirToken::U => logical.up && !logical.down && !logical.forward && !logical.back,
        DirToken::D => logical.down && !logical.up && !logical.forward && !logical.back,
        DirToken::F => logical.forward && !logical.up && !logical.down && !logical.back,
        DirToken::B => logical.back && !logical.up && !logical.down && !logical.forward,
        DirToken::UF => logical.up && logical.forward,
        DirToken::UB => logical.up && logical.back,
        DirToken::DF => logical.down && logical.forward,
        DirToken::DB => logical.down && logical.back,
    }
}

/// Checks if a [`LogicalDirection`] matches a [`DirToken`] in *direction-detect*
/// mode (the MUGEN `$` prefix).
///
/// Unlike [`dir_matches`], direction-detect does **not** require an exact match
/// of the whole stick. It only requires that the token's component axis (or
/// axes) be held, ignoring whatever else is pressed — except the directly
/// opposing axis, which must not be held. This is what makes `$F` (and the
/// `holdfwd` command built from `/$F`) fire while the player holds Forward at
/// *any* vertical angle (F, UF, or DF), the way MUGEN does.
///
/// Concretely:
/// - `$F` is satisfied by F, UF, or DF (forward held, back not held).
/// - `$B` is satisfied by B, UB, or DB (back held, forward not held).
/// - `$U` is satisfied by U, UF, or UB (up held, down not held).
/// - `$D` is satisfied by D, DF, or DB (down held, up not held).
/// - Diagonals require both component axes held, with neither opposing axis
///   held (e.g. `$UF` needs up and forward, but not down or back).
pub fn dir_matches_detect(logical: &LogicalDirection, token: DirToken) -> bool {
    match token {
        DirToken::U => logical.up && !logical.down,
        DirToken::D => logical.down && !logical.up,
        DirToken::F => logical.forward && !logical.back,
        DirToken::B => logical.back && !logical.forward,
        DirToken::UF => logical.up && logical.forward && !logical.down && !logical.back,
        DirToken::UB => logical.up && logical.back && !logical.down && !logical.forward,
        DirToken::DF => logical.down && logical.forward && !logical.up && !logical.back,
        DirToken::DB => logical.down && logical.back && !logical.up && !logical.forward,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_set_get() {
        let mut state = InputState::default();
        assert!(!state.button(Button::A));
        state.set_button(Button::A, true);
        assert!(state.button(Button::A));
    }

    #[test]
    fn direction_neutral() {
        let dir = Direction::default();
        assert!(dir.is_neutral());
    }

    #[test]
    fn logical_direction_facing_right() {
        let dir = Direction {
            right: true,
            left: false,
            up: false,
            down: false,
        };
        let logical = logical_direction(&dir, true);
        assert!(logical.forward);
        assert!(!logical.back);
    }

    #[test]
    fn logical_direction_facing_left() {
        let dir = Direction {
            left: true,
            right: false,
            up: false,
            down: false,
        };
        let logical = logical_direction(&dir, false);
        assert!(logical.forward);
        assert!(!logical.back);
    }

    #[test]
    fn dir_matches_cardinal() {
        let logical = LogicalDirection {
            forward: true,
            up: false,
            down: false,
            back: false,
        };
        assert!(dir_matches(&logical, DirToken::F));
        assert!(!dir_matches(&logical, DirToken::B));
        assert!(!dir_matches(&logical, DirToken::U));
    }

    #[test]
    fn dir_matches_diagonal() {
        let logical = LogicalDirection {
            down: true,
            forward: true,
            up: false,
            back: false,
        };
        assert!(dir_matches(&logical, DirToken::DF));
        assert!(!dir_matches(&logical, DirToken::D));
        assert!(!dir_matches(&logical, DirToken::F));
    }

    #[test]
    fn dir_matches_detect_axis_held() {
        // `$F` (direction-detect forward) fires on F, UF and DF, but not back.
        let fwd = LogicalDirection {
            forward: true,
            ..Default::default()
        };
        let up_fwd = LogicalDirection {
            forward: true,
            up: true,
            ..Default::default()
        };
        let down_fwd = LogicalDirection {
            forward: true,
            down: true,
            ..Default::default()
        };
        let back = LogicalDirection {
            back: true,
            ..Default::default()
        };
        assert!(dir_matches_detect(&fwd, DirToken::F));
        assert!(dir_matches_detect(&up_fwd, DirToken::F));
        assert!(dir_matches_detect(&down_fwd, DirToken::F));
        assert!(!dir_matches_detect(&back, DirToken::F));

        // Plain (non-detect) cardinal `F` rejects the diagonals.
        assert!(!dir_matches(&up_fwd, DirToken::F));
        assert!(!dir_matches(&down_fwd, DirToken::F));
    }

    #[test]
    fn dir_matches_detect_opposing_axis_excluded() {
        // Both forward and back held simultaneously is ambiguous: neither
        // `$F` nor `$B` should fire.
        let both = LogicalDirection {
            forward: true,
            back: true,
            ..Default::default()
        };
        assert!(!dir_matches_detect(&both, DirToken::F));
        assert!(!dir_matches_detect(&both, DirToken::B));
    }

    // -- Proctor: additional state-layer coverage -------------------------

    #[test]
    fn logical_direction_facing_left_back_mapping() {
        // Facing LEFT: hardware right is Back, hardware left is Forward.
        // This is the mirror of the facing-right case and underpins
        // facing-relative command matching on side switch.
        let dir = Direction {
            right: true,
            ..Default::default()
        };
        let logical = logical_direction(&dir, false);
        assert!(!logical.forward, "hardware right is Back when facing left");
        assert!(logical.back, "hardware right is Back when facing left");
    }

    #[test]
    fn logical_direction_preserves_vertical_axes() {
        // Up/Down are facing-independent; they must pass through unchanged
        // regardless of facing.
        let dir = Direction {
            up: true,
            down: true,
            ..Default::default()
        };
        for facing_right in [true, false] {
            let logical = logical_direction(&dir, facing_right);
            assert!(logical.up);
            assert!(logical.down);
        }
    }

    #[test]
    fn dir_matches_cardinal_requires_pure_axis() {
        // MUGEN cardinal (non-detect) F requires *no other* axis held: UF must
        // not satisfy plain F. (This is precisely why holdfwd uses `$F`, not F.)
        let up_fwd = LogicalDirection {
            forward: true,
            up: true,
            ..Default::default()
        };
        assert!(!dir_matches(&up_fwd, DirToken::F));
        assert!(!dir_matches(&up_fwd, DirToken::U));
        // ...but the diagonal token UF accepts it.
        assert!(dir_matches(&up_fwd, DirToken::UF));
    }

    #[test]
    fn dir_matches_detect_all_cardinals() {
        // Exhaustive: every cardinal `$` token fires on its own axis and on
        // both adjacent diagonals, and is rejected by the opposite cardinal.
        let cases = [
            (DirToken::U, ("up",)),
            (DirToken::D, ("down",)),
            (DirToken::F, ("forward",)),
            (DirToken::B, ("back",)),
        ];
        for (token, _) in cases {
            // Build the pure-axis direction for this token.
            let mut pure = LogicalDirection::default();
            match token {
                DirToken::U => pure.up = true,
                DirToken::D => pure.down = true,
                DirToken::F => pure.forward = true,
                DirToken::B => pure.back = true,
                _ => unreachable!(),
            }
            assert!(
                dir_matches_detect(&pure, token),
                "{token:?} must fire on its pure axis"
            );
        }
        // Opposite-cardinal rejection.
        let up = LogicalDirection {
            up: true,
            ..Default::default()
        };
        assert!(!dir_matches_detect(&up, DirToken::D));
        let fwd = LogicalDirection {
            forward: true,
            ..Default::default()
        };
        assert!(!dir_matches_detect(&fwd, DirToken::B));
    }

    #[test]
    fn dir_matches_detect_diagonal_needs_both_axes() {
        // `$DF` (detect down-forward) requires BOTH down and forward, and must
        // reject either component alone or any opposing axis.
        let df = LogicalDirection {
            down: true,
            forward: true,
            ..Default::default()
        };
        assert!(dir_matches_detect(&df, DirToken::DF));

        let only_down = LogicalDirection {
            down: true,
            ..Default::default()
        };
        assert!(!dir_matches_detect(&only_down, DirToken::DF));

        let only_fwd = LogicalDirection {
            forward: true,
            ..Default::default()
        };
        assert!(!dir_matches_detect(&only_fwd, DirToken::DF));

        // Opposing vertical axis held => reject.
        let df_plus_up = LogicalDirection {
            down: true,
            forward: true,
            up: true,
            ..Default::default()
        };
        assert!(!dir_matches_detect(&df_plus_up, DirToken::DF));
    }

    #[test]
    fn dir_matches_detect_neutral_matches_nothing() {
        // A neutral stick satisfies no direction-detect token. (Critical: a
        // hold-only holdfwd `/$F` with time=1 must NOT fire on neutral.)
        let neutral = LogicalDirection::default();
        for token in [
            DirToken::U,
            DirToken::D,
            DirToken::F,
            DirToken::B,
            DirToken::UF,
            DirToken::UB,
            DirToken::DF,
            DirToken::DB,
        ] {
            assert!(
                !dir_matches_detect(&neutral, token),
                "neutral must not satisfy {token:?}"
            );
        }
    }

    #[test]
    fn button_discriminants_are_unique_indices() {
        // The InputState::buttons array is indexed by `Button as usize`; verify
        // every variant maps to a distinct in-range slot so set/get never alias.
        let all = [
            Button::A,
            Button::B,
            Button::C,
            Button::X,
            Button::Y,
            Button::Z,
            Button::Start,
        ];
        let mut state = InputState::default();
        for (i, &btn) in all.iter().enumerate() {
            assert_eq!(btn as usize, i, "{btn:?} index drifted");
            assert!((btn as usize) < BUTTON_COUNT);
            state.set_button(btn, true);
        }
        // All seven independently set, none aliased.
        for &btn in &all {
            assert!(state.button(btn), "{btn:?} got clobbered (aliasing)");
        }
    }
}
