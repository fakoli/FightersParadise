//! MUGEN command sequence parsing and matching.
//!
//! This module handles the recognition of special move input sequences. Command
//! definitions (from `.cmd` files) are compiled into [`CommandDef`] structures,
//! and the [`CommandMatcher`] checks them against the [`crate::buffer::InputBuffer`]
//! each tick to detect when the player has executed a command.

use crate::buffer::InputBuffer;
use crate::state::*;
use fp_core::{FpError, FpResult};
use serde::{Deserialize, Serialize};

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
        /// MUGEN `$` direction-detect: when `true`, the token only requires its
        /// component axis to be held (e.g. `$F` matches F, UF, or DF) instead of
        /// an exact cardinal match. See [`crate::state::dir_matches_detect`].
        detect: bool,
        /// MUGEN `>` strict-immediate: when `true`, this element must occur on
        /// the input frame directly preceding the *next* element in the
        /// sequence, with no other distinct input frame between them.
        strict: bool,
    },
    /// A button input with a modifier.
    Button {
        /// Which button to match.
        button: Button,
        /// How the button must be input.
        modifier: InputModifier,
        /// MUGEN `>` strict-immediate: when `true`, this element must occur on
        /// the input frame directly preceding the *next* element in the
        /// sequence, with no other distinct input frame between them.
        strict: bool,
    },
    /// Multiple inputs that must occur on the same frame.
    Simultaneous(Vec<CommandElement>),
}

impl CommandElement {
    /// Returns whether this element carries the MUGEN `>` strict-immediate flag.
    ///
    /// A strict element must be matched on the input frame directly preceding the
    /// element that follows it in the command sequence (no gap allowed). For a
    /// [`CommandElement::Simultaneous`] group, the flag of its first member
    /// governs the whole group.
    fn is_strict(&self) -> bool {
        match self {
            CommandElement::Dir { strict, .. } => *strict,
            CommandElement::Button { strict, .. } => *strict,
            CommandElement::Simultaneous(parts) => {
                parts.first().is_some_and(CommandElement::is_strict)
            }
        }
    }
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

/// A serializable snapshot of a [`CommandMatcher`]'s **transient recognition
/// state** (replay / rollback, #38).
///
/// Captures only the currently-active (matched, not-yet-expired) commands and
/// their remaining buffer timers — the recognition state that carries across
/// ticks. The compiled [`CommandDef`]s (parsed from the character's `.cmd`) are
/// **static** and are *not* captured: [`CommandMatcher::restore_snapshot`] is
/// applied to a matcher already built from the same `.cmd`. A snapshot whose
/// names are not in the live matcher's vocabulary are dropped on restore (never
/// panics).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandMatcherSnapshot {
    /// The `(name, remaining_ticks)` of each active command.
    active: Vec<(String, u32)>,
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

    /// Returns the names of every command active on the current tick.
    ///
    /// Duplicates never occur because an already-active command is not re-matched,
    /// and callers must not rely on any particular ordering. To snapshot into a
    /// character's command source, prefer [`active_command_names_in`], which bounds
    /// the result to that character's own command vocabulary and gives a stable
    /// order.
    ///
    /// [`active_command_names_in`]: CommandMatcher::active_command_names_in
    #[must_use]
    pub fn active_command_names(&self) -> Vec<String> {
        self.active.iter().map(|r| r.name.clone()).collect()
    }

    /// Returns the names from `defs` that are active this tick, in `defs` order.
    ///
    /// This is the single shared snapshot primitive: it bounds the matcher's
    /// active set to a character's own command vocabulary (and yields a
    /// deterministic order) before the names are handed to a command source.
    /// Borrowing from `defs` avoids cloning. Both the two-player
    /// `fp_engine::Match` and the single-character `fp-app` path use it, so the
    /// filter logic lives in exactly one place.
    #[must_use]
    pub fn active_command_names_in<'a>(&self, defs: &'a [CommandDef]) -> Vec<&'a str> {
        defs.iter()
            .map(|d| d.name.as_str())
            .filter(|name| self.command_active(name))
            .collect()
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

    /// Captures the matcher's transient recognition state (active commands and
    /// their remaining timers) as a serializable [`CommandMatcherSnapshot`]
    /// (replay / rollback, #38).
    ///
    /// The active commands are emitted **sorted by name** so the snapshot bytes
    /// are deterministic regardless of match order. The compiled command
    /// definitions are not captured (they are static; see
    /// [`CommandMatcherSnapshot`]).
    #[must_use]
    pub fn snapshot(&self) -> CommandMatcherSnapshot {
        let mut active: Vec<(String, u32)> = self
            .active
            .iter()
            .map(|r| (r.name.clone(), r.remaining))
            .collect();
        active.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        CommandMatcherSnapshot { active }
    }

    /// Restores the matcher's transient recognition state from a
    /// [`CommandMatcherSnapshot`].
    ///
    /// Replaces the active-command set with the snapshot's. The compiled command
    /// definitions are untouched (they are static; this matcher must already be
    /// built from the same `.cmd`). Never panics; a name that is no longer in the
    /// vocabulary is simply restored as an active result and will expire normally.
    pub fn restore_snapshot(&mut self, snap: &CommandMatcherSnapshot) {
        self.active = snap
            .active
            .iter()
            .map(|(name, remaining)| CommandResult {
                name: name.clone(),
                remaining: *remaining,
            })
            .collect();
    }

    /// Attempts to match a command definition against the buffer by scanning
    /// backward through frames.
    ///
    /// Elements are matched in reverse order (last element = most recent frame).
    /// Each element must match a distinct, earlier frame, all within the `time`
    /// window.
    ///
    /// The MUGEN `>` (strict-immediate) flag on an element constrains the frame
    /// gap to the element that *follows* it in the sequence: when element `i+1`
    /// is strict, element `i` must sit on the input frame directly before the
    /// frame that matched element `i+1` (no other distinct frame between them).
    fn try_match(cmd: &CommandDef, buffer: &InputBuffer, facing_right: bool) -> bool {
        if cmd.elements.is_empty() || buffer.is_empty() {
            return false;
        }

        let max_frames = cmd.time.min(buffer.len() as u32) as usize;
        let mut elem_idx = cmd.elements.len() - 1; // start from last element
        let mut frame_offset = 0usize;
        // When the element we just matched is strict, the next (earlier) element
        // must be exactly one frame older — no gap permitted.
        let mut require_immediate = false;

        loop {
            if frame_offset >= max_frames {
                return false; // Ran out of time window
            }

            let matched =
                Self::element_matches(&cmd.elements[elem_idx], buffer, frame_offset, facing_right);

            if matched {
                if elem_idx == 0 {
                    return true; // All elements matched
                }
                // A `>` on the current element means the *previous* element in
                // the sequence (the next one we look for) must immediately
                // precede this one.
                require_immediate = cmd.elements[elem_idx].is_strict();
                elem_idx -= 1;
                frame_offset += 1;
                continue;
            }

            // Element did not match at this frame. If the next element we are
            // looking for is required to be immediately adjacent, any non-match
            // at the very next frame breaks the strict sequence.
            if require_immediate {
                return false;
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
            CommandElement::Dir {
                token,
                modifier,
                detect,
                ..
            } => {
                let Some(current) = buffer.get(frame_offset) else {
                    return false;
                };
                let logical = logical_direction(&current.direction, facing_right);
                // `$` direction-detect relaxes the cardinal exact-match to an
                // "axis held" check (e.g. `$F` matches F, UF, or DF).
                let dir_hit = |l: &LogicalDirection| -> bool {
                    if *detect {
                        dir_matches_detect(l, *token)
                    } else {
                        dir_matches(l, *token)
                    }
                };

                match modifier {
                    InputModifier::Hold => dir_hit(&logical),
                    InputModifier::Press => {
                        if !dir_hit(&logical) {
                            return false;
                        }
                        // For press, the previous frame should NOT match
                        if let Some(prev) = buffer.get(frame_offset + 1) {
                            let prev_logical = logical_direction(&prev.direction, facing_right);
                            !dir_hit(&prev_logical)
                        } else {
                            true // No previous frame = first frame = counts as press
                        }
                    }
                    InputModifier::Release => {
                        if dir_hit(&logical) {
                            return false;
                        }
                        // For release, the previous frame SHOULD match
                        if let Some(prev) = buffer.get(frame_offset + 1) {
                            let prev_logical = logical_direction(&prev.direction, facing_right);
                            dir_hit(&prev_logical)
                        } else {
                            false
                        }
                    }
                }
            }
            CommandElement::Button {
                button, modifier, ..
            } => {
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
/// - `$` prefix (directions only): direction-detect — the token only requires
///   its component axis to be held, so `$F` matches F, UF, or DF. This is the
///   basis of MUGEN's `holdfwd`/`holdback`/etc. commands (`/$F`, `/$B`, ...).
/// - `>` prefix: strict-immediate — the element must occur on the input frame
///   directly preceding the next element in the sequence (no gap allowed).
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
///
/// // MUGEN holdfwd: hold + direction-detect forward.
/// let holdfwd = compile_command("/$F").unwrap();
/// assert_eq!(holdfwd.len(), 1);
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

/// Parses a single command token (possibly with modifier prefixes).
///
/// In MUGEN, case disambiguates buttons from directions: lowercase `b` is the
/// B button, while uppercase `B` is the Back direction. Buttons are checked
/// first (case-sensitive), then directions (case-insensitive).
///
/// Recognised prefixes (any order, each at most once):
/// - `>` strict-immediate (this element must directly precede the next one)
/// - `~` release / `/` hold (mutually exclusive; default is press)
/// - `$` direction-detect (directions only; relaxes the cardinal exact-match)
fn parse_single_token(token: &str) -> FpResult<CommandElement> {
    let mut chars = token.chars().peekable();
    let mut modifier = InputModifier::Press;
    let mut detect = false;
    let mut strict = false;
    let mut have_modifier = false;

    // Consume any leading prefix symbols. Order is not enforced (real `.cmd`
    // files vary), but each prefix may appear at most once.
    while let Some(&ch) = chars.peek() {
        match ch {
            '>' => {
                if strict {
                    return Err(FpError::parse("CMD", "duplicate '>' in command token"));
                }
                strict = true;
                chars.next();
            }
            '$' => {
                if detect {
                    return Err(FpError::parse("CMD", "duplicate '$' in command token"));
                }
                detect = true;
                chars.next();
            }
            '~' | '/' => {
                if have_modifier {
                    return Err(FpError::parse(
                        "CMD",
                        "conflicting '~'/'/' modifiers in command token",
                    ));
                }
                modifier = if ch == '~' {
                    InputModifier::Release
                } else {
                    InputModifier::Hold
                };
                have_modifier = true;
                chars.next();
            }
            _ => break,
        }
    }

    let remaining: String = chars.collect();

    // Try case-sensitive button tokens first (lowercase = button).
    let button = match remaining.as_str() {
        "a" => Some(Button::A),
        "b" => Some(Button::B),
        "c" => Some(Button::C),
        "x" => Some(Button::X),
        "y" => Some(Button::Y),
        "z" => Some(Button::Z),
        "s" => Some(Button::Start),
        _ => None,
    };
    if let Some(button) = button {
        // `$` (direction-detect) is meaningless on a button.
        if detect {
            return Err(FpError::parse(
                "CMD",
                format!("'$' direction-detect is not valid on button '{remaining}'"),
            ));
        }
        return Ok(CommandElement::Button {
            button,
            modifier,
            strict,
        });
    }

    // Then try direction tokens (case-insensitive, check two-char before single).
    let upper = remaining.to_uppercase();
    let dir_token = match upper.as_str() {
        "UF" => Some(DirToken::UF),
        "UB" => Some(DirToken::UB),
        "DF" => Some(DirToken::DF),
        "DB" => Some(DirToken::DB),
        "U" => Some(DirToken::U),
        "D" => Some(DirToken::D),
        "F" => Some(DirToken::F),
        "B" => Some(DirToken::B),
        _ => None,
    };
    match dir_token {
        Some(token) => Ok(CommandElement::Dir {
            token,
            modifier,
            detect,
            strict,
        }),
        None => Err(FpError::parse(
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
                strict: false,
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
                detect: false,
                strict: false,
            }
        );
        assert_eq!(
            elements[1],
            CommandElement::Dir {
                token: DirToken::DF,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
            }
        );
        assert_eq!(
            elements[2],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
            }
        );
        assert_eq!(
            elements[3],
            CommandElement::Button {
                button: Button::X,
                modifier: InputModifier::Press,
                strict: false,
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
                strict: false,
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
                strict: false,
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
                        strict: false,
                    }
                );
                assert_eq!(
                    parts[1],
                    CommandElement::Button {
                        button: Button::B,
                        modifier: InputModifier::Press,
                        strict: false,
                    }
                );
            }
            other => panic!("expected Simultaneous, got {other:?}"),
        }
    }

    #[test]
    fn compile_direction_detect() {
        // `$F` => direction-detect forward, default press modifier.
        let elements = compile_command("$F").unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
                detect: true,
                strict: false,
            }
        );
    }

    #[test]
    fn compile_holdfwd_style() {
        // MUGEN holdfwd: `/$F` => hold + direction-detect forward.
        let elements = compile_command("/$F").unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Hold,
                detect: true,
                strict: false,
            }
        );
    }

    #[test]
    fn compile_strict_prefix() {
        // `>F` => strict-immediate forward.
        let elements = compile_command(">F").unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
                detect: false,
                strict: true,
            }
        );
    }

    #[test]
    fn compile_strict_release_button() {
        // `>~a` => strict + release on the A button (prefix order: > then ~).
        let elements = compile_command(">~a").unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0],
            CommandElement::Button {
                button: Button::A,
                modifier: InputModifier::Release,
                strict: true,
            }
        );
    }

    #[test]
    fn compile_detect_on_button_is_error() {
        // `$` is meaningless on a button and must be rejected, not silently kept.
        assert!(compile_command("$a").is_err());
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

    #[test]
    fn active_command_names_reports_all_active() {
        // `active_command_names` must list every active command (the shared
        // snapshot helper both fp-engine and fp-app use) and be empty when none
        // are active.
        let cmds = vec![
            CommandDef {
                name: "holdfwd".into(),
                elements: compile_command("/$F").unwrap(),
                time: 1,
                buffer_time: 1,
            },
            CommandDef {
                name: "punch".into(),
                elements: compile_command("x").unwrap(),
                time: 1,
                buffer_time: 1,
            },
        ];
        let mut matcher = CommandMatcher::new(cmds);

        // Nothing active on an empty buffer.
        let buffer = InputBuffer::new();
        matcher.check_commands(&buffer, true);
        assert!(matcher.active_command_names().is_empty());

        // Hold Forward + press X: both commands active.
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[Button::X],
        ));
        matcher.check_commands(&buffer, true);
        let names = matcher.active_command_names();
        assert!(names.iter().any(|n| n == "holdfwd"), "holdfwd active: {names:?}");
        assert!(names.iter().any(|n| n == "punch"), "punch active: {names:?}");
        assert_eq!(names.len(), 2, "exactly the two active commands: {names:?}");
        // It agrees with `command_active` for every name.
        for n in &names {
            assert!(matcher.command_active(n));
        }
    }

    #[test]
    fn active_command_names_in_bounds_and_orders_by_defs() {
        // `active_command_names_in` returns the defs that are active, in DEFS order
        // (not matcher order), excluding defs that are not active. This is the
        // shared snapshot primitive fp-engine and fp-app both call.
        let mut matcher = CommandMatcher::new(vec![
            holdfwd_cmd(),
            CommandDef {
                name: "punch".into(),
                elements: compile_command("x").unwrap(),
                time: 1,
                buffer_time: 1,
            },
        ]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[Button::X],
        ));
        matcher.check_commands(&buffer, true);

        // Pass defs in a DIFFERENT order than the matcher, with an extra inactive
        // "kick": the result is bounded to active defs, in defs order.
        let defs = vec![
            CommandDef {
                name: "punch".into(),
                elements: compile_command("x").unwrap(),
                time: 1,
                buffer_time: 1,
            },
            holdfwd_cmd(),
            CommandDef {
                name: "kick".into(),
                elements: compile_command("y").unwrap(),
                time: 1,
                buffer_time: 1,
            },
        ];
        assert_eq!(
            matcher.active_command_names_in(&defs),
            vec!["punch", "holdfwd"],
            "active defs in defs order, inactive 'kick' excluded"
        );
    }

    /// Builds the MUGEN `holdfwd` command (`/$F`, time = 1) used by KFM's
    /// `[Statedef -1]` walk bridge.
    fn holdfwd_cmd() -> CommandDef {
        CommandDef {
            name: "holdfwd".into(),
            elements: compile_command("/$F").unwrap(),
            time: 1,
            buffer_time: 1,
        }
    }

    #[test]
    fn matcher_holdfwd_active_when_forward_held() {
        let mut matcher = CommandMatcher::new(vec![holdfwd_cmd()]);
        let mut buffer = InputBuffer::new();

        // Hold Forward (facing right => hardware right is forward).
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));

        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("holdfwd"),
            "holding Forward should activate holdfwd (/$F)"
        );
    }

    #[test]
    fn matcher_holdfwd_active_on_diagonal_forward() {
        // Direction-detect: `$F` must also fire on UF / DF (forward held at any
        // vertical angle), which is what distinguishes it from plain `/F`.
        let mut matcher = CommandMatcher::new(vec![holdfwd_cmd()]);
        let mut buffer = InputBuffer::new();

        buffer.push(make_state(
            Direction {
                right: true,
                up: true,
                ..Default::default()
            },
            &[],
        ));

        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("holdfwd"),
            "holding up-forward should still activate holdfwd via direction-detect"
        );
    }

    #[test]
    fn matcher_holdfwd_inactive_when_back_held() {
        let mut matcher = CommandMatcher::new(vec![holdfwd_cmd()]);
        let mut buffer = InputBuffer::new();

        // Facing right => hardware left is Back. holdfwd must NOT be active.
        buffer.push(make_state(
            Direction {
                left: true,
                ..Default::default()
            },
            &[],
        ));

        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("holdfwd"),
            "holding Back must not activate holdfwd"
        );
    }

    #[test]
    fn matcher_holdfwd_inactive_when_neutral() {
        let mut matcher = CommandMatcher::new(vec![holdfwd_cmd()]);
        let mut buffer = InputBuffer::new();
        buffer.push(InputState::default());

        matcher.check_commands(&buffer, true);
        assert!(!matcher.command_active("holdfwd"));
    }

    #[test]
    fn detect_vs_plain_on_diagonal() {
        // Plain `/F` requires an exact Forward (no vertical), so up-forward must
        // NOT match it — whereas `/$F` does (covered above).
        let plain = CommandDef {
            name: "plainfwd".into(),
            elements: compile_command("/F").unwrap(),
            time: 1,
            buffer_time: 1,
        };
        let mut matcher = CommandMatcher::new(vec![plain]);
        let mut buffer = InputBuffer::new();

        buffer.push(make_state(
            Direction {
                right: true,
                up: true,
                ..Default::default()
            },
            &[],
        ));

        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("plainfwd"),
            "plain /F must not match up-forward (only /$F does)"
        );
    }

    #[test]
    fn matcher_strict_requires_adjacent_frames() {
        // `F, >a` : the A press must land on the frame *immediately* after the
        // Forward frame. A gap between them breaks the strict sequence.
        let cmd = CommandDef {
            name: "strict_fa".into(),
            elements: compile_command("F, >a").unwrap(),
            time: 15,
            buffer_time: 3,
        };

        // --- Adjacent: should match. ---
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(make_state(Direction::default(), &[Button::A]));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("strict_fa"),
            "F immediately followed by a must satisfy `F, >a`"
        );

        // --- With a gap frame between F and a: should NOT match. ---
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(InputState::default()); // intervening neutral frame
        buffer.push(make_state(Direction::default(), &[Button::A]));
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("strict_fa"),
            "a gap between F and a must break the strict `>a` sequence"
        );
    }

    // ====================================================================
    // Proctor: additional command-layer coverage
    // (acceptance: `$`, `>`, holdfwd matching, error paths, MUGEN semantics)
    // ====================================================================

    // ---- compile error / edge paths ------------------------------------

    #[test]
    fn compile_empty_string_is_error() {
        // A wholly empty command value cannot be matched and must be rejected.
        assert!(compile_command("").is_err());
        assert!(compile_command("   ").is_err());
        // Only separators, no tokens.
        assert!(compile_command(",,,").is_err());
    }

    #[test]
    fn compile_unknown_token_is_error() {
        assert!(compile_command("Q").is_err());
        assert!(compile_command("D, DF, Z9").is_err());
        // A lone modifier with no token after it.
        assert!(compile_command("/").is_err());
        assert!(compile_command("~").is_err());
        assert!(compile_command("$").is_err());
        assert!(compile_command(">").is_err());
    }

    #[test]
    fn compile_skips_blank_segments_between_commas() {
        // Trailing/leading/extra commas are tolerated (real .cmd files vary in
        // whitespace) so long as at least one real token remains.
        let elements = compile_command(" D , , F , ").unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(
            elements[0],
            CommandElement::Dir {
                token: DirToken::D,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
            }
        );
        assert_eq!(
            elements[1],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
            }
        );
    }

    #[test]
    fn compile_conflicting_hold_and_release_is_error() {
        // `~` and `/` are mutually exclusive on one token.
        assert!(compile_command("~/a").is_err());
        assert!(compile_command("/~a").is_err());
    }

    #[test]
    fn compile_duplicate_detect_is_error() {
        assert!(compile_command("$$F").is_err());
    }

    #[test]
    fn compile_duplicate_strict_is_error() {
        assert!(compile_command(">>F").is_err());
    }

    #[test]
    fn compile_button_case_sensitivity() {
        // Lowercase => button; uppercase same letter => direction.
        // lowercase b = B button.
        assert_eq!(
            compile_command("b").unwrap()[0],
            CommandElement::Button {
                button: Button::B,
                modifier: InputModifier::Press,
                strict: false,
            }
        );
        // Uppercase B = Back direction.
        assert_eq!(
            compile_command("B").unwrap()[0],
            CommandElement::Dir {
                token: DirToken::B,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
            }
        );
    }

    #[test]
    fn compile_direction_case_insensitive() {
        // Direction tokens are case-insensitive (`df` == `DF`).
        let lower = compile_command("df").unwrap();
        assert_eq!(
            lower[0],
            CommandElement::Dir {
                token: DirToken::DF,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
            }
        );
    }

    #[test]
    fn compile_prefix_order_is_flexible() {
        // `$/F`, `/$F`, even with a leading `>` in any order, all yield the same
        // logical element (hold + detect + strict forward). Real .cmd authors
        // are inconsistent about ordering.
        let want = CommandElement::Dir {
            token: DirToken::F,
            modifier: InputModifier::Hold,
            detect: true,
            strict: true,
        };
        for src in [">/$F", ">$/F", "/$>F", "$/>F", "/>$F"] {
            let got = compile_command(src).unwrap();
            assert_eq!(got.len(), 1, "src `{src}` should compile to one element");
            assert_eq!(got[0], want, "src `{src}` mis-parsed");
        }
    }

    #[test]
    fn compile_detect_on_diagonal_direction() {
        // `$DF` is valid (direction-detect on a diagonal).
        assert_eq!(
            compile_command("$DF").unwrap()[0],
            CommandElement::Dir {
                token: DirToken::DF,
                modifier: InputModifier::Press,
                detect: true,
                strict: false,
            }
        );
    }

    #[test]
    fn compile_detect_on_start_button_is_error() {
        // `$` is meaningless on the Start button just like any other button.
        assert!(compile_command("$s").is_err());
    }

    #[test]
    fn compile_simultaneous_with_modifiers() {
        // KFM uses `x+y` and similar; ensure each sub-token keeps its modifier.
        let elements = compile_command("~F, D, DF, x+y").unwrap();
        assert_eq!(elements.len(), 4);
        match &elements[3] {
            CommandElement::Simultaneous(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(
                    parts[0],
                    CommandElement::Button {
                        button: Button::X,
                        modifier: InputModifier::Press,
                        strict: false,
                    }
                );
                assert_eq!(
                    parts[1],
                    CommandElement::Button {
                        button: Button::Y,
                        modifier: InputModifier::Press,
                        strict: false,
                    }
                );
            }
            other => panic!("expected Simultaneous, got {other:?}"),
        }
        // The leading `~F` carries the release modifier.
        assert_eq!(
            elements[0],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Release,
                detect: false,
                strict: false,
            }
        );
    }

    #[test]
    fn compile_simultaneous_with_bad_subtoken_is_error() {
        // If any side of a `+` group is invalid, the whole compile fails.
        assert!(compile_command("a+Q").is_err());
    }

    // ---- matcher: holdfwd / direction-detect semantics -----------------

    #[test]
    fn matcher_holdfwd_facing_left_uses_hardware_left() {
        // When facing LEFT, Forward is hardware LEFT. holdfwd must fire on
        // hardware-left and NOT on hardware-right — proving facing-relative
        // resolution flows through the matcher, not just `logical_direction`.
        let mut matcher = CommandMatcher::new(vec![holdfwd_cmd()]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                left: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buffer, /* facing_right = */ false);
        assert!(
            matcher.command_active("holdfwd"),
            "facing left: hardware-left is Forward, holdfwd must fire"
        );

        // Conversely hardware-right while facing left is Back: no holdfwd.
        let mut matcher = CommandMatcher::new(vec![holdfwd_cmd()]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buffer, false);
        assert!(
            !matcher.command_active("holdfwd"),
            "facing left: hardware-right is Back, holdfwd must NOT fire"
        );
    }

    #[test]
    fn matcher_holdfwd_active_on_down_forward() {
        // Direction-detect must also fire on DF (the third diagonal alongside
        // the UF case already covered), per `$F` = F | UF | DF.
        let mut matcher = CommandMatcher::new(vec![holdfwd_cmd()]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                down: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("holdfwd"),
            "down-forward must satisfy holdfwd via direction-detect"
        );
    }

    #[test]
    fn matcher_plain_holdfwd_matches_pure_forward() {
        // Sanity: the non-detect hold `/F` DOES fire on a pure Forward hold
        // (it only fails on diagonals). Guards against an over-broad fix that
        // would make `/F` and `/$F` indistinguishable.
        let plain = CommandDef {
            name: "plainfwd".into(),
            elements: compile_command("/F").unwrap(),
            time: 1,
            buffer_time: 1,
        };
        let mut matcher = CommandMatcher::new(vec![plain]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buffer, true);
        assert!(matcher.command_active("plainfwd"));
    }

    // ---- matcher: `>` strict semantics ---------------------------------

    #[test]
    fn matcher_chained_strict_all_adjacent() {
        // KFM-style chained strict: `F, >~F, >F` requires three consecutive
        // input frames F (held), F released, F (held) again with NO gaps.
        let cmd = CommandDef {
            name: "ffdash".into(),
            elements: compile_command("F, >~F, >F").unwrap(),
            time: 15,
            buffer_time: 3,
        };

        // Adjacent F / not-F / F across three back-to-back frames => match.
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        let fwd = Direction {
            right: true,
            ..Default::default()
        };
        buffer.push(make_state(fwd, &[])); // F
        buffer.push(make_state(Direction::default(), &[])); // ~F (released)
        buffer.push(make_state(fwd, &[])); // F
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("ffdash"),
            "three adjacent frames F/~F/F must satisfy the chained-strict run"
        );

        // Insert a gap between the release and the final F => strict broken.
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(fwd, &[])); // F
        buffer.push(make_state(Direction::default(), &[])); // ~F
        buffer.push(make_state(Direction::default(), &[])); // extra neutral gap
        buffer.push(make_state(fwd, &[])); // F (too late, gap before it)
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("ffdash"),
            "a gap inside the chained-strict run must break it"
        );
    }

    #[test]
    fn matcher_non_strict_tolerates_gaps() {
        // The same motion WITHOUT `>` (plain `D, F, x`) is forgiving of gaps,
        // confirming `>` is what tightens the timing, not the matcher in general.
        let cmd = CommandDef {
            name: "loose".into(),
            elements: compile_command("D, F, x").unwrap(),
            time: 15,
            buffer_time: 3,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(
            Direction {
                down: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(InputState::default()); // gap
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(InputState::default()); // gap
        buffer.push(make_state(Direction::default(), &[Button::X]));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("loose"),
            "non-strict sequence should match across gap frames"
        );
    }

    #[test]
    fn matcher_strict_on_simultaneous_group() {
        // `is_strict` for a Simultaneous group is governed by its first member.
        // Compile `F, >a+b`: the a+b group is strict, so it must land directly
        // after F.
        let cmd = CommandDef {
            name: "strict_simul".into(),
            elements: compile_command("F, >a+b").unwrap(),
            time: 15,
            buffer_time: 3,
        };
        // Confirm the parsed structure: group's first element carries strict.
        assert!(
            matches!(
                &cmd.elements[1],
                CommandElement::Simultaneous(parts)
                    if matches!(parts[0], CommandElement::Button { strict: true, .. })
            ),
            "first member of the simultaneous group should carry the `>` flag"
        );

        // Adjacent => match.
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(make_state(Direction::default(), &[Button::A, Button::B]));
        matcher.check_commands(&buffer, true);
        assert!(matcher.command_active("strict_simul"));

        // Gap before the group => strict broken.
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(InputState::default());
        buffer.push(make_state(Direction::default(), &[Button::A, Button::B]));
        matcher.check_commands(&buffer, true);
        assert!(!matcher.command_active("strict_simul"));
    }

    #[test]
    fn matcher_strict_held_input_straddling_boundary() {
        // Pins the intended behavior when a *held* input straddles a `>`
        // strict boundary. Command `/F, >a`: Forward is HELD (so it can be
        // satisfied on more than one adjacent frame), and the A press must sit
        // on the frame immediately after a Forward frame.
        //
        // The matcher resolves strict adjacency by frame position, not by
        // requiring a *distinct* edge: the held-Forward frame directly before
        // the A frame counts as the strict-adjacent hit. This is the correct
        // outcome for hold-style elements — Forward genuinely is held on the
        // adjacent frame — and is the behavior we deliberately lock in here.
        let cmd = CommandDef {
            name: "hold_strict".into(),
            elements: compile_command("/F, >a").unwrap(),
            time: 15,
            buffer_time: 3,
        };
        let fwd = Direction {
            right: true,
            ..Default::default()
        };

        // Forward held across two frames, then A pressed while still holding
        // Forward. The frame immediately before the A frame is a Forward-held
        // frame, so the strict `>a` adjacency is satisfied.
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(fwd, &[])); // Forward held (frame N-2)
        buffer.push(make_state(fwd, &[])); // Forward STILL held (frame N-1)
        buffer.push(make_state(fwd, &[Button::A])); // A pressed, Forward held (frame N)
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("hold_strict"),
            "held Forward on the frame adjacent to the A press must satisfy `/F, >a`"
        );

        // Conversely, if Forward is NOT held on the frame immediately before
        // the A press (a neutral frame intervenes), the strict adjacency is
        // broken even though A itself is fine.
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(fwd, &[])); // Forward held
        buffer.push(make_state(Direction::default(), &[])); // neutral gap (Forward dropped)
        buffer.push(make_state(Direction::default(), &[Button::A])); // A press, no Forward
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("hold_strict"),
            "a non-Forward frame immediately before the A press must break `>a`"
        );
    }

    // ---- matcher: misc semantics & guards ------------------------------

    #[test]
    fn matcher_empty_elements_never_matches() {
        // A CommandDef with no elements is degenerate and must never fire
        // (and must not panic on an empty buffer either).
        let cmd = CommandDef {
            name: "empty".into(),
            elements: Vec::new(),
            time: 15,
            buffer_time: 3,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let buffer = InputBuffer::new();
        matcher.check_commands(&buffer, true);
        assert!(!matcher.command_active("empty"));

        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[Button::X]));
        matcher.check_commands(&buffer, true);
        assert!(!matcher.command_active("empty"));
    }

    #[test]
    fn matcher_empty_buffer_no_match_no_panic() {
        let cmd = CommandDef {
            name: "qcf".into(),
            elements: compile_command("D, DF, F, x").unwrap(),
            time: 15,
            buffer_time: 3,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let buffer = InputBuffer::new();
        matcher.check_commands(&buffer, true);
        assert!(!matcher.command_active("qcf"));
    }

    #[test]
    fn matcher_does_not_rematch_while_active() {
        // Once active, a command is not re-pushed (no duplicate stacking), and
        // it stays active for exactly `buffer_time` ticks of holding.
        let cmd = CommandDef {
            name: "hold_a".into(),
            elements: compile_command("/a").unwrap(),
            time: 1,
            buffer_time: 4,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[Button::A]));
        matcher.check_commands(&buffer, true);
        assert!(matcher.command_active("hold_a"));

        // Keep holding for several ticks: still active, but consume removes it
        // exactly once (proving a single result, not a stack of duplicates).
        for _ in 0..2 {
            buffer.push(make_state(Direction::default(), &[Button::A]));
            matcher.check_commands(&buffer, true);
        }
        assert!(matcher.consume("hold_a"));
        assert!(
            !matcher.command_active("hold_a"),
            "a single consume must clear the only result (no duplicate matches stacked)"
        );
    }

    #[test]
    fn matcher_release_modifier_detection() {
        // `~a` (release) fires when the A button was held last frame and is now
        // up. Verifies the press/release edge logic end-to-end.
        let cmd = CommandDef {
            name: "rel_a".into(),
            elements: compile_command("~a").unwrap(),
            time: 5,
            buffer_time: 2,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        // Held, then released.
        buffer.push(make_state(Direction::default(), &[Button::A]));
        buffer.push(make_state(Direction::default(), &[]));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("rel_a"),
            "A held then released must satisfy `~a`"
        );
    }

    #[test]
    fn matcher_release_not_triggered_by_continuous_hold() {
        // `~a` must NOT fire while A stays held (no rising-to-falling edge).
        let cmd = CommandDef {
            name: "rel_a".into(),
            elements: compile_command("~a").unwrap(),
            time: 5,
            buffer_time: 2,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[Button::A]));
        buffer.push(make_state(Direction::default(), &[Button::A]));
        matcher.check_commands(&buffer, true);
        assert!(!matcher.command_active("rel_a"));
    }

    #[test]
    fn matcher_press_requires_rising_edge() {
        // A button held across two frames is NOT a fresh press on the latest
        // frame (the previous frame already had it down).
        let cmd = CommandDef {
            name: "press_a".into(),
            elements: compile_command("a").unwrap(),
            time: 1, // only inspect the most recent frame
            buffer_time: 2,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[Button::A])); // prev: down
        buffer.push(make_state(Direction::default(), &[Button::A])); // cur: still down
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("press_a"),
            "a held button is not a fresh press within a 1-frame window"
        );
    }

    #[test]
    fn matcher_qcf_with_leading_release_real_kfm() {
        // KFM's real QCF is `~D, DF, F, x` (lead with release-of-down). Drive a
        // realistic stick path and assert detection — exercises the actual
        // shipping motion, not a simplified one.
        let cmd = CommandDef {
            name: "QCF_x".into(),
            elements: compile_command("~D, DF, F, x").unwrap(),
            time: 20,
            buffer_time: 4,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        // Establish D held first (so the later "release of D" has an edge).
        buffer.push(make_state(
            Direction {
                down: true,
                ..Default::default()
            },
            &[],
        ));
        // ~D : down no longer held (released) — here represented by DF where
        // down is dropped; use a clean not-down frame to satisfy release.
        buffer.push(make_state(
            Direction {
                down: false,
                ..Default::default()
            },
            &[],
        ));
        // DF
        buffer.push(make_state(
            Direction {
                down: true,
                right: true,
                ..Default::default()
            },
            &[],
        ));
        // F
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        // x
        buffer.push(make_state(Direction::default(), &[Button::X]));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("QCF_x"),
            "realistic KFM QCF motion `~D, DF, F, x` should be detected"
        );
    }

    // ====================================================================
    // Proctor (round 2): real-KFM command *shapes* not yet matcher-tested.
    // Each mirrors a literal `command = ...` line from test-assets/kfm/kfm.cmd
    // so the synthetic coverage tracks the genuine fixture.
    // ====================================================================

    #[test]
    fn matcher_double_tap_forward_dash() {
        // KFM `command = F, F` (forward dash). Two distinct Forward presses
        // within the time window. The non-strict matcher tolerates a neutral
        // gap between the taps (you must release between presses to re-press).
        let cmd = CommandDef {
            name: "FF".into(),
            elements: compile_command("F, F").unwrap(),
            time: 15,
            buffer_time: 3,
        };
        let fwd = Direction {
            right: true,
            ..Default::default()
        };

        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(fwd, &[])); // first tap
        buffer.push(InputState::default()); // release between taps
        buffer.push(make_state(fwd, &[])); // second tap
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("FF"),
            "two Forward taps with a release between must satisfy `F, F`"
        );

        // A single sustained Forward hold is NOT two presses: with no release
        // between, the earlier element's press-edge can't be found, so `F, F`
        // must not fire.
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        for _ in 0..3 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(fwd, &[]));
        buffer.push(make_state(fwd, &[]));
        buffer.push(make_state(fwd, &[]));
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("FF"),
            "a single sustained Forward hold is not a double-tap dash"
        );
    }

    #[test]
    fn matcher_double_qcf_repeated_motion() {
        // KFM super: `command = ~D, DF, F, D, DF, F, x` (two quarter-circles
        // then x). Drives the full repeated motion and asserts detection,
        // proving the backward scan handles a long, repeated-token sequence.
        let cmd = CommandDef {
            name: "super_x".into(),
            elements: compile_command("~D, DF, F, D, DF, F, x").unwrap(),
            time: 40,
            buffer_time: 4,
        };
        assert_eq!(cmd.elements.len(), 7, "double-QCF + x is seven elements");

        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();

        let down = Direction {
            down: true,
            ..Default::default()
        };
        let df = Direction {
            down: true,
            right: true,
            ..Default::default()
        };
        let fwd = Direction {
            right: true,
            ..Default::default()
        };

        // Establish D held so the leading `~D` has a falling edge to detect.
        buffer.push(make_state(down, &[]));
        // ~D : down released.
        buffer.push(make_state(Direction::default(), &[]));
        // First quarter-circle: DF, F.
        buffer.push(make_state(df, &[]));
        buffer.push(make_state(fwd, &[]));
        // Second quarter-circle: D, DF, F.
        buffer.push(make_state(down, &[]));
        buffer.push(make_state(df, &[]));
        buffer.push(make_state(fwd, &[]));
        // Finisher.
        buffer.push(make_state(Direction::default(), &[Button::X]));

        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("super_x"),
            "the full double-QCF motion `~D, DF, F, D, DF, F, x` must be detected"
        );
    }

    #[test]
    fn matcher_holddir_down_then_button_chain() {
        // KFM `command = /$D,a` : hold Down (direction-detect) AND press a.
        // The `a` press is on the most recent frame; the frame before it must
        // have Down held (detect). Mirrors a hold-direction + attack chain.
        let cmd = CommandDef {
            name: "lowkick".into(),
            elements: compile_command("/$D,a").unwrap(),
            time: 15,
            buffer_time: 3,
        };
        // Structure check: hold + detect Down, then a plain `a` press.
        assert_eq!(
            cmd.elements[0],
            CommandElement::Dir {
                token: DirToken::D,
                modifier: InputModifier::Hold,
                detect: true,
                strict: false,
            }
        );

        let down = Direction {
            down: true,
            ..Default::default()
        };
        let df = Direction {
            down: true,
            right: true,
            ..Default::default()
        };

        // Down held across two frames, then a pressed while still holding Down.
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(down, &[]));
        buffer.push(make_state(down, &[Button::A]));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("lowkick"),
            "Down held with `a` pressed must satisfy `/$D,a`"
        );

        // Direction-detect: holding Down-Forward (DF) must still satisfy `$D`.
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(df, &[]));
        buffer.push(make_state(df, &[Button::A]));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("lowkick"),
            "Down-Forward held (detect) with `a` must still satisfy `/$D,a`"
        );

        // No Down held => must not fire even though `a` is pressed.
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[]));
        buffer.push(make_state(Direction::default(), &[Button::A]));
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("lowkick"),
            "`a` without Down held must not satisfy `/$D,a`"
        );
    }

    #[test]
    fn compile_all_four_holddir_commands() {
        // KFM defines all four hold-direction commands: `/$F /$B /$U /$D`.
        // Each must compile to a single hold + direction-detect element on the
        // matching cardinal token. This is the basis of holdfwd/holdback/etc.
        for (src, token) in [
            ("/$F", DirToken::F),
            ("/$B", DirToken::B),
            ("/$U", DirToken::U),
            ("/$D", DirToken::D),
        ] {
            let elements = compile_command(src).unwrap();
            assert_eq!(elements.len(), 1, "`{src}` should be one element");
            assert_eq!(
                elements[0],
                CommandElement::Dir {
                    token,
                    modifier: InputModifier::Hold,
                    detect: true,
                    strict: false,
                },
                "`{src}` must be hold + direction-detect {token:?}"
            );
        }
    }

    #[test]
    fn matcher_holdback_fires_only_on_back() {
        // `/$B` (holdback) is the mirror of holdfwd: facing right, hardware LEFT
        // is Back. holdback must fire on hardware-left and not on hardware-right.
        let cmd = CommandDef {
            name: "holdback".into(),
            elements: compile_command("/$B").unwrap(),
            time: 1,
            buffer_time: 1,
        };

        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                left: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("holdback"),
            "facing right: hardware-left is Back, holdback `/$B` must fire"
        );

        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("holdback"),
            "facing right: hardware-right is Forward, holdback `/$B` must NOT fire"
        );
    }

    #[test]
    fn matcher_release_of_direction_detect() {
        // `~$F` : release of a direction-detect Forward. The detect flag must
        // flow through the release branch — it fires when forward (at ANY
        // vertical angle) was held last frame and is no longer held now.
        let cmd = CommandDef {
            name: "rel_detect_f".into(),
            elements: compile_command("~$F").unwrap(),
            time: 5,
            buffer_time: 2,
        };
        // Structure: release + detect on Forward.
        assert_eq!(
            cmd.elements[0],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Release,
                detect: true,
                strict: false,
            }
        );

        // Up-forward held last frame (detect forward), neutral now => release.
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                up: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(make_state(Direction::default(), &[]));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("rel_detect_f"),
            "up-forward released must satisfy `~$F` via direction-detect release"
        );

        // Still holding forward => no release edge, must not fire.
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("rel_detect_f"),
            "continuously held Forward has no release edge for `~$F`"
        );
    }

    #[test]
    fn matcher_time_window_is_exclusive_upper_bound() {
        // Off-by-one guard on the `time` window. A single `x` press sitting
        // exactly `time` frames in the past must NOT match (the window scans
        // frame offsets `0..time`, i.e. `time` itself is out of range).
        let cmd = CommandDef {
            name: "x_press".into(),
            elements: compile_command("x").unwrap(),
            time: 3,
            buffer_time: 2,
        };

        // x at offset 2 (== time-1): inside the window => matches.
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[Button::X])); // offset 2 after two pushes
        buffer.push(InputState::default()); // offset 1
        buffer.push(InputState::default()); // offset 0
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("x_press"),
            "x at offset 2 (time-1) is within the window and must match"
        );

        // x at offset 3 (== time): just outside the window => no match.
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[Button::X])); // offset 3
        buffer.push(InputState::default()); // offset 2
        buffer.push(InputState::default()); // offset 1
        buffer.push(InputState::default()); // offset 0
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("x_press"),
            "x at offset 3 (== time) is outside the window and must NOT match"
        );
    }

    #[test]
    fn matcher_multiple_commands_independent_activation() {
        // The matcher holds several commands at once; matching one must not
        // affect the others. Register holdfwd, holdback and a button press.
        let cmds = vec![
            CommandDef {
                name: "holdfwd".into(),
                elements: compile_command("/$F").unwrap(),
                time: 1,
                buffer_time: 2,
            },
            CommandDef {
                name: "holdback".into(),
                elements: compile_command("/$B").unwrap(),
                time: 1,
                buffer_time: 2,
            },
            CommandDef {
                name: "a_press".into(),
                elements: compile_command("a").unwrap(),
                time: 2,
                buffer_time: 2,
            },
        ];
        let mut matcher = CommandMatcher::new(cmds);
        let mut buffer = InputBuffer::new();
        // Forward + a pressed: holdfwd and a_press fire, holdback does not.
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[Button::A],
        ));
        matcher.check_commands(&buffer, true);
        assert!(matcher.command_active("holdfwd"));
        assert!(matcher.command_active("a_press"));
        assert!(
            !matcher.command_active("holdback"),
            "holding Forward must not activate holdback"
        );
        // Consuming one leaves the others intact.
        assert!(matcher.consume("holdfwd"));
        assert!(!matcher.command_active("holdfwd"));
        assert!(
            matcher.command_active("a_press"),
            "consuming holdfwd must not disturb a_press"
        );
    }

    #[test]
    fn consume_unknown_command_is_false() {
        // Consuming/querying a never-registered name must be a clean `false`,
        // never a panic.
        let mut matcher = CommandMatcher::new(vec![CommandDef {
            name: "real".into(),
            elements: compile_command("x").unwrap(),
            time: 5,
            buffer_time: 2,
        }]);
        assert!(!matcher.command_active("ghost"));
        assert!(!matcher.consume("ghost"));
    }

    // ====================================================================
    // Proctor (round 3): degenerate-timing guards, simultaneous mixing,
    // strict-offset edges, prefix/error completeness, and behavioral pins
    // for quirks surfaced during review. None of these change impl behavior;
    // they lock in the engine's contract so a regression is caught.
    // ====================================================================

    // ---- degenerate timing windows (never panic, never mis-fire) -------

    #[test]
    fn matcher_time_zero_never_matches() {
        // `time = 0` means a zero-frame scan window. `max_frames` is 0, so the
        // loop returns immediately without inspecting any frame: no command may
        // fire and nothing panics, even with a perfectly-matching input present.
        let cmd = CommandDef {
            name: "zero".into(),
            elements: compile_command("x").unwrap(),
            time: 0,
            buffer_time: 2,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[Button::X]));
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("zero"),
            "a zero-frame time window must never match"
        );
    }

    #[test]
    fn matcher_buffer_time_zero_is_active_only_the_match_tick() {
        // Behavioral pin: `buffer_time = 0` pushes a result with `remaining = 0`.
        // Because timers are decremented+retained at the *start* of the tick
        // (before this tick's matches are pushed), a freshly-matched zero-buffer
        // command is reported active on the tick it completes...
        let cmd = CommandDef {
            name: "bt0".into(),
            elements: compile_command("x").unwrap(),
            time: 5,
            buffer_time: 0,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[Button::X]));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("bt0"),
            "buffer_time=0 command is active on the tick it matches"
        );

        // ...and is gone by the very next tick (the retain at tick start drops
        // the remaining==0 result before any re-match attempt). With the X press
        // now aged out of the (here ample) window the press is gone anyway, but
        // the key invariant is that a zero-buffer result never lingers.
        buffer.push(InputState::default());
        matcher.check_commands(&buffer, true);
        // Push enough neutral frames that the original X is far in the past.
        for _ in 0..6 {
            buffer.push(InputState::default());
        }
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("bt0"),
            "buffer_time=0 result must not persist past its match tick"
        );
    }

    // ---- simultaneous groups mixing directions and buttons -------------

    #[test]
    fn matcher_simultaneous_direction_and_button_same_frame() {
        // `$F+x` (direction-detect Forward together with the x button) must fire
        // only when BOTH land on the *same* frame — the defining property of a
        // Simultaneous group. KFM's `blocking` is the real-world analogue.
        let cmd = CommandDef {
            name: "blockx".into(),
            elements: compile_command("$F+x").unwrap(),
            time: 2,
            buffer_time: 2,
        };
        // Confirm it parsed to a single simultaneous group of {detect-F, x}.
        assert!(
            matches!(&cmd.elements[0], CommandElement::Simultaneous(p) if p.len() == 2),
            "`$F+x` must compile to one two-member simultaneous group"
        );

        // Same frame => fires.
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[Button::X],
        ));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("blockx"),
            "Forward + x on the same frame must satisfy `$F+x`"
        );

        // Forward then x on the NEXT frame (not simultaneous) => must not fire.
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(make_state(Direction::default(), &[Button::X]));
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("blockx"),
            "Forward and x on different frames must NOT satisfy a simultaneous `$F+x`"
        );
    }

    #[test]
    fn matcher_simultaneous_detect_fires_on_diagonal() {
        // The `$` inside a simultaneous group must still relax to direction-
        // detect: `$F+x` fires on up-forward + x (not just pure forward + x).
        let cmd = CommandDef {
            name: "blockx".into(),
            elements: compile_command("$F+x").unwrap(),
            time: 2,
            buffer_time: 2,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(
            Direction {
                right: true,
                up: true,
                ..Default::default()
            },
            &[Button::X],
        ));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("blockx"),
            "up-forward + x must satisfy `$F+x` via direction-detect inside the group"
        );
    }

    // ---- strict-flag edge cases ----------------------------------------

    #[test]
    fn matcher_strict_flag_on_final_element_is_harmless() {
        // A `>` on the LAST element has no following element to constrain, so it
        // must behave exactly like the same command without `>`. (The flag only
        // gates the gap to the *next* element, of which there is none.)
        let strict = CommandDef {
            name: "s".into(),
            elements: compile_command(">F").unwrap(),
            time: 5,
            buffer_time: 2,
        };
        let plain = CommandDef {
            name: "p".into(),
            elements: compile_command("F").unwrap(),
            time: 5,
            buffer_time: 2,
        };
        let mut matcher = CommandMatcher::new(vec![strict, plain]);
        let mut buffer = InputBuffer::new();
        // Forward a few frames in the past (a gap before "now") — both should
        // still match because neither has a *following* element to constrain.
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(InputState::default());
        matcher.check_commands(&buffer, true);
        assert_eq!(
            matcher.command_active("s"),
            matcher.command_active("p"),
            "a trailing `>` must not change matching vs. the un-decorated command"
        );
        assert!(matcher.command_active("p"), "plain `F` should match");
    }

    #[test]
    fn matcher_strict_breaks_on_gap_before_final_element() {
        // `x, >a`: the `>` lives on `a`, the final element. Per the matcher,
        // `require_immediate` is set when the element we *just matched* (a, at
        // offset 0) is strict, constraining the PRECEDING element (x) to be on
        // the immediately-next frame. A gap between x and a must break it.
        let cmd = CommandDef {
            name: "xa".into(),
            elements: compile_command("x, >a").unwrap(),
            time: 8,
            buffer_time: 2,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[Button::X])); // offset 2
        buffer.push(InputState::default()); // offset 1: gap
        buffer.push(make_state(Direction::default(), &[Button::A])); // offset 0
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("xa"),
            "a gap between x and the strict `>a` must break the sequence"
        );
    }

    // ---- consume / re-match interaction --------------------------------

    #[test]
    fn matcher_rematches_next_tick_after_consume_if_input_persists() {
        // Behavioral pin: `consume` clears the active result, but it does not
        // suppress a fresh match on a later tick. If the triggering input is
        // still inside the `time` window, the command re-activates next tick.
        // (Per-tick dedup is `command_active`/`consume`; there is no edge-latch
        // beyond the active list.) Callers that want one-shot semantics must
        // consume every tick the input lingers.
        let cmd = CommandDef {
            name: "hold_a".into(),
            elements: compile_command("/a").unwrap(),
            time: 2,
            buffer_time: 3,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        buffer.push(make_state(Direction::default(), &[Button::A]));
        matcher.check_commands(&buffer, true);
        assert!(matcher.consume("hold_a"));
        assert!(!matcher.command_active("hold_a"));

        // Still holding A next tick => re-matches.
        buffer.push(make_state(Direction::default(), &[Button::A]));
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("hold_a"),
            "a persisting hold must re-activate after a prior consume"
        );
    }

    // ---- compile: prefix / error completeness --------------------------

    #[test]
    fn compile_strict_on_simultaneous_subtoken_inner() {
        // A `>` may appear on a member *inside* a `+` group (e.g. `>a+b`). The
        // group's `is_strict` is governed by its first member, so the flag must
        // be recorded on that first member.
        let elements = compile_command(">a+b").unwrap();
        assert_eq!(elements.len(), 1);
        match &elements[0] {
            CommandElement::Simultaneous(parts) => {
                assert_eq!(
                    parts[0],
                    CommandElement::Button {
                        button: Button::A,
                        modifier: InputModifier::Press,
                        strict: true,
                    },
                    "first member of `>a+b` must carry the strict flag"
                );
                assert_eq!(
                    parts[1],
                    CommandElement::Button {
                        button: Button::B,
                        modifier: InputModifier::Press,
                        strict: false,
                    }
                );
            }
            other => panic!("expected Simultaneous, got {other:?}"),
        }
    }

    #[test]
    fn compile_all_six_buttons_plus_start() {
        // Every MUGEN button token maps to the right Button variant. `s` is the
        // Start button (lowercase), distinct from `$`/direction tokens.
        let cases = [
            ("a", Button::A),
            ("b", Button::B),
            ("c", Button::C),
            ("x", Button::X),
            ("y", Button::Y),
            ("z", Button::Z),
            ("s", Button::Start),
        ];
        for (src, btn) in cases {
            assert_eq!(
                compile_command(src).unwrap()[0],
                CommandElement::Button {
                    button: btn,
                    modifier: InputModifier::Press,
                    strict: false,
                },
                "`{src}` should compile to button {btn:?}"
            );
        }
    }

    #[test]
    fn compile_does_not_strip_comments() {
        // `compile_command` is given an already-cleaned value by the loader; it
        // must NOT itself try to interpret `;` (a CMD comment marker). A value
        // containing `;` therefore fails as an unknown token rather than being
        // silently truncated — pinning the layering contract so the loader, not
        // the compiler, owns comment stripping.
        assert!(
            compile_command("F;junk").is_err(),
            "compile_command must not strip `;` comments itself"
        );
    }

    #[test]
    fn compile_uppercase_button_letters_are_directions_or_errors() {
        // Case disambiguation: uppercase `A` is NOT the A button (buttons are
        // lowercase). `A` is not a direction token either, so it must error;
        // `B` uppercase, however, is the Back direction.
        assert!(
            compile_command("A").is_err(),
            "uppercase `A` is neither a button nor a direction"
        );
        assert_eq!(
            compile_command("B").unwrap()[0],
            CommandElement::Dir {
                token: DirToken::B,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
            }
        );
    }

    #[test]
    fn compile_whitespace_around_plus_is_tolerated() {
        // Real .cmd authors write `a + b` with spaces; each `+`-split sub-token
        // is trimmed, so the spaced form parses identically to `a+b`.
        let spaced = compile_command("a + b").unwrap();
        let tight = compile_command("a+b").unwrap();
        assert_eq!(spaced, tight, "`a + b` must parse identically to `a+b`");
    }

    #[test]
    fn compile_internal_space_in_token_is_error() {
        // A token with an interior space (no separator) is not a valid single
        // token and must be rejected rather than silently accepted.
        assert!(compile_command("x y").is_err());
    }

    #[test]
    fn compile_release_of_detect_diagonal() {
        // `~$DF` — release of a direction-detect diagonal — must carry all three
        // flags (release + detect on DF). Guards against a prefix being dropped
        // when several stack on a diagonal token.
        assert_eq!(
            compile_command("~$DF").unwrap()[0],
            CommandElement::Dir {
                token: DirToken::DF,
                modifier: InputModifier::Release,
                detect: true,
                strict: false,
            }
        );
    }
}
