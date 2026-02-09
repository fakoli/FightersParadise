//! Input state types for the Fighters Paradise engine.
//!
//! This module defines the raw and logical input representations used throughout
//! the input system. Raw [`Direction`] and [`InputState`] represent hardware-level
//! inputs, while [`LogicalDirection`] and [`DirToken`] represent facing-relative
//! directions used in MUGEN command definitions.

/// MUGEN button identifiers.
///
/// Maps to the standard 6-button MUGEN layout (A through Z) plus Start.
/// The enum discriminants serve as indices into [`InputState::buttons`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
}
