//! MUGEN command sequence parsing and matching.
//!
//! This module handles the recognition of special move input sequences. Command
//! definitions (from `.cmd` files) are compiled into [`CommandDef`] structures,
//! and the [`CommandMatcher`] checks them against the [`crate::buffer::InputBuffer`]
//! each tick to detect when the player has executed a command.

use crate::buffer::InputBuffer;
use crate::state::*;
use fp_core::{FpError, FpResult};

/// Modifier applied to a command element.
///
/// Determines whether the input must be freshly pressed, released, or held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputModifier {
    /// The input must be newly pressed (was not pressed last frame).
    Press,
    /// The input must be newly released (was pressed last frame, now released).
    Release,
    /// The input must be currently held down.
    Hold,
}

/// A single element within a command sequence.
///
/// Command sequences are made up of directional and button elements, optionally
/// grouped as simultaneous inputs (e.g., `a+b`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandElement {
    /// A directional input with a modifier.
    Dir {
        /// Which direction token to match.
        token: DirToken,
        /// How the direction must be input.
        modifier: InputModifier,
    },
    /// A button input with a modifier.
    Button {
        /// Which button to match.
        button: Button,
        /// How the button must be input.
        modifier: InputModifier,
    },
    /// Multiple inputs that must occur on the same frame.
    Simultaneous(Vec<CommandElement>),
}

/// A complete command definition parsed from a MUGEN `.cmd` file.
///
/// Contains the sequence of elements to match and timing constraints.
#[derive(Debug, Clone)]
pub struct CommandDef {
    /// Name of the command (referenced by CNS triggers).
    pub name: String,
    /// Sequence of input elements to match, in order.
    pub elements: Vec<CommandElement>,
    /// Maximum number of ticks to complete the entire sequence.
    pub time: u32,
    /// Number of ticks the command stays active after detection.
    pub buffer_time: u32,
}

/// Tracks a successfully matched command and its remaining active duration.
struct CommandResult {
    /// Name of the matched command.
    name: String,
    /// Ticks remaining before this result expires.
    remaining: u32,
}

/// Matches input buffer contents against command definitions.
///
/// Call [`CommandMatcher::check_commands`] once per tick to scan for newly
/// completed commands, then use [`CommandMatcher::command_active`] or
/// [`CommandMatcher::consume`] to query results from state controllers.
pub struct CommandMatcher {
    /// Registered command definitions.
    commands: Vec<CommandDef>,
    /// Currently active (matched and not yet expired) commands.
    active: Vec<CommandResult>,
}

impl CommandMatcher {
    /// Creates a new matcher with the given command definitions.
    pub fn new(commands: Vec<CommandDef>) -> Self {
        Self {
            commands,
            active: Vec::new(),
        }
    }

    /// Checks all commands against the input buffer. Call once per tick.
    ///
    /// Decrements active command timers, removes expired ones, then attempts
    /// to match each command definition by scanning backward through the buffer.
    pub fn check_commands(&mut self, buffer: &InputBuffer, facing_right: bool) {
        // Decrement active timers and remove expired
        for result in &mut self.active {
            result.remaining = result.remaining.saturating_sub(1);
        }
        self.active.retain(|r| r.remaining > 0);

        // Try to match each command
        for cmd in &self.commands {
            if self.active.iter().any(|r| r.name == cmd.name) {
                continue; // Already active, don't re-match
            }
            if Self::try_match(cmd, buffer, facing_right) {
                self.active.push(CommandResult {
                    name: cmd.name.clone(),
                    remaining: cmd.buffer_time,
                });
            }
        }
    }

    /// Returns `true` if the named command is currently active.
    pub fn command_active(&self, name: &str) -> bool {
        self.active.iter().any(|r| r.name == name)
    }

    /// Returns `true` and removes the command if it is active (consuming it).
    ///
    /// This prevents the same command match from triggering multiple times.
    pub fn consume(&mut self, name: &str) -> bool {
        if let Some(pos) = self.active.iter().position(|r| r.name == name) {
            self.active.remove(pos);
            true
        } else {
            false
        }
    }

    /// Attempts to match a command definition against the buffer by scanning
    /// backward through frames.
    ///
    /// Elements are matched in reverse order (last element = most recent frame).
    /// Each element must match a distinct, earlier frame, all within the `time`
    /// window.
    fn try_match(cmd: &CommandDef, buffer: &InputBuffer, facing_right: bool) -> bool {
        if cmd.elements.is_empty() || buffer.is_empty() {
            return false;
        }

        let max_frames = cmd.time.min(buffer.len() as u32) as usize;
        let mut elem_idx = cmd.elements.len() - 1; // start from last element
        let mut frame_offset = 0usize;

        loop {
            if frame_offset >= max_frames {
                return false; // Ran out of time window
            }

            let matched = Self::element_matches(
                &cmd.elements[elem_idx],
                buffer,
                frame_offset,
                facing_right,
            );

            if matched {
                if elem_idx == 0 {
                    return true; // All elements matched
                }
                elem_idx -= 1;
            }

            frame_offset += 1;
        }
    }

    /// Checks whether a single command element matches at the given frame offset.
    fn element_matches(
        element: &CommandElement,
        buffer: &InputBuffer,
        frame_offset: usize,
        facing_right: bool,
    ) -> bool {
        match element {
            CommandElement::Dir { token, modifier } => {
                let Some(current) = buffer.get(frame_offset) else {
                    return false;
                };
                let logical = logical_direction(&current.direction, facing_right);

                match modifier {
                    InputModifier::Hold => dir_matches(&logical, *token),
                    InputModifier::Press => {
                        if !dir_matches(&logical, *token) {
                            return false;
                        }
                        // For press, the previous frame should NOT match
                        if let Some(prev) = buffer.get(frame_offset + 1) {
                            let prev_logical =
                                logical_direction(&prev.direction, facing_right);
                            !dir_matches(&prev_logical, *token)
                        } else {
                            true // No previous frame = first frame = counts as press
                        }
                    }
                    InputModifier::Release => {
                        if dir_matches(&logical, *token) {
                            return false;
                        }
                        // For release, the previous frame SHOULD match
                        if let Some(prev) = buffer.get(frame_offset + 1) {
                            let prev_logical =
                                logical_direction(&prev.direction, facing_right);
                            dir_matches(&prev_logical, *token)
                        } else {
                            false
                        }
                    }
                }
            }
            CommandElement::Button { button, modifier } => {
                let Some(current) = buffer.get(frame_offset) else {
                    return false;
                };
                let pressed = current.button(*button);

                match modifier {
                    InputModifier::Hold => pressed,
                    InputModifier::Press => {
                        if !pressed {
                            return false;
                        }
                        // Previous frame should NOT have the button pressed
                        if let Some(prev) = buffer.get(frame_offset + 1) {
                            !prev.button(*button)
                        } else {
                            true
                        }
                    }
                    InputModifier::Release => {
                        if pressed {
                            return false;
                        }
                        // Previous frame SHOULD have the button pressed
                        if let Some(prev) = buffer.get(frame_offset + 1) {
                            prev.button(*button)
                        } else {
                            false
                        }
                    }
                }
            }
            CommandElement::Simultaneous(elements) => {
                elements.iter().all(|e| {
                    Self::element_matches(e, buffer, frame_offset, facing_right)
                })
            }
        }
    }
}

/// Parses a MUGEN command string into a vector of command elements.
///
/// Supports the following syntax:
/// - Direction tokens: `U`, `D`, `F`, `B`, `UF`, `UB`, `DF`, `DB`
/// - Button tokens: `a`, `b`, `c`, `x`, `y`, `z`, `s` (case-insensitive)
/// - `~` prefix: release modifier
/// - `/` prefix: hold modifier
/// - `+` separator: simultaneous inputs (e.g., `a+b`)
/// - `,` separator: sequential elements
///
/// # Examples
///
/// ```
/// use fp_input::command::compile_command;
///
/// let elements = compile_command("D, DF, F, x").unwrap();
/// assert_eq!(elements.len(), 4);
/// ```
pub fn compile_command(raw: &str) -> FpResult<Vec<CommandElement>> {
    let mut elements = Vec::new();

    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        // Check for simultaneous inputs (a+b)
        if part.contains('+') {
            let sub_parts: Vec<&str> = part.split('+').collect();
            let mut simultaneous = Vec::new();
            for sub in sub_parts {
                simultaneous.push(parse_single_token(sub.trim())?);
            }
            elements.push(CommandElement::Simultaneous(simultaneous));
        } else {
            elements.push(parse_single_token(part)?);
        }
    }

    if elements.is_empty() {
        return Err(FpError::parse("CMD", "empty command string"));
    }

    Ok(elements)
}

/// Parses a single command token (possibly with modifier prefix).
///
/// In MUGEN, case disambiguates buttons from directions: lowercase `b` is the
/// B button, while uppercase `B` is the Back direction. Buttons are checked
/// first (case-sensitive), then directions (case-insensitive).
fn parse_single_token(token: &str) -> FpResult<CommandElement> {
    let mut chars = token.chars().peekable();
    let mut modifier = InputModifier::Press;

    // Check for modifier prefix
    if let Some(&ch) = chars.peek() {
        match ch {
            '~' => {
                modifier = InputModifier::Release;
                chars.next();
            }
            '/' => {
                modifier = InputModifier::Hold;
                chars.next();
            }
            _ => {}
        }
    }

    let remaining: String = chars.collect();

    // Try case-sensitive button tokens first (lowercase = button)
    match remaining.as_str() {
        "a" => {
            return Ok(CommandElement::Button {
                button: Button::A,
                modifier,
            })
        }
        "b" => {
            return Ok(CommandElement::Button {
                button: Button::B,
                modifier,
            })
        }
        "c" => {
            return Ok(CommandElement::Button {
                button: Button::C,
                modifier,
            })
        }
        "x" => {
            return Ok(CommandElement::Button {
                button: Button::X,
                modifier,
            })
        }
        "y" => {
            return Ok(CommandElement::Button {
                button: Button::Y,
                modifier,
            })
        }
        "z" => {
            return Ok(CommandElement::Button {
                button: Button::Z,
                modifier,
            })
        }
        "s" => {
            return Ok(CommandElement::Button {
                button: Button::Start,
                modifier,
            })
        }
        _ => {}
    }

    // Then try direction tokens (case-insensitive, check two-char before single)
    let upper = remaining.to_uppercase();
    match upper.as_str() {
        "UF" => Ok(CommandElement::Dir {
            token: DirToken::UF,
            modifier,
        }),
        "UB" => Ok(CommandElement::Dir {
            token: DirToken::UB,
            modifier,
        }),
        "DF" => Ok(CommandElement::Dir {
            token: DirToken::DF,
            modifier,
        }),
        "DB" => Ok(CommandElement::Dir {
            token: DirToken::DB,
            modifier,
        }),
        "U" => Ok(CommandElement::Dir {
            token: DirToken::U,
            modifier,
        }),
        "D" => Ok(CommandElement::Dir {
            token: DirToken::D,
            modifier,
        }),
        "F" => Ok(CommandElement::Dir {
            token: DirToken::F,
            modifier,
        }),
        "B" => Ok(CommandElement::Dir {
            token: DirToken::B,
            modifier,
        }),
        _ => Err(FpError::parse(
            "CMD",
            format!("unknown command token: '{remaining}'"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_simple_button() {
        let elements = compile_command("x").unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0],
            CommandElement::Button {
                button: Button::X,
                modifier: InputModifier::Press,
            }
        );
    }

    #[test]
    fn compile_direction_sequence() {
        let elements = compile_command("D, DF, F, x").unwrap();
        assert_eq!(elements.len(), 4);
        assert_eq!(
            elements[0],
            CommandElement::Dir {
                token: DirToken::D,
                modifier: InputModifier::Press,
            }
        );
        assert_eq!(
            elements[1],
            CommandElement::Dir {
                token: DirToken::DF,
                modifier: InputModifier::Press,
            }
        );
        assert_eq!(
            elements[2],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
            }
        );
        assert_eq!(
            elements[3],
            CommandElement::Button {
                button: Button::X,
                modifier: InputModifier::Press,
            }
        );
    }

    #[test]
    fn compile_release_modifier() {
        let elements = compile_command("~x").unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0],
            CommandElement::Button {
                button: Button::X,
                modifier: InputModifier::Release,
            }
        );
    }

    #[test]
    fn compile_hold_modifier() {
        let elements = compile_command("/x").unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0],
            CommandElement::Button {
                button: Button::X,
                modifier: InputModifier::Hold,
            }
        );
    }

    #[test]
    fn compile_simultaneous() {
        let elements = compile_command("a+b").unwrap();
        assert_eq!(elements.len(), 1);
        match &elements[0] {
            CommandElement::Simultaneous(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(
                    parts[0],
                    CommandElement::Button {
                        button: Button::A,
                        modifier: InputModifier::Press,
                    }
                );
                assert_eq!(
                    parts[1],
                    CommandElement::Button {
                        button: Button::B,
                        modifier: InputModifier::Press,
                    }
                );
            }
            other => panic!("expected Simultaneous, got {other:?}"),
        }
    }

    /// Helper: build an InputState with the given direction and buttons.
    fn make_state(dir: Direction, buttons: &[Button]) -> InputState {
        let mut state = InputState {
            direction: dir,
            ..Default::default()
        };
        for &btn in buttons {
            state.set_button(btn, true);
        }
        state
    }

    #[test]
    fn matcher_qcf_detection() {
        // Quarter-circle forward + x: D, DF, F, x
        let cmd = CommandDef {
            name: "QCF_x".into(),
            elements: compile_command("D, DF, F, x").unwrap(),
            time: 15,
            buffer_time: 3,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();

        // Push neutral frames first
        for _ in 0..5 {
            buffer.push(InputState::default());
        }

        // Down (press)
        buffer.push(make_state(
            Direction {
                down: true,
                ..Default::default()
            },
            &[],
        ));
        // Down-forward (hold modifier not needed for default Press matching of dirs)
        buffer.push(make_state(
            Direction {
                down: true,
                right: true,
                ..Default::default()
            },
            &[],
        ));
        // Forward
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        // X button press
        buffer.push(make_state(Direction::default(), &[Button::X]));

        matcher.check_commands(&buffer, true);
        assert!(matcher.command_active("QCF_x"));
    }

    #[test]
    fn matcher_timing_window() {
        // Command with a very tight time window
        let cmd = CommandDef {
            name: "tight".into(),
            elements: compile_command("D, F, x").unwrap(),
            time: 3, // Only 3 frames to complete
            buffer_time: 3,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();

        // Push neutral frames
        for _ in 0..10 {
            buffer.push(InputState::default());
        }

        // Down
        buffer.push(make_state(
            Direction {
                down: true,
                ..Default::default()
            },
            &[],
        ));
        // Many neutral frames (too many for the tight window)
        for _ in 0..5 {
            buffer.push(InputState::default());
        }
        // Forward
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        // X
        buffer.push(make_state(Direction::default(), &[Button::X]));

        matcher.check_commands(&buffer, true);
        assert!(!matcher.command_active("tight"));
    }

    #[test]
    fn matcher_buffer_expiry() {
        let cmd = CommandDef {
            name: "test_expire".into(),
            elements: compile_command("x").unwrap(),
            time: 2,          // Only look back 2 frames
            buffer_time: 2,   // Active for 2 ticks after detection
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();

        // Push neutral then button press
        buffer.push(InputState::default());
        buffer.push(make_state(Direction::default(), &[Button::X]));

        // Tick 1: command detected, remaining = 2
        matcher.check_commands(&buffer, true);
        assert!(matcher.command_active("test_expire"));

        // Tick 2: remaining decremented to 1, still active (skip re-match)
        buffer.push(InputState::default());
        matcher.check_commands(&buffer, true);
        assert!(matcher.command_active("test_expire"));

        // Tick 3: remaining decremented to 0 and removed.
        // X press is now at frames_ago=2, which equals time=2, so the
        // matcher won't find it within the window and cannot re-match.
        buffer.push(InputState::default());
        matcher.check_commands(&buffer, true);
        assert!(!matcher.command_active("test_expire"));
    }

    #[test]
    fn matcher_consume() {
        let cmd = CommandDef {
            name: "consume_test".into(),
            elements: compile_command("x").unwrap(),
            time: 15,
            buffer_time: 5,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();

        buffer.push(InputState::default());
        buffer.push(make_state(Direction::default(), &[Button::X]));

        matcher.check_commands(&buffer, true);
        assert!(matcher.consume("consume_test"));
        assert!(!matcher.consume("consume_test"));
    }
}
