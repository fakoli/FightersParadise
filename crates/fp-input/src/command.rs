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
        /// MUGEN charge hold-duration: the minimum number of consecutive ticks
        /// the input must have been held immediately before the
        /// [`Release`](InputModifier::Release) edge (e.g. `~60B` requires Back
        /// held for at least 60 ticks before it is let go). `0` means no charge
        /// requirement. Only meaningful with [`InputModifier::Release`]; the
        /// 60-frame ring buffer clamps the effective maximum to 60.
        min_hold: u32,
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
        /// MUGEN charge hold-duration: the minimum number of consecutive ticks
        /// the button must have been held immediately before the
        /// [`Release`](InputModifier::Release) edge (e.g. `~30a` requires the A
        /// button held for at least 30 ticks before it is let go). `0` means no
        /// charge requirement. Only meaningful with [`InputModifier::Release`];
        /// the 60-frame ring buffer clamps the effective maximum to 60.
        min_hold: u32,
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

/// Input-leniency configuration for a [`CommandMatcher`] (T075).
///
/// Buffering is a deterministic, input-layer concern: the same inputs always
/// produce the same buffered commands, so versus determinism is unchanged. All
/// fields default to *off* (`jump_buffer_frames = 0`) so the matcher behaves
/// exactly as before unless leniency is explicitly enabled — existing content,
/// replays, and tests are unaffected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeniencyConfig {
    /// How many frames before the actionable frame an up-press is buffered so a
    /// jump still fires. `0` disables the jump buffer entirely.
    pub jump_buffer_frames: u32,
    /// The command name kept active by a buffered up-press. This is the engine's
    /// built-in jump gate (`holdup`); a buffered jump is intentionally applied
    /// only to that built-in locomotion command, never to authored content
    /// motions (the variable jump height of an authored arc stays the
    /// character's concern).
    pub jump_command: String,
}

impl Default for LeniencyConfig {
    fn default() -> Self {
        Self {
            jump_buffer_frames: 0,
            jump_command: "holdup".to_string(),
        }
    }
}

impl LeniencyConfig {
    /// A leniency config with the jump buffer enabled over the standard `holdup`
    /// gate, using a 3-frame (~50ms at 60Hz) window — short enough that a stale
    /// up-press never resurrects a jump.
    #[must_use]
    pub fn with_jump_buffer() -> Self {
        Self {
            jump_buffer_frames: 3,
            ..Self::default()
        }
    }
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
    /// Input-leniency configuration (jump buffer). Defaults to off.
    leniency: LeniencyConfig,
    /// Names of commands that transitioned from inactive to active on the most
    /// recent [`check_commands`](Self::check_commands) call (i.e. were
    /// *recognized this frame*). Rebuilt every tick; read via
    /// [`just_matched`](Self::just_matched) to flash a command name in the
    /// on-screen input display (T064). An already-active command that merely
    /// stays active is **not** listed — only the frame of recognition.
    just_matched: Vec<String>,
}

impl CommandMatcher {
    /// Creates a new matcher with the given command definitions.
    ///
    /// Input leniency is **off** by default ([`LeniencyConfig::default`]); enable
    /// the jump buffer with [`with_leniency`](Self::with_leniency).
    pub fn new(commands: Vec<CommandDef>) -> Self {
        Self {
            commands,
            active: Vec::new(),
            leniency: LeniencyConfig::default(),
            just_matched: Vec::new(),
        }
    }

    /// Builder variant of [`new`](Self::new) that also installs a
    /// [`LeniencyConfig`] (e.g. [`LeniencyConfig::with_jump_buffer`]).
    #[must_use]
    pub fn with_leniency(commands: Vec<CommandDef>, leniency: LeniencyConfig) -> Self {
        Self {
            commands,
            active: Vec::new(),
            leniency,
            just_matched: Vec::new(),
        }
    }

    /// Checks all commands against the input buffer. Call once per tick.
    ///
    /// Decrements active command timers, removes expired ones, then attempts
    /// to match each command definition by scanning backward through the buffer.
    ///
    /// Finally, when the jump buffer is enabled ([`LeniencyConfig`]), a fresh
    /// up-press within the leniency window keeps the configured `jump_command`
    /// (the engine built-in `holdup` gate) active even if up is no longer held
    /// this exact frame — so a jump tapped a few frames before the player became
    /// actionable still comes out (T075). This is applied *after* normal matching
    /// and only re-arms the single jump gate, so it cannot turn one motion into
    /// another (a `QCF` never becomes a `DP`).
    pub fn check_commands(&mut self, buffer: &InputBuffer, facing_right: bool) {
        // Fresh per tick: only commands recognized *this* frame are recorded.
        self.just_matched.clear();

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
                self.just_matched.push(cmd.name.clone());
            }
        }

        self.apply_jump_buffer(buffer);
    }

    /// Names of commands recognized on the most recent
    /// [`check_commands`](Self::check_commands) call — the commands that
    /// transitioned from inactive to active *this frame*.
    ///
    /// Used by the on-screen input display (T064) to flash a special-move name
    /// the instant its motion completes. A command that was already active and
    /// merely stayed active is **not** included; the list is rebuilt every tick
    /// (empty on a frame with no new recognition).
    #[must_use]
    pub fn just_matched(&self) -> &[String] {
        &self.just_matched
    }

    /// Re-arms the configured jump command from a buffered up-press (T075).
    ///
    /// No-op when the jump buffer is disabled (`jump_buffer_frames == 0`), when
    /// the jump command is not in this matcher's vocabulary, or when it is
    /// already active this tick. Otherwise, if [`InputBuffer::up_pressed_within`]
    /// finds a fresh up-press inside the window, the jump command is activated
    /// with its own `buffer_time` so downstream code reads it exactly as a
    /// freshly-matched `holdup`.
    fn apply_jump_buffer(&mut self, buffer: &InputBuffer) {
        let window = self.leniency.jump_buffer_frames;
        if window == 0 {
            return;
        }
        // Bound the scan to the configured jump gate only — never authored motions.
        let Some(cmd) = self
            .commands
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(&self.leniency.jump_command))
        else {
            return;
        };
        if self.active.iter().any(|r| r.name == cmd.name) {
            return; // already live this tick (real hold or prior buffer)
        }
        if buffer.up_pressed_within(window as usize) {
            // Keep the jump alive for the gate command's own buffer_time so the
            // engine's `command = "holdup"` ChangeState can fire on the first
            // actionable frame. A minimum of 1 ensures a `buffer_time = 0` gate
            // (KFM's instantaneous holds) is still observable this tick.
            let remaining = cmd.buffer_time.max(1);
            let name = cmd.name.clone();
            self.just_matched.push(name.clone());
            self.active.push(CommandResult { name, remaining });
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
                min_hold,
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
                // Per-frame "held" predicate used both for the release edge and
                // for counting the charge run before it.
                let held_at = |ago: usize| -> bool {
                    buffer
                        .get(ago)
                        .map(|s| dir_hit(&logical_direction(&s.direction, facing_right)))
                        .unwrap_or(false)
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
                        // For release, the previous frame SHOULD match.
                        if !held_at(frame_offset + 1) {
                            return false;
                        }
                        // Charge: require the direction held for >= min_hold
                        // consecutive ticks immediately before the release.
                        charge_satisfied(*min_hold, frame_offset + 1, held_at, buffer.len())
                    }
                }
            }
            CommandElement::Button {
                button,
                modifier,
                min_hold,
                ..
            } => {
                let Some(current) = buffer.get(frame_offset) else {
                    return false;
                };
                let pressed = current.button(*button);
                let held_at = |ago: usize| -> bool {
                    buffer.get(ago).map(|s| s.button(*button)).unwrap_or(false)
                };

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
                        // Previous frame SHOULD have the button pressed.
                        if !held_at(frame_offset + 1) {
                            return false;
                        }
                        // Charge: require the button held for >= min_hold
                        // consecutive ticks immediately before the release.
                        charge_satisfied(*min_hold, frame_offset + 1, held_at, buffer.len())
                    }
                }
            }
            CommandElement::Simultaneous(elements) => elements
                .iter()
                .all(|e| Self::element_matches(e, buffer, frame_offset, facing_right)),
        }
    }
}

/// Returns whether a charge hold-duration is satisfied at a release edge.
///
/// `release_held_offset` is the buffer offset of the *held* frame immediately
/// preceding the release (i.e. `frame_offset + 1` for the matched release). The
/// `held` predicate reports whether the charged input was held at a given
/// buffer offset. The function walks backward from `release_held_offset`,
/// counting consecutive held frames, and returns `true` once the run reaches
/// `min_hold`.
///
/// `min_hold == 0` is the no-charge case and returns `true` immediately (the
/// caller has already verified the release edge).
///
/// Boundary behaviour (the documented 60-tick ring clamp): the buffer holds at
/// most 60 frames, so a long charge can run off the oldest recorded frame. When
/// the consecutive-held run reaches the end of the buffer (the oldest frame is
/// still held and there is no earlier frame to disprove the charge), the charge
/// is treated as satisfied — the input was held since before the window opened.
/// This is what lets a buffer saturated with the charged direction fire a
/// `~60`-class command; a charge that is interrupted by a non-held frame within
/// the window is bounded by that gap and must reach `min_hold` to count.
fn charge_satisfied(
    min_hold: u32,
    release_held_offset: usize,
    held: impl Fn(usize) -> bool,
    buffer_len: usize,
) -> bool {
    if min_hold == 0 {
        return true;
    }
    let mut count: u32 = 0;
    let mut ago = release_held_offset;
    while held(ago) {
        count += 1;
        if count >= min_hold {
            return true;
        }
        // Ran off the oldest recorded frame while still held: the charge began
        // before the window, so it cannot be disproven — treat it as charged.
        if ago + 1 >= buffer_len {
            return true;
        }
        ago += 1;
    }
    false
}

/// Parses a MUGEN command string into a vector of command elements.
///
/// Supports the following syntax:
/// - Direction tokens: `U`, `D`, `F`, `B`, `UF`, `UB`, `DF`, `DB`. Diagonals
///   also accept the reversed (horizontal-then-vertical) token order authored
///   by some community characters: `FU`/`BU`/`FD`/`BD` are aliases for
///   `UF`/`UB`/`DF`/`DB` respectively.
/// - Button tokens: `a`, `b`, `c`, `x`, `y`, `z`, `s` (case-insensitive)
/// - `~` prefix: release modifier
/// - `/` prefix: hold modifier
/// - `~NN`/`/NN` prefix: charge hold-duration — the input must have been held
///   for at least `NN` consecutive ticks immediately before the release/hold
///   edge (e.g. `~60B` = release Back after holding it ≥60 ticks). The 60-frame
///   ring buffer clamps the effective maximum to 60.
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
    let mut min_hold: u32 = 0;

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

                // MUGEN charge syntax: a `~`/`/` prefix may be followed by a
                // run of digits giving the minimum hold duration in ticks
                // (e.g. `~60B`). Parse them here so the charge count enforces
                // the hold window in the matcher.
                let mut digits = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        digits.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if !digits.is_empty() {
                    // Saturating parse: an absurdly large charge clamps to the
                    // ring-buffer cap during matching rather than erroring.
                    min_hold = digits.parse::<u32>().unwrap_or(u32::MAX);
                }
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
            min_hold,
        });
    }

    // Then try direction tokens (case-insensitive, check two-char before single).
    //
    // Diagonals are accepted in either token order: the canonical
    // vertical-then-horizontal form (`UF`/`UB`/`DF`/`DB`) and the reversed
    // horizontal-then-vertical aliases (`FU`/`BU`/`FD`/`BD`) authored by many
    // community characters (e.g. CVTW2Ryu). Both orderings map to the same
    // `DirToken`, so a char that writes diagonals in reversed order still
    // compiles every command instead of failing with "unknown command token".
    let upper = remaining.to_uppercase();
    let dir_token = match upper.as_str() {
        "UF" | "FU" => Some(DirToken::UF),
        "UB" | "BU" => Some(DirToken::UB),
        "DF" | "FD" => Some(DirToken::DF),
        "DB" | "BD" => Some(DirToken::DB),
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
            min_hold,
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
                min_hold: 0,
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
                min_hold: 0,
            }
        );
        assert_eq!(
            elements[1],
            CommandElement::Dir {
                token: DirToken::DF,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            }
        );
        assert_eq!(
            elements[2],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            }
        );
        assert_eq!(
            elements[3],
            CommandElement::Button {
                button: Button::X,
                modifier: InputModifier::Press,
                strict: false,
                min_hold: 0,
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
                min_hold: 0,
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
                min_hold: 0,
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
                        min_hold: 0,
                    }
                );
                assert_eq!(
                    parts[1],
                    CommandElement::Button {
                        button: Button::B,
                        modifier: InputModifier::Press,
                        strict: false,
                        min_hold: 0,
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
                min_hold: 0,
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
                min_hold: 0,
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
                min_hold: 0,
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
                min_hold: 0,
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
            time: 2,        // Only look back 2 frames
            buffer_time: 2, // Active for 2 ticks after detection
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

    // ---- T059: charge hold-duration enforcement ----

    /// `~60$B` must parse into a Release direction element carrying
    /// `min_hold == 60` and `detect == true`, the canonical charge form.
    #[test]
    fn compile_charge_token() {
        let elements = compile_command("~60$B").unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(
            elements[0],
            CommandElement::Dir {
                token: DirToken::B,
                modifier: InputModifier::Release,
                detect: true,
                strict: false,
                min_hold: 60,
            }
        );
        // A plain release (no digits) keeps min_hold == 0 (unchanged behaviour).
        assert_eq!(
            compile_command("~B").unwrap()[0],
            CommandElement::Dir {
                token: DirToken::B,
                modifier: InputModifier::Release,
                detect: false,
                strict: false,
                min_hold: 0,
            }
        );
        // Charge on a button (`~30a`) parses too.
        assert_eq!(
            compile_command("~30a").unwrap()[0],
            CommandElement::Button {
                button: Button::A,
                modifier: InputModifier::Release,
                strict: false,
                min_hold: 30,
            }
        );
    }

    /// T058 acceptance: `~NN`/`/NN` charge tokens parse the digit run into
    /// `min_hold` instead of gluing it onto the token and hitting the
    /// `unknown command token` error.
    ///
    /// Covers every acceptance criterion of T058:
    /// - `~60$B` → a `Release` direction element with `min_hold == 60` on `$B`.
    /// - `~D` (no digits) keeps `min_hold == 0` (negative-edge behaviour intact).
    /// - `command = ~60$B, F, x` compiles to a real (non-empty) element list, so
    ///   it never falls back to the const-0 "unknown command token" path.
    #[test]
    fn parse_charge_token() {
        // `~60$B` parses to a charged Release on `$B` carrying `min_hold == 60`.
        assert_eq!(
            compile_command("~60$B").unwrap()[0],
            CommandElement::Dir {
                token: DirToken::B,
                modifier: InputModifier::Release,
                detect: true,
                strict: false,
                min_hold: 60,
            }
        );

        // `~D` (release, no digits) keeps `min_hold == 0`.
        assert_eq!(
            compile_command("~D").unwrap()[0],
            CommandElement::Dir {
                token: DirToken::D,
                modifier: InputModifier::Release,
                detect: false,
                strict: false,
                min_hold: 0,
            }
        );

        // The `/NN` (hold) form parses its digit run too, e.g. `/40F`.
        assert_eq!(
            compile_command("/40F").unwrap()[0],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Hold,
                detect: false,
                strict: false,
                min_hold: 40,
            }
        );

        // A charge on a button (`~30a`) parses its digits too.
        assert_eq!(
            compile_command("~30a").unwrap()[0],
            CommandElement::Button {
                button: Button::A,
                modifier: InputModifier::Release,
                strict: false,
                min_hold: 30,
            }
        );

        // The full `command = ~60$B, F, x` compiles without the
        // `unknown command token` error: three elements, none a fallback.
        let cmd = compile_command("~60$B, F, x").unwrap();
        assert_eq!(cmd.len(), 3, "~60$B, F, x must compile to three elements");
        assert_eq!(
            cmd[0],
            CommandElement::Dir {
                token: DirToken::B,
                modifier: InputModifier::Release,
                detect: true,
                strict: false,
                min_hold: 60,
            }
        );
        assert_eq!(
            cmd[1],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            }
        );
        assert_eq!(
            cmd[2],
            CommandElement::Button {
                button: Button::X,
                modifier: InputModifier::Press,
                strict: false,
                min_hold: 0,
            }
        );
    }

    /// Builds a hardware Back direction (logical Back when facing right).
    fn back_dir() -> Direction {
        Direction {
            left: true,
            ..Default::default()
        }
    }

    /// Builds a hardware Forward direction (logical Forward when facing right).
    fn fwd_dir() -> Direction {
        Direction {
            right: true,
            ..Default::default()
        }
    }

    /// Pushes a `gap?`-Back-release-Forward-x charge sequence onto `buffer`:
    /// optionally a leading non-held (neutral) frame that *bounds* the charge
    /// run, then `hold` ticks of Back held, a neutral frame (the Back release
    /// edge), a Forward frame (the F press), then a Forward+x frame (button).
    /// The release edge sits on its own frame so it does not collide with F.
    ///
    /// With `lead_gap = true`, the run is bounded to exactly `hold` ticks (the
    /// charge count cannot exceed it). With `lead_gap = false`, the Back run
    /// runs off the start of the buffer (held since before the window) — the
    /// documented saturated-ring case.
    fn push_charge_seq(buffer: &mut InputBuffer, hold: u32, lead_gap: bool) {
        if lead_gap {
            buffer.push(InputState::default());
        }
        for _ in 0..hold {
            buffer.push(make_state(back_dir(), &[]));
        }
        buffer.push(InputState::default());
        buffer.push(make_state(fwd_dir(), &[]));
        buffer.push(make_state(fwd_dir(), &[Button::X]));
    }

    /// `~60$B, F, x` fires when Back is held for the full charge (a ring
    /// saturated with Back, then F, x). This is the literal acceptance case.
    /// The exact 60-vs-59 boundary needs a non-held frame to bound the run, and
    /// a 60-tick run + release + F + x cannot all fit the 60-frame ring (the
    /// documented clamp), so the strict N-vs-(N-1) boundary is exercised at a
    /// sub-ring charge in [`charge_boundary_is_exact`].
    #[test]
    fn charge_requires_hold() {
        let cmd = CommandDef {
            name: "charge".into(),
            elements: compile_command("~60$B, F, x").unwrap(),
            time: 12,
            buffer_time: 3,
        };

        // Ring saturated with Back (held since before the window) -> fires.
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        push_charge_seq(&mut buffer, 60, false);
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("charge"),
            "Back held for the full charge then F, x must fire ~60$B,F,x"
        );

        // No charge at all (Back tapped once, bounded, then released) -> no fire.
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        push_charge_seq(&mut buffer, 1, true);
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("charge"),
            "a single-tick Back must NOT fire a 60-tick charge"
        );
    }

    /// The charge boundary is exact: at a sub-ring charge `N` (so a bounding
    /// non-held frame fits in the 60-frame ring), `N` held ticks fire and
    /// `N - 1` do not. `min_hold == 0` is unchanged (a plain release fires
    /// after a single held tick).
    #[test]
    fn charge_boundary_is_exact() {
        let cmd = CommandDef {
            name: "charge50".into(),
            elements: compile_command("~50$B, F, x").unwrap(),
            time: 12,
            buffer_time: 3,
        };

        // Exactly 50 held ticks (bounded by a leading gap) -> fires.
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        push_charge_seq(&mut buffer, 50, true);
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("charge50"),
            "exactly 50 held ticks must fire ~50$B"
        );

        // 49 held ticks (bounded) -> does NOT fire (one short).
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        push_charge_seq(&mut buffer, 49, true);
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("charge50"),
            "49 held ticks must NOT fire ~50$B (one short of the charge)"
        );

        // min_hold == 0 unchanged: a plain `~B, F, x` fires after a single
        // bounded Back tick.
        let plain = CommandDef {
            name: "plain".into(),
            elements: compile_command("~B, F, x").unwrap(),
            time: 12,
            buffer_time: 3,
        };
        let mut matcher = CommandMatcher::new(vec![plain]);
        let mut buffer = InputBuffer::new();
        push_charge_seq(&mut buffer, 1, true);
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("plain"),
            "a plain (min_hold==0) ~B release must be unchanged"
        );
    }

    /// Direction-detect: a `~$B` charge counts diagonal Back-down (DB) frames
    /// toward the hold, since `$B` matches B / DB / UB.
    #[test]
    fn charge_dollar_directiondetect() {
        let cmd = CommandDef {
            name: "charge_detect".into(),
            // Lower charge so the whole sequence fits the 60-frame ring with
            // room for the post-release frames.
            elements: compile_command("~20$B, F, x").unwrap(),
            time: 12,
            buffer_time: 3,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();

        buffer.push(InputState::default());
        // Charge held as Down-Back (DB) the whole time — only counts because
        // `$B` is direction-detect (axis held), not an exact-cardinal Back.
        for _ in 0..20 {
            buffer.push(make_state(
                Direction {
                    left: true,
                    down: true,
                    ..Default::default()
                },
                &[],
            ));
        }
        // Release the back axis (neutral edge), then forward, then the button.
        buffer.push(InputState::default());
        buffer.push(make_state(fwd_dir(), &[]));
        buffer.push(make_state(fwd_dir(), &[Button::X]));

        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("charge_detect"),
            "DB held should count toward the ~$B charge via direction-detect"
        );

        // Negative control: a plain `~20B` (no `$`) does NOT accept DB as the
        // charge, because exact-cardinal Back is never satisfied by DB.
        let cmd_exact = CommandDef {
            name: "charge_exact".into(),
            elements: compile_command("~20B, F, x").unwrap(),
            time: 12,
            buffer_time: 3,
        };
        let mut matcher = CommandMatcher::new(vec![cmd_exact]);
        // Same DB-held buffer.
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("charge_exact"),
            "DB must NOT satisfy an exact (non-$) Back charge"
        );
    }

    /// Documented 60-frame ring clamp. A charge larger than the ring cannot be
    /// distinguished from a saturated ring: when the held run runs off the
    /// oldest recorded frame, the charge is treated as satisfied (held since
    /// before the window) — so a ring saturated with Back fires even a
    /// `~120`-class charge. But a charge run that is *bounded* by a non-held
    /// frame within the window and falls short of `min_hold` never fires,
    /// regardless of how large `min_hold` is.
    #[test]
    fn charge_above_ring_clamp() {
        let cmd = CommandDef {
            name: "huge".into(),
            elements: compile_command("~120$B, F, x").unwrap(),
            time: 12,
            buffer_time: 3,
        };

        // Saturated ring (Back since before the window) -> fires (clamp).
        let mut matcher = CommandMatcher::new(vec![cmd.clone()]);
        let mut buffer = InputBuffer::new();
        push_charge_seq(&mut buffer, 120, false);
        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("huge"),
            "a ring saturated with Back satisfies any charge (held since before window)"
        );

        // Bounded run shorter than the charge -> never fires.
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        push_charge_seq(&mut buffer, 30, true);
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("huge"),
            "a bounded 30-tick run must not fire a 120-tick charge"
        );
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
        assert!(
            names.iter().any(|n| n == "holdfwd"),
            "holdfwd active: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "punch"),
            "punch active: {names:?}"
        );
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
                min_hold: 0,
            }
        );
        assert_eq!(
            elements[1],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
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
                min_hold: 0,
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
                min_hold: 0,
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
                min_hold: 0,
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
            min_hold: 0,
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
                min_hold: 0,
            }
        );
    }

    #[test]
    fn compile_reversed_diagonal_aliases() {
        // Many community chars (e.g. CVTW2Ryu) author diagonals in reversed
        // (horizontal-then-vertical) order: FU/BU/FD/BD. These must compile to
        // the same DirToken as the canonical UF/UB/DF/DB so the character's
        // commands work instead of failing with "unknown command token".
        let cases = [
            ("FU", DirToken::UF),
            ("BU", DirToken::UB),
            ("FD", DirToken::DF),
            ("BD", DirToken::DB),
        ];
        for (src, want) in cases {
            assert_eq!(
                compile_command(src).unwrap()[0],
                CommandElement::Dir {
                    token: want,
                    modifier: InputModifier::Press,
                    detect: false,
                    strict: false,
                    min_hold: 0,
                },
                "reversed-diagonal `{src}` should alias to {want:?}"
            );
        }
    }

    #[test]
    fn compile_reversed_diagonal_aliases_case_insensitive() {
        // Reversed diagonals are case-insensitive just like the canonical ones
        // (`fu` == `FU`).
        for src in ["fu", "Fu", "fU", "FU"] {
            assert_eq!(
                compile_command(src).unwrap()[0],
                CommandElement::Dir {
                    token: DirToken::UF,
                    modifier: InputModifier::Press,
                    detect: false,
                    strict: false,
                    min_hold: 0,
                },
                "`{src}` should alias to UF regardless of case"
            );
        }
    }

    #[test]
    fn compile_reversed_diagonal_aliases_with_prefixes() {
        // The aliases play nicely with the modifier/detect/strict prefixes the
        // same way the canonical tokens do (e.g. `/$FU` == hold + detect + UF).
        assert_eq!(
            compile_command("/$FU").unwrap()[0],
            CommandElement::Dir {
                token: DirToken::UF,
                modifier: InputModifier::Hold,
                detect: true,
                strict: false,
                min_hold: 0,
            }
        );
    }

    #[test]
    fn compile_cvtw2ryu_style_reversed_diagonal_commands() {
        // Acceptance criterion: `~DF, F, FU, x` and `~DB, B, BU, a` (the
        // reversed-diagonal command shapes authored by characters like
        // CVTW2Ryu) compile successfully, FU->UF and BU->UB.
        let qcf_up = compile_command("~DF, F, FU, x").unwrap();
        assert_eq!(qcf_up.len(), 4);
        assert_eq!(
            qcf_up[2],
            CommandElement::Dir {
                token: DirToken::UF,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            },
            "the third element `FU` must alias to UF"
        );

        let qcb_up = compile_command("~DB, B, BU, a").unwrap();
        assert_eq!(qcb_up.len(), 4);
        assert_eq!(
            qcb_up[2],
            CommandElement::Dir {
                token: DirToken::UB,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            },
            "the third element `BU` must alias to UB"
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
                        min_hold: 0,
                    }
                );
                assert_eq!(
                    parts[1],
                    CommandElement::Button {
                        button: Button::Y,
                        modifier: InputModifier::Press,
                        strict: false,
                        min_hold: 0,
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
                min_hold: 0,
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
    fn just_matched_reports_only_the_recognition_frame() {
        // T064: a special recognized *this* frame appears in `just_matched`;
        // on the next tick (while still active but not freshly matched) it does
        // not. This is what lets the input display flash a name once.
        let cmd = CommandDef {
            name: "qcf".into(),
            elements: compile_command("D, DF, F, x").unwrap(),
            time: 15,
            buffer_time: 8,
        };
        let mut matcher = CommandMatcher::new(vec![cmd]);
        let mut buffer = InputBuffer::new();
        // Feed D, DF, F, +x.
        buffer.push(make_state(
            Direction {
                down: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(make_state(
            Direction {
                down: true,
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
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[Button::X],
        ));
        matcher.check_commands(&buffer, true);
        assert!(matcher.command_active("qcf"));
        assert_eq!(
            matcher.just_matched(),
            ["qcf"],
            "the move is recognized on this frame"
        );

        // Next tick: still active (buffer_time not expired) but NOT freshly
        // matched, so `just_matched` is empty.
        buffer.push(make_state(
            Direction {
                right: true,
                ..Default::default()
            },
            &[],
        ));
        matcher.check_commands(&buffer, true);
        assert!(matcher.command_active("qcf"), "still buffered");
        assert!(
            matcher.just_matched().is_empty(),
            "not recognized again this frame"
        );
    }

    #[test]
    fn just_matched_empty_before_any_check() {
        let matcher = CommandMatcher::new(vec![]);
        assert!(matcher.just_matched().is_empty());
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
                min_hold: 0,
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
                    min_hold: 0,
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
                min_hold: 0,
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
                        min_hold: 0,
                    },
                    "first member of `>a+b` must carry the strict flag"
                );
                assert_eq!(
                    parts[1],
                    CommandElement::Button {
                        button: Button::B,
                        modifier: InputModifier::Press,
                        strict: false,
                        min_hold: 0,
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
                    min_hold: 0,
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
                min_hold: 0,
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
                min_hold: 0,
            }
        );
    }

    // ====================================================================
    // T049: trainingdummy special-move command compile tests
    // ====================================================================

    #[test]
    fn trainingdummy_fireball_command_compiles() {
        // The trainingdummy QCF+a fireball: `~D, DF, F, a` (4 elements).
        // Must compile without error — every token must be recognised.
        let elements = compile_command("~D, DF, F, a").unwrap();
        assert_eq!(
            elements.len(),
            4,
            "QCF fireball must parse as 4 elements, got {}",
            elements.len()
        );
        // First element: ~D (release Down).
        assert_eq!(
            elements[0],
            CommandElement::Dir {
                token: DirToken::D,
                modifier: InputModifier::Release,
                detect: false,
                strict: false,
                min_hold: 0,
            },
            "element 0 must be ~D (release Down)"
        );
        // Second: DF.
        assert_eq!(
            elements[1],
            CommandElement::Dir {
                token: DirToken::DF,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            },
            "element 1 must be DF"
        );
        // Third: F.
        assert_eq!(
            elements[2],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            },
            "element 2 must be F"
        );
        // Fourth: a button.
        assert_eq!(
            elements[3],
            CommandElement::Button {
                button: Button::A,
                modifier: InputModifier::Press,
                strict: false,
                min_hold: 0,
            },
            "element 3 must be the A button"
        );
    }

    #[test]
    fn trainingdummy_dp_command_compiles() {
        // The trainingdummy DP+a dragon-punch: `F, D, DF, a` (4 elements).
        // Must compile without error — every token must be recognised.
        let elements = compile_command("F, D, DF, a").unwrap();
        assert_eq!(
            elements.len(),
            4,
            "DP dragon-punch must parse as 4 elements, got {}",
            elements.len()
        );
        assert_eq!(
            elements[0],
            CommandElement::Dir {
                token: DirToken::F,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            },
            "element 0 must be F"
        );
        assert_eq!(
            elements[1],
            CommandElement::Dir {
                token: DirToken::D,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            },
            "element 1 must be D"
        );
        assert_eq!(
            elements[2],
            CommandElement::Dir {
                token: DirToken::DF,
                modifier: InputModifier::Press,
                detect: false,
                strict: false,
                min_hold: 0,
            },
            "element 2 must be DF"
        );
        assert_eq!(
            elements[3],
            CommandElement::Button {
                button: Button::A,
                modifier: InputModifier::Press,
                strict: false,
                min_hold: 0,
            },
            "element 3 must be the A button"
        );
    }

    // ====================================================================
    // T075: input leniency — jump buffer + command-window regression
    // ====================================================================

    /// Builds the engine `holdup` jump gate as authored by trainingdummy/KFM:
    /// hold + direction-detect up (`/$U`), instantaneous window.
    fn holdup_cmd() -> CommandDef {
        CommandDef {
            name: "holdup".into(),
            elements: compile_command("/$U").unwrap(),
            time: 1,
            buffer_time: 1,
        }
    }

    /// Push an up-held or neutral (default) frame.
    fn push_up(buffer: &mut InputBuffer, up: bool) {
        buffer.push(make_state(
            Direction {
                up,
                ..Default::default()
            },
            &[],
        ));
    }

    #[test]
    fn leniency_defaults_to_off() {
        // Default leniency must be the old behavior: no jump buffer.
        let def = LeniencyConfig::default();
        assert_eq!(def.jump_buffer_frames, 0);
        assert_eq!(def.jump_command, "holdup");
        assert_eq!(LeniencyConfig::with_jump_buffer().jump_buffer_frames, 3);
    }

    #[test]
    fn jump_buffer_fires_on_actionable_frame_after_release() {
        // Player taps up, then RELEASES it 2 frames before becoming actionable
        // (the actionable frame is the most recent / offset 0, where up is no
        // longer held). With the jump buffer on, `holdup` must still be active on
        // that frame.
        let mut matcher =
            CommandMatcher::with_leniency(vec![holdup_cmd()], LeniencyConfig::with_jump_buffer());
        let mut buffer = InputBuffer::new();
        push_up(&mut buffer, false); // neutral
        push_up(&mut buffer, true); // <- up pressed (the buffered tap)
        push_up(&mut buffer, false); // released, still in recovery
        push_up(&mut buffer, false); // actionable frame, up no longer held

        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("holdup"),
            "a jump tapped 2 frames before the actionable frame must still fire"
        );
    }

    #[test]
    fn jump_buffer_off_does_not_revive_released_jump() {
        // Same input, jump buffer DISABLED (default matcher): on the actionable
        // frame up is not held, so plain `holdup` is NOT active — proving the new
        // behavior is opt-in and the baseline is unchanged.
        let mut matcher = CommandMatcher::new(vec![holdup_cmd()]);
        let mut buffer = InputBuffer::new();
        push_up(&mut buffer, false);
        push_up(&mut buffer, true);
        push_up(&mut buffer, false);
        push_up(&mut buffer, false);

        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("holdup"),
            "without the jump buffer, a released up-press must not fire holdup"
        );
    }

    #[test]
    fn jump_buffer_respects_window_size() {
        // An up-press OLDER than the window must not be buffered.
        let mut matcher =
            CommandMatcher::with_leniency(vec![holdup_cmd()], LeniencyConfig::with_jump_buffer());
        let mut buffer = InputBuffer::new();
        push_up(&mut buffer, true); // up press, far back
        for _ in 0..5 {
            push_up(&mut buffer, false); // 5 neutral frames (> 3-frame window)
        }
        matcher.check_commands(&buffer, true);
        assert!(
            !matcher.command_active("holdup"),
            "an up-press older than the buffer window must not fire holdup"
        );
    }

    #[test]
    fn jump_buffer_does_not_invent_jump_without_press() {
        // No up at all => no buffered jump, even with the buffer enabled.
        let mut matcher =
            CommandMatcher::with_leniency(vec![holdup_cmd()], LeniencyConfig::with_jump_buffer());
        let mut buffer = InputBuffer::new();
        for _ in 0..6 {
            push_up(&mut buffer, false);
        }
        matcher.check_commands(&buffer, true);
        assert!(!matcher.command_active("holdup"));
    }

    #[test]
    fn jump_buffer_only_touches_its_own_command_not_qcf_or_dp() {
        // REGRESSION (task gotcha): the jump buffer must re-arm ONLY `holdup`.
        // It must never cause an unrelated motion to misfire — a QCF must not eat
        // a DP, and a stray up-press must not conjure either. We register holdup,
        // a QCF, and a DP; feed only a buffered up-press; and assert holdup fires
        // while QCF and DP stay silent.
        let qcf = CommandDef {
            name: "fireball".into(),
            elements: compile_command("D, DF, F, x").unwrap(),
            time: 15,
            buffer_time: 3,
        };
        let dp = CommandDef {
            name: "dp".into(),
            elements: compile_command("F, D, DF, a").unwrap(),
            time: 20,
            buffer_time: 3,
        };
        let mut matcher = CommandMatcher::with_leniency(
            vec![holdup_cmd(), qcf, dp],
            LeniencyConfig::with_jump_buffer(),
        );
        let mut buffer = InputBuffer::new();
        push_up(&mut buffer, false);
        push_up(&mut buffer, true); // the only meaningful input: an up tap
        push_up(&mut buffer, false);

        matcher.check_commands(&buffer, true);
        assert!(
            matcher.command_active("holdup"),
            "buffered up must fire jump"
        );
        assert!(
            !matcher.command_active("fireball"),
            "the jump buffer must NOT make a QCF misfire"
        );
        assert!(
            !matcher.command_active("dp"),
            "the jump buffer must NOT make a DP misfire"
        );
    }

    #[test]
    fn jump_buffer_no_false_positives_for_real_motions() {
        // Replay a genuine QCF+x with the jump buffer ON over a matcher that knows
        // QCF, DP and holdup. The buffer must not add spurious commands: exactly
        // the QCF should fire (no up was pressed, so holdup stays off; DP stays
        // off). This is the "no new false matches" acceptance check against a
        // real-shaped motion.
        let qcf = CommandDef {
            name: "fireball".into(),
            elements: compile_command("D, DF, F, x").unwrap(),
            time: 15,
            buffer_time: 3,
        };
        let dp = CommandDef {
            name: "dp".into(),
            elements: compile_command("F, D, DF, a").unwrap(),
            time: 20,
            buffer_time: 3,
        };
        let mut lenient = CommandMatcher::with_leniency(
            vec![holdup_cmd(), qcf.clone(), dp.clone()],
            LeniencyConfig::with_jump_buffer(),
        );
        let mut strict = CommandMatcher::new(vec![holdup_cmd(), qcf, dp]);

        let mut buffer = InputBuffer::new();
        for _ in 0..5 {
            buffer.push(InputState::default());
        }
        buffer.push(make_state(
            Direction {
                down: true,
                ..Default::default()
            },
            &[],
        ));
        buffer.push(make_state(
            Direction {
                down: true,
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
        buffer.push(make_state(Direction::default(), &[Button::X]));

        lenient.check_commands(&buffer, true);
        strict.check_commands(&buffer, true);

        // The lenient matcher matches exactly what the strict one does for this
        // up-free motion: the QCF and nothing else.
        assert!(lenient.command_active("fireball"));
        assert!(strict.command_active("fireball"));
        assert!(
            !lenient.command_active("holdup"),
            "no up pressed => no jump"
        );
        assert!(!lenient.command_active("dp"), "QCF must not become a DP");
        assert_eq!(
            lenient.active_command_names().len(),
            strict.active_command_names().len(),
            "jump buffer added no extra command matches for a real QCF"
        );
    }

    #[test]
    fn jump_buffer_is_deterministic() {
        // Determinism: identical input sequences yield identical active sets.
        let build = || {
            CommandMatcher::with_leniency(vec![holdup_cmd()], LeniencyConfig::with_jump_buffer())
        };
        let mut a = build();
        let mut b = build();
        let mut buf_a = InputBuffer::new();
        let mut buf_b = InputBuffer::new();
        for up in [false, true, false, false, false] {
            push_up(&mut buf_a, up);
            push_up(&mut buf_b, up);
            a.check_commands(&buf_a, true);
            b.check_commands(&buf_b, true);
            assert_eq!(a.active_command_names(), b.active_command_names());
        }
    }
}
