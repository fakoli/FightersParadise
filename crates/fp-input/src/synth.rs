//! Command / motion synthesizer — lowers a parsed MUGEN command motion into a
//! frame-by-frame sequence of absolute [`InputState`]s.
//!
//! This is the keystone of the GUI-free behavioral test harness: given the
//! symbolic motion of a command (e.g. `~D, DF, F, a`, the charge `~B, F, a`, or
//! a double-QCF super `~D, DF, F, D, DF, F, x`), [`synth_command`] produces a
//! `Vec<InputState>` of *absolute* directions + buttons that, when pushed into an
//! [`InputBuffer`](crate::buffer::InputBuffer) and fed to the engine's own
//! [`CommandMatcher`](crate::command::CommandMatcher), triggers that command.
//!
//! It is deliberately a **simple lowering**, not a reimplementation of input
//! semantics: it emits one dedicated frame per command element (with a neutral
//! separator frame between non-adjacent elements, and a held precursor frame for
//! release elements), and leans on the real matcher to certify correctness. Every
//! synthesis is meant to be *self-validated* by replaying it through the same
//! [`CommandMatcher`](crate::command::CommandMatcher) the engine uses — the
//! matcher is the oracle.

use crate::command::{CommandElement, InputModifier};
use crate::state::{Button, DirToken, Direction, InputState};

/// Lowers a parsed command motion into a frame-by-frame sequence of absolute
/// [`InputState`]s that the engine's
/// [`CommandMatcher`](crate::command::CommandMatcher) recognizes as that command.
///
/// The returned frames are ordered **oldest-first** (push them into an
/// [`InputBuffer`](crate::buffer::InputBuffer) in order); the last frame carries
/// the final element of the motion. `facing_right` selects the absolute
/// Left/Right that maps to Forward/Back, matching
/// [`logical_direction`](crate::state::logical_direction): when facing right,
/// Forward is hardware Right.
///
/// The lowering is intentionally minimal:
/// - each element gets one dedicated frame holding the absolute inputs that
///   satisfy it;
/// - a neutral separator frame is inserted *before* an element only when the
///   following element is **not** strict (`>`), so strict adjacency is preserved
///   while [`Press`](InputModifier::Press) elements still see a non-matching
///   prior frame;
/// - a [`Release`](InputModifier::Release) element is preceded by a frame that
///   *does* hold the input, so the release edge lands on the element's own frame.
///
/// Self-validate the result by replaying it through a
/// [`CommandMatcher`](crate::command::CommandMatcher) built from the same command
/// and asserting the command becomes active.
#[must_use]
pub fn synth_command(elements: &[CommandElement], facing_right: bool) -> Vec<InputState> {
    let mut frames: Vec<InputState> = Vec::new();
    // A leading neutral frame so the first element, if a press, sees a
    // non-matching prior frame.
    frames.push(InputState::default());

    for (i, element) in elements.iter().enumerate() {
        // Strict (`>`) on this element requires it to sit on the frame directly
        // after the previous element — so no separator before a strict element.
        // The first element never needs a separator (the leading neutral frame
        // already provides the non-matching precursor).
        let strict = element_is_strict(element);
        if i > 0 && !strict {
            frames.push(InputState::default());
        }
        emit_element(&mut frames, element, facing_right);
    }

    frames
}

/// Returns whether an element carries the MUGEN `>` strict-immediate flag.
///
/// Mirrors the matcher's own rule: a [`CommandElement::Simultaneous`] group is
/// governed by the flag of its first member.
fn element_is_strict(element: &CommandElement) -> bool {
    match element {
        CommandElement::Dir { strict, .. } => *strict,
        CommandElement::Button { strict, .. } => *strict,
        CommandElement::Simultaneous(parts) => parts.first().is_some_and(element_is_strict),
    }
}

/// Appends the frame(s) that satisfy a single element to `frames`.
///
/// Most elements emit one frame. A [`Release`](InputModifier::Release) element
/// first emits a *held* frame (so the release edge is real), then the releasing
/// frame.
fn emit_element(frames: &mut Vec<InputState>, element: &CommandElement, facing_right: bool) {
    match element {
        CommandElement::Dir {
            token,
            modifier,
            min_hold,
            ..
        } => {
            let dir = dir_token_to_direction(*token, facing_right);
            match modifier {
                InputModifier::Press | InputModifier::Hold => {
                    frames.push(state_with_direction(dir));
                }
                InputModifier::Release => {
                    // Hold the direction (long enough to satisfy any charge),
                    // then release it: the release edge lands on the (neutral)
                    // frame after the held run.
                    for _ in 0..charge_hold_frames(*min_hold) {
                        frames.push(state_with_direction(dir));
                    }
                    frames.push(InputState::default());
                }
            }
        }
        CommandElement::Button {
            button,
            modifier,
            min_hold,
            ..
        } => match modifier {
            InputModifier::Press | InputModifier::Hold => {
                frames.push(state_with_buttons(&[*button]));
            }
            InputModifier::Release => {
                for _ in 0..charge_hold_frames(*min_hold) {
                    frames.push(state_with_buttons(&[*button]));
                }
                frames.push(InputState::default());
            }
        },
        CommandElement::Simultaneous(parts) => {
            emit_simultaneous(frames, parts, facing_right);
        }
    }
}

/// Appends the frame that satisfies a simultaneous (`+`) group.
///
/// All press/hold members are merged onto a single frame (directions OR'd
/// together, buttons combined), since the matcher requires them on the same
/// frame. Release members within a group are uncommon; they merge into the same
/// merged frame followed by a release frame.
fn emit_simultaneous(frames: &mut Vec<InputState>, parts: &[CommandElement], facing_right: bool) {
    let mut merged = InputState::default();
    let mut any_release = false;
    for part in parts {
        match part {
            CommandElement::Dir {
                token, modifier, ..
            } => {
                let dir = dir_token_to_direction(*token, facing_right);
                or_direction(&mut merged.direction, dir);
                any_release |= matches!(modifier, InputModifier::Release);
            }
            CommandElement::Button {
                button, modifier, ..
            } => {
                merged.set_button(*button, true);
                any_release |= matches!(modifier, InputModifier::Release);
            }
            // Nested simultaneous groups are not produced by the parser, but
            // recurse defensively rather than dropping inputs.
            CommandElement::Simultaneous(inner) => {
                let mut tmp = Vec::new();
                emit_simultaneous(&mut tmp, inner, facing_right);
                if let Some(first) = tmp.first() {
                    or_direction(&mut merged.direction, first.direction);
                    for b in ALL_BUTTONS {
                        if first.button(b) {
                            merged.set_button(b, true);
                        }
                    }
                }
            }
        }
    }
    frames.push(merged);
    if any_release {
        frames.push(InputState::default());
    }
}

/// Number of held frames to emit before a release edge for a given charge
/// `min_hold`.
///
/// A non-charge release (`min_hold == 0`) emits a single held frame so the
/// release edge is real. A charge release emits `min_hold` held frames so the
/// matcher's consecutive-hold count is satisfied.
fn charge_hold_frames(min_hold: u32) -> u32 {
    min_hold.max(1)
}

/// All game buttons, used to merge simultaneous-group frames.
const ALL_BUTTONS: [Button; 7] = [
    Button::A,
    Button::B,
    Button::C,
    Button::X,
    Button::Y,
    Button::Z,
    Button::Start,
];

/// OR the set axes of `src` into `dst`.
fn or_direction(dst: &mut Direction, src: Direction) {
    dst.up |= src.up;
    dst.down |= src.down;
    dst.left |= src.left;
    dst.right |= src.right;
}

/// Builds an [`InputState`] holding the given absolute direction.
fn state_with_direction(direction: Direction) -> InputState {
    InputState {
        direction,
        ..Default::default()
    }
}

/// Builds an [`InputState`] holding the given buttons (no direction).
fn state_with_buttons(buttons: &[Button]) -> InputState {
    let mut s = InputState::default();
    for &b in buttons {
        s.set_button(b, true);
    }
    s
}

/// Converts a [`DirToken`] to the absolute hardware [`Direction`] that satisfies
/// it for the given facing.
///
/// This is the inverse of [`logical_direction`](crate::state::logical_direction):
/// Forward maps to hardware Right when `facing_right`, else hardware Left.
fn dir_token_to_direction(token: DirToken, facing_right: bool) -> Direction {
    // Forward / Back -> absolute Right / Left depending on facing.
    let (fwd_right, fwd_left) = if facing_right {
        (true, false)
    } else {
        (false, true)
    };
    let forward = Direction {
        right: fwd_right,
        left: fwd_left,
        ..Default::default()
    };
    let back = Direction {
        right: fwd_left,
        left: fwd_right,
        ..Default::default()
    };
    match token {
        DirToken::U => Direction {
            up: true,
            ..Default::default()
        },
        DirToken::D => Direction {
            down: true,
            ..Default::default()
        },
        DirToken::F => forward,
        DirToken::B => back,
        DirToken::UF => Direction {
            up: true,
            ..forward
        },
        DirToken::UB => Direction { up: true, ..back },
        DirToken::DF => Direction {
            down: true,
            ..forward
        },
        DirToken::DB => Direction { down: true, ..back },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::InputBuffer;
    use crate::command::{compile_command, CommandDef, CommandMatcher};

    /// Self-validation harness: synthesize `motion`, replay the frames through a
    /// fresh [`CommandMatcher`] built from the *same* command, and return whether
    /// the matcher recognizes it. The matcher is the oracle.
    fn synth_is_recognized(name: &str, motion: &str, facing_right: bool) -> bool {
        let elements = compile_command(motion).expect("motion compiles");
        let cmd = CommandDef {
            name: name.into(),
            elements: elements.clone(),
            time: 30,
            buffer_time: 3,
        };
        let frames = synth_command(&elements, facing_right);

        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        // Replay frame-by-frame, ticking the matcher each frame exactly as the
        // engine does, so timing/strictness is exercised realistically.
        for frame in &frames {
            buffer.push(*frame);
            matcher.check_commands(&buffer, facing_right);
            if matcher.command_active(name) {
                return true;
            }
        }
        matcher.command_active(name)
    }

    #[test]
    fn synth_single_button() {
        assert!(synth_is_recognized("punch", "x", true));
    }

    #[test]
    fn synth_qcf_button() {
        // Quarter-circle forward + a: the canonical fireball.
        assert!(synth_is_recognized("qcf_a", "D, DF, F, a", true));
    }

    #[test]
    fn synth_dp_dragon_punch() {
        // Dragon punch / shoryuken: F, D, DF + button.
        assert!(synth_is_recognized("dp_x", "F, D, DF, x", true));
    }

    #[test]
    fn synth_charge_back_forward() {
        // Charge motion: hold Back, then Forward + button (e.g. sonic boom).
        assert!(synth_is_recognized("charge_a", "~B, F, a", true));
    }

    #[test]
    fn synth_charge_with_min_hold() {
        // A charge with an explicit hold duration (`~30$B, F, a`) synthesizes
        // enough held frames to satisfy the charge and is recognized by the
        // same matcher — the synthesizer honors min_hold.
        assert!(synth_is_recognized("charge30_a", "~30$B, F, a", true));
        // The same motion synthesizes correctly facing left, too.
        assert!(synth_is_recognized("charge30_a_left", "~30$B, F, a", false));
    }

    #[test]
    fn synth_multi_qcf_super() {
        // Double quarter-circle forward super: D, DF, F, D, DF, F + button.
        assert!(synth_is_recognized(
            "super_x",
            "D, DF, F, D, DF, F, x",
            true
        ));
    }

    #[test]
    fn synth_works_facing_left() {
        // The same QCF must synthesize correctly when facing left (Forward is
        // hardware Left): proves the facing-relative lowering, not a hardcoded
        // right bias.
        assert!(synth_is_recognized("qcf_a_left", "D, DF, F, a", false));
    }

    #[test]
    fn synth_simultaneous_buttons() {
        // A two-button (a+b) press, the common "throw"/"dodge" input.
        assert!(synth_is_recognized("ab", "a+b", true));
    }

    #[test]
    fn synth_qcf_simultaneous_super() {
        // QCF ending in a simultaneous button group.
        assert!(synth_is_recognized("qcf_xy", "D, DF, F, x+y", true));
    }

    #[test]
    fn synth_strict_motion() {
        // A strict-immediate element (`>`) must still synthesize to recognized
        // frames: the lowering omits the separator before a strict element so the
        // adjacency holds.
        assert!(synth_is_recognized("strict_fa", "F, >a", true));
    }

    #[test]
    fn synth_frames_start_with_neutral() {
        // Structural guard: a leading neutral precursor frame is always present so
        // a press on the first element registers a real edge.
        let elements = compile_command("x").unwrap();
        let frames = synth_command(&elements, true);
        assert!(frames[0] == InputState::default());
        assert!(frames.len() >= 2);
    }

    #[test]
    fn synth_facing_left_uses_hardware_left_for_forward() {
        // Forward (`F`) facing left must map to hardware LEFT, the inverse of the
        // facing-right mapping.
        let dir = dir_token_to_direction(DirToken::F, false);
        assert!(
            dir.left && !dir.right,
            "Forward facing left is hardware left"
        );
        let dir_r = dir_token_to_direction(DirToken::F, true);
        assert!(
            dir_r.right && !dir_r.left,
            "Forward facing right is hardware right"
        );
    }
}
