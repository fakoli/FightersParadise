//! Pure gamepad / game-controller → game-input mapping.
//!
//! This module is deliberately **free of any SDL (or other backend) types**: it
//! takes a backend-agnostic snapshot of a controller's physical state
//! ([`RawController`]) and maps it to the engine's directional + button model
//! ([`ControllerInput`], built from the existing [`Direction`] and [`Button`]).
//!
//! Keeping the mapping pure makes it unit-testable without a live device or an
//! SDL context (the application crate owns the thin SDL adapter that fills in a
//! [`RawController`] each frame). The mapping never allocates, never panics, and
//! treats a fully-neutral / all-false [`RawController`] as "nothing pressed".
//!
//! # Direction mapping
//!
//! The four MUGEN directions come from **either** the D-pad **or** the left
//! analog stick — whichever is active wins, OR'd together, so a player can use
//! either at any moment:
//!
//! - D-pad: the four boolean buttons map straight to up/down/left/right.
//! - Left stick: each axis past the [deadzone](RawController::stick_x) in the
//!   negative direction is left/up and in the positive direction is right/down
//!   (SDL axis convention: `-32768` = left/up, `+32767` = right/down).
//!
//! # Button mapping (Street-Fighter-style 6-button layout)
//!
//! MUGEN's six attack buttons are `a b c` (the punch row) and `x y z` (the kick
//! row). They map to a standard fight-pad face + shoulder layout:
//!
//! | MUGEN | Meaning      | Controller source        |
//! |-------|--------------|--------------------------|
//! | `a`   | Light punch  | West face button (X/□)    |
//! | `b`   | Medium punch | North face button (Y/△)   |
//! | `c`   | Heavy punch  | Right shoulder (RB/R1)    |
//! | `x`   | Light kick   | South face button (A/✕)   |
//! | `y`   | Medium kick  | East face button (B/○)    |
//! | `z`   | Heavy kick   | Left shoulder (LB/L1)     |
//! | start | Pause/start  | Start button              |
//!
//! This is the classic arcade-stick / Xbox fight-pad arrangement: the four face
//! buttons give the two light + two medium attacks (punches on top, kicks on the
//! diagonal) and the shoulders give the two heavies, so all six attacks are
//! reachable without a claw grip. The naming above uses Xbox letters; on a
//! PlayStation pad the SDL game-controller layer keeps the same *positions*
//! (West/North/South/East), so `□ △ ✕ ○` land on the same MUGEN buttons.

use crate::state::{Button, Direction, BUTTON_COUNT};

/// Default left-stick deadzone, as a magnitude on SDL's `i16` axis range
/// (`-32768..=32767`).
///
/// Roughly 26% of full deflection — large enough to swallow resting-stick jitter
/// on worn pads, small enough that a deliberate push still registers. The
/// application can pass a different value to [`map_controller`] if desired.
pub const DEADZONE_DEFAULT: i16 = 8000;

/// A backend-agnostic snapshot of one controller's physical state for a frame.
///
/// The application's SDL adapter fills this in from a live `GameController`; the
/// pure [`map_controller`] turns it into a [`ControllerInput`]. Every field
/// defaults to neutral / not-pressed, so [`RawController::default`] is a clean
/// "nothing held" snapshot (used when no device is connected).
///
/// Axis values follow the SDL game-controller convention: `0` is centered,
/// `-32768` is full left / full up, `+32767` is full right / full down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawController {
    /// Left-stick horizontal axis (`-` = left, `+` = right).
    pub stick_x: i16,
    /// Left-stick vertical axis (`-` = up, `+` = down).
    pub stick_y: i16,
    /// D-pad up is held.
    pub dpad_up: bool,
    /// D-pad down is held.
    pub dpad_down: bool,
    /// D-pad left is held.
    pub dpad_left: bool,
    /// D-pad right is held.
    pub dpad_right: bool,
    /// South face button (Xbox `A`, PlayStation `✕`) — maps to MUGEN `x`.
    pub face_south: bool,
    /// East face button (Xbox `B`, PlayStation `○`) — maps to MUGEN `y`.
    pub face_east: bool,
    /// West face button (Xbox `X`, PlayStation `□`) — maps to MUGEN `a`.
    pub face_west: bool,
    /// North face button (Xbox `Y`, PlayStation `△`) — maps to MUGEN `b`.
    pub face_north: bool,
    /// Left shoulder (`LB` / `L1`) — maps to MUGEN `z`.
    pub shoulder_left: bool,
    /// Right shoulder (`RB` / `R1`) — maps to MUGEN `c`.
    pub shoulder_right: bool,
    /// Start button — maps to MUGEN `start`.
    pub start: bool,
}

/// The mapped game input for one controller, in the engine's own model.
///
/// Combines a resolved absolute [`Direction`] with the seven-entry button array
/// (`a b c x y z start`, indexed by [`Button`]). This is the pure output of
/// [`map_controller`]; the application converts it to whatever its match layer
/// consumes (e.g. merging it with the keyboard for player 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ControllerInput {
    /// Resolved absolute direction (D-pad OR'd with the deadzoned left stick).
    pub direction: Direction,
    /// Button states indexed by the [`Button`] discriminant (`a`..=`Start`).
    pub buttons: [bool; BUTTON_COUNT],
}

impl ControllerInput {
    /// Returns whether the given button is pressed in this mapped input.
    #[must_use]
    pub fn button(&self, btn: Button) -> bool {
        self.buttons[btn as usize]
    }
}

/// Maps a raw controller snapshot to a [`ControllerInput`], applying `deadzone`
/// to the left analog stick.
///
/// Pure and total: any `RawController` (including the all-zero
/// [`RawController::default`]) yields a valid result, and it never panics. The
/// direction is the D-pad OR the deadzoned left stick (either source can assert a
/// direction); the buttons follow the Street-Fighter-style layout documented at
/// the [module level](crate::controller).
///
/// `deadzone` is compared by magnitude, so its sign does not matter; a value of
/// `0` makes the stick maximally sensitive (any non-zero axis registers) and a
/// value `>= i16::MAX` effectively disables the stick (D-pad only). The common
/// case is [`DEADZONE_DEFAULT`].
#[must_use]
pub fn map_controller(raw: &RawController, deadzone: i16) -> ControllerInput {
    let dz = deadzone.unsigned_abs() as i32;
    let x = i32::from(raw.stick_x);
    let y = i32::from(raw.stick_y);

    // Stick contribution past the deadzone (SDL convention: -X/-Y = left/up).
    let stick_left = x < -dz;
    let stick_right = x > dz;
    let stick_up = y < -dz;
    let stick_down = y > dz;

    let direction = Direction {
        up: raw.dpad_up || stick_up,
        down: raw.dpad_down || stick_down,
        left: raw.dpad_left || stick_left,
        right: raw.dpad_right || stick_right,
    };

    let mut buttons = [false; BUTTON_COUNT];
    // Punch row (a/b/c): West, North, Right-shoulder.
    buttons[Button::A as usize] = raw.face_west;
    buttons[Button::B as usize] = raw.face_north;
    buttons[Button::C as usize] = raw.shoulder_right;
    // Kick row (x/y/z): South, East, Left-shoulder.
    buttons[Button::X as usize] = raw.face_south;
    buttons[Button::Y as usize] = raw.face_east;
    buttons[Button::Z as usize] = raw.shoulder_left;
    // Start / pause.
    buttons[Button::Start as usize] = raw.start;

    ControllerInput { direction, buttons }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_snapshot_maps_to_nothing() {
        let out = map_controller(&RawController::default(), DEADZONE_DEFAULT);
        assert!(out.direction.is_neutral());
        assert_eq!(out.buttons, [false; BUTTON_COUNT]);
    }

    #[test]
    fn dpad_drives_each_direction() {
        let r = RawController {
            dpad_up: true,
            dpad_right: true,
            ..RawController::default()
        };
        let out = map_controller(&r, DEADZONE_DEFAULT);
        assert!(out.direction.up);
        assert!(out.direction.right);
        assert!(!out.direction.down);
        assert!(!out.direction.left);
    }

    #[test]
    fn stick_inside_deadzone_is_neutral() {
        // Just under the deadzone on both axes -> no direction.
        let r = RawController {
            stick_x: DEADZONE_DEFAULT - 1,
            stick_y: -(DEADZONE_DEFAULT - 1),
            ..RawController::default()
        };
        let out = map_controller(&r, DEADZONE_DEFAULT);
        assert!(
            out.direction.is_neutral(),
            "axis within deadzone must not register: {:?}",
            out.direction
        );
    }

    #[test]
    fn stick_at_exact_deadzone_is_neutral() {
        // Strictly-greater-than comparison: exactly the deadzone does not fire.
        let r = RawController {
            stick_x: DEADZONE_DEFAULT,
            ..RawController::default()
        };
        let out = map_controller(&r, DEADZONE_DEFAULT);
        assert!(!out.direction.right);
        assert!(!out.direction.left);
    }

    #[test]
    fn stick_past_deadzone_registers_correct_direction() {
        // Negative X = left, positive Y = down (SDL convention).
        let r = RawController {
            stick_x: -20000,
            stick_y: 20000,
            ..RawController::default()
        };
        let out = map_controller(&r, DEADZONE_DEFAULT);
        assert!(out.direction.left);
        assert!(out.direction.down);
        assert!(!out.direction.right);
        assert!(!out.direction.up);
    }

    #[test]
    fn stick_full_deflection_up_right() {
        let r = RawController {
            stick_x: i16::MAX,
            stick_y: i16::MIN,
            ..RawController::default()
        };
        let out = map_controller(&r, DEADZONE_DEFAULT);
        assert!(out.direction.right);
        assert!(out.direction.up);
    }

    #[test]
    fn dpad_and_stick_are_ored() {
        // D-pad left + stick right: both assert (OR), so left and right both set.
        let r = RawController {
            dpad_left: true,
            stick_x: 20000,
            ..RawController::default()
        };
        let out = map_controller(&r, DEADZONE_DEFAULT);
        assert!(out.direction.left, "d-pad contribution");
        assert!(out.direction.right, "stick contribution");
    }

    #[test]
    fn zero_deadzone_makes_small_input_register() {
        let r = RawController {
            stick_x: 1,
            ..RawController::default()
        };
        let out = map_controller(&r, 0);
        assert!(out.direction.right);
    }

    #[test]
    fn huge_deadzone_disables_stick_keeps_dpad() {
        // A deadzone at the top of the i16 range swallows any stick value whose
        // magnitude is `<= dz` (which is every value except i16::MIN). Use a
        // near-full deflection so the stick is suppressed; the d-pad survives.
        let r = RawController {
            stick_x: i16::MAX,
            stick_y: -i16::MAX,
            dpad_down: true,
            ..RawController::default()
        };
        let out = map_controller(&r, i16::MAX);
        assert!(!out.direction.right);
        assert!(!out.direction.up);
        assert!(out.direction.down);
    }

    #[test]
    fn negative_deadzone_is_treated_by_magnitude() {
        // A negative deadzone must behave identically to its absolute value.
        let r = RawController {
            stick_x: 9000,
            ..RawController::default()
        };
        let pos = map_controller(&r, DEADZONE_DEFAULT);
        let neg = map_controller(&r, -DEADZONE_DEFAULT);
        assert_eq!(pos, neg);
    }

    #[test]
    fn each_face_and_shoulder_button_maps_to_the_documented_mugen_button() {
        // a = West, b = North, c = RightShoulder.
        assert!(map_controller(
            &RawController {
                face_west: true,
                ..RawController::default()
            },
            DEADZONE_DEFAULT
        )
        .button(Button::A));
        assert!(map_controller(
            &RawController {
                face_north: true,
                ..RawController::default()
            },
            DEADZONE_DEFAULT
        )
        .button(Button::B));
        assert!(map_controller(
            &RawController {
                shoulder_right: true,
                ..RawController::default()
            },
            DEADZONE_DEFAULT
        )
        .button(Button::C));
        // x = South, y = East, z = LeftShoulder.
        assert!(map_controller(
            &RawController {
                face_south: true,
                ..RawController::default()
            },
            DEADZONE_DEFAULT
        )
        .button(Button::X));
        assert!(map_controller(
            &RawController {
                face_east: true,
                ..RawController::default()
            },
            DEADZONE_DEFAULT
        )
        .button(Button::Y));
        assert!(map_controller(
            &RawController {
                shoulder_left: true,
                ..RawController::default()
            },
            DEADZONE_DEFAULT
        )
        .button(Button::Z));
        // start.
        assert!(map_controller(
            &RawController {
                start: true,
                ..RawController::default()
            },
            DEADZONE_DEFAULT
        )
        .button(Button::Start));
    }

    #[test]
    fn one_button_does_not_set_others() {
        let out = map_controller(
            &RawController {
                face_west: true,
                ..RawController::default()
            },
            DEADZONE_DEFAULT,
        );
        assert!(out.button(Button::A));
        for b in [
            Button::B,
            Button::C,
            Button::X,
            Button::Y,
            Button::Z,
            Button::Start,
        ] {
            assert!(!out.button(b), "{b:?} should be unpressed");
        }
    }
}
