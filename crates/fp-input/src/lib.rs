//! # fp-input
//!
//! Input handling for the Fighters Paradise engine. Manages keyboard/gamepad
//! polling, input buffering (60-frame ring buffer), and MUGEN command sequence
//! matching for special move detection.

#![warn(missing_docs)]

pub mod ai;
pub mod buffer;
pub mod command;
pub mod controller;
pub mod display;
pub mod state;
pub mod synth;

pub use ai::{AiDifficulty, AiObservation, AiTuning, BehaviorMode, CpuAi};
pub use buffer::{InputBuffer, InputBufferSnapshot};
pub use command::{
    compile_command, CommandDef, CommandElement, CommandMatcher, CommandMatcherSnapshot,
    InputModifier, LeniencyConfig,
};
pub use controller::{map_controller, ControllerInput, RawController, DEADZONE_DEFAULT};
pub use display::{
    button_glyphs, input_display_rows, numpad_digit, InputDisplayRow, DEFAULT_DISPLAY_ROWS,
};
pub use state::{
    dir_matches, dir_matches_detect, logical_direction, Button, DirToken, Direction, InputState,
    LogicalDirection, BUTTON_COUNT,
};
pub use synth::synth_command;

#[cfg(test)]
mod playability_tests {
    //! Crate-level integration coverage for the keyboard-driven input path
    //! (T024): the documented movement directions and each attack button,
    //! expressed as the raw absolute [`InputState`]s the app's keyboard sampler
    //! produces, must reach the engine as the expected commands.
    //!
    //! The app (`fp-app`) maps physical scancodes to an absolute
    //! left/right/up/down + a/b/c/x/y/z snapshot; the engine pushes that into an
    //! [`InputBuffer`] and runs a [`CommandMatcher`]. These tests stand in for
    //! that path with synthetic [`InputState`]s (no SDL needed), asserting that
    //! the four directions activate the standard movement commands
    //! (`holdfwd`/`holdback`/jump-up/crouch-down) and that each of the six attack
    //! buttons activates its own single-button command.

    use crate::buffer::InputBuffer;
    use crate::command::{compile_command, CommandDef, CommandMatcher};
    use crate::state::{Button, Direction, InputState};

    /// A one-frame hold command (e.g. `/$F`, `/$B`, `/$U`, `/$D`).
    fn hold_cmd(name: &str, seq: &str) -> CommandDef {
        CommandDef {
            name: name.into(),
            elements: compile_command(seq).expect("seq compiles"),
            time: 1,
            buffer_time: 1,
        }
    }

    /// A single-button press command (e.g. `a`).
    fn button_cmd(name: &str, seq: &str) -> CommandDef {
        CommandDef {
            name: name.into(),
            elements: compile_command(seq).expect("seq compiles"),
            time: 1,
            buffer_time: 1,
        }
    }

    /// Builds an absolute-direction [`InputState`] the way the app's keyboard
    /// sampler does (raw left/right/up/down + button presses).
    fn frame(dir: Direction, buttons: &[Button]) -> InputState {
        let mut s = InputState {
            direction: dir,
            ..Default::default()
        };
        for &b in buttons {
            s.set_button(b, true);
        }
        s
    }

    #[test]
    fn right_and_left_drive_forward_and_back_commands() {
        // Facing right: hardware Right is Forward (walk forward), hardware Left
        // is Back (walk back / guard). These are the engine's walk commands.
        let mut matcher = CommandMatcher::new(vec![
            hold_cmd("holdfwd", "/$F"),
            hold_cmd("holdback", "/$B"),
        ]);

        let mut buf = InputBuffer::new();
        buf.push(frame(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buf, true);
        assert!(matcher.command_active("holdfwd"), "Right -> walk forward");
        assert!(!matcher.command_active("holdback"));

        let mut buf = InputBuffer::new();
        buf.push(frame(
            Direction {
                left: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buf, true);
        assert!(matcher.command_active("holdback"), "Left -> walk back");
        assert!(!matcher.command_active("holdfwd"));
    }

    #[test]
    fn up_and_down_drive_jump_and_crouch_commands() {
        // Up = jump, Down = crouch — facing-independent vertical axes.
        let mut matcher =
            CommandMatcher::new(vec![hold_cmd("holdup", "/$U"), hold_cmd("holddown", "/$D")]);

        let mut buf = InputBuffer::new();
        buf.push(frame(
            Direction {
                up: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buf, true);
        assert!(matcher.command_active("holdup"), "Up -> jump");
        assert!(!matcher.command_active("holddown"));

        let mut buf = InputBuffer::new();
        buf.push(frame(
            Direction {
                down: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buf, true);
        assert!(matcher.command_active("holddown"), "Down -> crouch");
        assert!(!matcher.command_active("holdup"));
    }

    #[test]
    fn each_attack_button_activates_its_command() {
        // The six MUGEN attack buttons each fire their own single-button command,
        // which is how an attack key triggers a move.
        let cases = [
            ("cmd_a", "a", Button::A),
            ("cmd_b", "b", Button::B),
            ("cmd_c", "c", Button::C),
            ("cmd_x", "x", Button::X),
            ("cmd_y", "y", Button::Y),
            ("cmd_z", "z", Button::Z),
        ];
        let defs: Vec<CommandDef> = cases
            .iter()
            .map(|(name, seq, _)| button_cmd(name, seq))
            .collect();

        for (name, _, button) in cases {
            let mut matcher = CommandMatcher::new(defs.clone());
            let mut buf = InputBuffer::new();
            // A neutral frame, then the button press (Press = newly pressed).
            buf.push(InputState::default());
            buf.push(frame(Direction::default(), &[button]));
            matcher.check_commands(&buf, true);
            assert!(
                matcher.command_active(name),
                "pressing {button:?} should activate {name}"
            );
            // No other attack command should fire from a single button.
            for (other, _, _) in cases {
                if other != name {
                    assert!(
                        !matcher.command_active(other),
                        "{button:?} must not also activate {other}"
                    );
                }
            }
        }
    }

    #[test]
    fn walk_forward_while_attacking_fires_both() {
        // Holding Right + pressing the `a` button (advance + light punch) must
        // surface BOTH the walk-forward hold and the attack command — the common
        // "attack while moving" case the keyboard must support.
        let mut matcher =
            CommandMatcher::new(vec![hold_cmd("holdfwd", "/$F"), button_cmd("cmd_a", "a")]);
        let mut buf = InputBuffer::new();
        buf.push(InputState::default());
        buf.push(frame(
            Direction {
                right: true,
                ..Default::default()
            },
            &[Button::A],
        ));
        matcher.check_commands(&buf, true);
        assert!(matcher.command_active("holdfwd"), "still walking forward");
        assert!(matcher.command_active("cmd_a"), "attack also fires");
    }
}
