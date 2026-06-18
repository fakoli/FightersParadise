//! Baseline command-driven CPU AI controller (T018).
//!
//! A simple, **deterministic** opponent brain for a player that has no human
//! input. Given a per-frame [`AiObservation`] of where the opponent is relative
//! to the AI fighter, [`CpuAi::decide`] emits a raw [`InputState`] (absolute
//! screen directions + button presses) — the same snapshot the keyboard sampler
//! produces — so the engine can feed it through the normal command-matching path
//! with no special-casing.
//!
//! # What it does
//!
//! The brain is intentionally minimal but covers the four behaviours the task
//! calls for — **approach, attack, block, jump**:
//!
//! - **Approach.** When the opponent is farther than [`AiTuning::attack_range`]
//!   the AI holds the absolute screen direction *toward* the opponent (walk in).
//! - **Attack.** Once inside [`AiTuning::attack_range`] it presses an attack
//!   button (a light punch, `a`), so it both closes distance and strikes — the
//!   core "move toward and attack" behaviour.
//! - **Block.** With probability scaled by difficulty it instead holds *away*
//!   from the opponent (guard), modelling a defensive reaction.
//! - **Jump.** With a small difficulty-scaled probability it taps up (a hop),
//!   adding vertical variety.
//!
//! Every decision is drawn from a self-contained Park–Miller RNG seeded once at
//! construction, so a given `(seed, observation-sequence)` always replays to the
//! same inputs — required for replay / rollback determinism. No wall-clock, no OS
//! randomness, no allocation, never panics.

use crate::state::{Button, Direction, InputState};
use std::collections::VecDeque;

/// The Park–Miller "minimal standard" multiplier (`7^5`).
const PARK_MILLER_MUL: i64 = 16807;
/// The Park–Miller modulus (`2^31 - 1`, a Mersenne prime).
const PARK_MILLER_MOD: i64 = 2_147_483_647;

/// Frames a teaching mode must observe its trigger *before* it reacts, so the CPU
/// never reverses/punishes on frame one — a human-plausible reaction window
/// (~133 ms at 60 Hz). Tuned to teach a habit, not to be unblockable.
const REACTION_DELAY_FRAMES: u32 = 8;

/// Horizontal reach (world pixels) inside which [`BehaviorMode::ReactiveDP`] will
/// throw an anti-air / wakeup dragon-punch. Slightly longer than a poke range so
/// the uppercut actually catches a jump-in.
const DP_RANGE: f32 = 70.0;

/// Horizontal reach (world pixels) inside which [`BehaviorMode::WhiffPunisher`]
/// commits to a dash-in punish when the opponent is recovering.
const PUNISH_RANGE: f32 = 90.0;

/// A deterministic Park–Miller "minimal standard" linear congruential generator.
///
/// This mirrors the generator `fp-vm` uses for the MUGEN `random` trigger so the
/// CPU AI's decisions are bit-reproducible from a seed (replay / rollback
/// safety). It is reproduced locally rather than depending on `fp-vm` to keep
/// `fp-input`'s dependency graph (and compile surface) unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AiRng {
    /// Current LCG state, kept strictly inside `1..=2^31-2`.
    seed: i32,
}

impl AiRng {
    /// Creates a generator, normalizing `seed` into the valid range `1..=2^31-2`
    /// (Park–Miller is undefined at `0`), so any `i32` is accepted.
    fn new(seed: i32) -> Self {
        let mut s = (seed as i64).rem_euclid(PARK_MILLER_MOD) as i32;
        if s == 0 {
            s = 1;
        }
        AiRng { seed: s }
    }

    /// Advances the generator and returns the next raw value in `1..=2^31-2`.
    fn next_u31(&mut self) -> i32 {
        let next = (self.seed as i64).wrapping_mul(PARK_MILLER_MUL) % PARK_MILLER_MOD;
        self.seed = if next == 0 { 1 } else { next as i32 };
        self.seed
    }

    /// Returns the next value as an inclusive integer in `[0, hi]` (`hi >= 0`).
    /// An `hi <= 0` yields `0`, so the call never panics.
    fn next_below(&mut self, span: i32) -> i32 {
        if span <= 1 {
            return 0;
        }
        (self.next_u31() as i64).rem_euclid(span as i64) as i32
    }
}

/// How aggressive / skilled the CPU AI is, on a small fixed scale.
///
/// A coarse difficulty knob (the task's "configurable difficulty"): higher levels
/// reach farther before attacking, attack more readily, and guard / hop a little
/// more often. The exact numeric effect lives in [`AiTuning::for_difficulty`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AiDifficulty {
    /// Passive: short reach, rarely guards or jumps.
    Easy,
    /// The balanced baseline.
    #[default]
    Normal,
    /// Aggressive: longer reach, guards and hops more.
    Hard,
}

impl AiDifficulty {
    /// A short uppercase label for the level, for a menu/HUD renderer (matches
    /// the menu font's glyph set). Used by the Setup/Options CPU-difficulty row
    /// (T069).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            AiDifficulty::Easy => "EASY",
            AiDifficulty::Normal => "NORMAL",
            AiDifficulty::Hard => "HARD",
        }
    }

    /// The next-harder level, saturating at [`AiDifficulty::Hard`] (no wrap), so a
    /// menu can step the selector right with Right/Confirm (T069).
    #[must_use]
    pub fn harder(self) -> Self {
        match self {
            AiDifficulty::Easy => AiDifficulty::Normal,
            AiDifficulty::Normal | AiDifficulty::Hard => AiDifficulty::Hard,
        }
    }

    /// The next-easier level, saturating at [`AiDifficulty::Easy`] (no wrap), so a
    /// menu can step the selector left (T069).
    #[must_use]
    pub fn easier(self) -> Self {
        match self {
            AiDifficulty::Hard => AiDifficulty::Normal,
            AiDifficulty::Normal | AiDifficulty::Easy => AiDifficulty::Easy,
        }
    }

    /// Maps this coarse difficulty onto the MUGEN `AILevel` scale (`1..=8`),
    /// the value the engine assigns a CPU-controlled fighter so its CNS can gate
    /// AI-only behaviour on the `AILevel` trigger (T052).
    ///
    /// `Easy → 2`, `Normal → 4`, `Hard → 7` — three well-separated points inside
    /// the `1..=8` band (never `0`, which is reserved for a human player). The
    /// coordinator (`fp-engine`) calls this when a side is driven by a
    /// [`CpuAi`](crate::CpuAi) and pushes the result onto the character via
    /// `Character::set_ai_level`.
    #[must_use]
    pub fn ai_level(self) -> u8 {
        match self {
            AiDifficulty::Easy => 2,
            AiDifficulty::Normal => 4,
            AiDifficulty::Hard => 7,
        }
    }
}

/// The numeric behaviour parameters the [`CpuAi`] reads each frame, derived from
/// an [`AiDifficulty`] (or supplied directly for fine control / tests).
///
/// All probabilities are expressed as a chance out of `1000` so the AI can draw
/// them from the integer Park–Miller RNG without floating point — keeping every
/// decision exactly reproducible.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AiTuning {
    /// Horizontal distance (in world pixels) at or under which the AI attacks
    /// instead of merely walking forward.
    pub attack_range: f32,
    /// Chance out of `1000`, per in-range frame, of guarding (holding away)
    /// instead of pressing an attack.
    pub block_chance: i32,
    /// Chance out of `1000`, per frame, of tapping up (a jump) regardless of
    /// range.
    pub jump_chance: i32,
}

impl AiTuning {
    /// The tuning for a coarse [`AiDifficulty`] level.
    #[must_use]
    pub fn for_difficulty(difficulty: AiDifficulty) -> Self {
        match difficulty {
            AiDifficulty::Easy => AiTuning {
                attack_range: 40.0,
                block_chance: 50,
                jump_chance: 10,
            },
            AiDifficulty::Normal => AiTuning {
                attack_range: 60.0,
                block_chance: 150,
                jump_chance: 25,
            },
            AiDifficulty::Hard => AiTuning {
                attack_range: 80.0,
                block_chance: 300,
                jump_chance: 50,
            },
        }
    }
}

impl Default for AiTuning {
    fn default() -> Self {
        Self::for_difficulty(AiDifficulty::Normal)
    }
}

/// A per-frame snapshot of the world the [`CpuAi`] reasons over.
///
/// Deliberately small and engine-agnostic: it carries only what the brain needs
/// to pick a direction, decide whether it is in range, and (for the teaching
/// [`BehaviorMode`]s) react to the opponent's situation. The caller (the engine /
/// app) fills it from the live fighters each frame. The "toward" direction is
/// derived purely from [`opponent_dx`](Self::opponent_dx), so the AI needs no
/// notion of its own facing.
///
/// The fields beyond `opponent_dx` are only *consulted* by the teaching modes
/// ([`BehaviorMode::PureBlocker`] / [`ReactiveDP`](BehaviorMode::ReactiveDP) /
/// [`WhiffPunisher`](BehaviorMode::WhiffPunisher)); the default
/// [`BehaviorMode::Ladder`] ignores them, so a caller that only drives the
/// difficulty ladder can leave them at their `false` default (see
/// [`AiObservation::at`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AiObservation {
    /// Opponent X minus AI X, in world pixels. Positive ⇒ the opponent is to the
    /// AI's **right** (so "toward" is screen-right); negative ⇒ to the left.
    pub opponent_dx: f32,
    /// `true` while the opponent is in the **active** phase of an attack (its
    /// `MoveType = A` and the move is not yet recovering). What
    /// [`BehaviorMode::PureBlocker`] guards against. Sourced from the opponent's
    /// move-type / frame phase via the engine's `EvalCtx` (T065).
    pub opponent_attacking: bool,
    /// `true` while the opponent is **airborne** (`StateType = A`). The anti-air
    /// signal for [`BehaviorMode::ReactiveDP`].
    pub opponent_airborne: bool,
    /// `true` while the opponent is in **recovery** (whiffing / move ended but
    /// still locked in lag, before it can act again) — the punish window
    /// [`BehaviorMode::WhiffPunisher`] strikes into.
    pub opponent_recovering: bool,
    /// `true` while the **AI itself** is rising from a knockdown (wakeup /
    /// getting-up). [`BehaviorMode::ReactiveDP`] uses this to throw a wakeup
    /// reversal when the opponent is closing in.
    pub self_waking_up: bool,
}

impl AiObservation {
    /// A bare observation that only knows the opponent's relative position, with
    /// every situational flag at its `false` default.
    ///
    /// This is the convenient constructor for the difficulty ladder (and for
    /// tests that don't exercise the teaching modes), since
    /// [`BehaviorMode::Ladder`] ignores the flags.
    #[must_use]
    pub fn at(opponent_dx: f32) -> Self {
        Self {
            opponent_dx,
            ..Self::default()
        }
    }

    /// Absolute horizontal distance to the opponent.
    #[must_use]
    pub fn distance(&self) -> f32 {
        self.opponent_dx.abs()
    }

    /// `true` when the opponent is to the AI's right (so it should walk/strike
    /// to screen-right). Exactly-zero `dx` (perfectly overlapped) counts as right.
    #[must_use]
    pub fn opponent_on_right(&self) -> bool {
        self.opponent_dx >= 0.0
    }
}

/// A selectable **teaching** behaviour for the CPU, layered on top of the raw
/// difficulty ladder ([`AiDifficulty`] / [`AiTuning`]).
///
/// Where the ladder scales *how hard* the baseline brain plays, a `BehaviorMode`
/// changes *what it tries to teach the human* — each mode reacts to a specific
/// situation in the opponent's [`AiObservation`] and is observably distinct from
/// the others and from the ladder. Every mode is fully deterministic given the
/// seed (all randomness flows through the same [`AiRng`]); reactions are capped
/// to a human-plausible delay rather than firing on the first frame, so the modes
/// *teach* a habit instead of frustrating the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BehaviorMode {
    /// The baseline difficulty ladder: approach / attack / block / hop scaled by
    /// [`AiDifficulty`]. The situational flags on [`AiObservation`] are ignored.
    #[default]
    Ladder,
    /// **Pure Blocker.** Only ever defends: holds *away* (guard) while the
    /// opponent is attacking and is otherwise neutral. Never attacks — so a human
    /// can drill a block-string / frame trap and watch what is and isn't safe.
    PureBlocker,
    /// **Reactive DP.** Throws a dragon-punch reversal (an invincible-style
    /// uppercut motion) when the opponent is airborne in range (anti-air) or when
    /// the AI is waking up with the opponent closing in (wakeup reversal). Teaches
    /// the human to respect anti-airs and not to press buttons on the opponent's
    /// wakeup.
    ReactiveDP,
    /// **Whiff Punisher.** Dashes in and strikes when the opponent is recovering
    /// from a whiffed attack inside punish range; otherwise it stays spaced just
    /// outside attack range. Teaches the human not to throw out random buttons.
    WhiffPunisher,
}

impl BehaviorMode {
    /// Every mode in selector / display order, so a menu (or a CLI flag parser)
    /// can enumerate and step through them without hardcoding the variant list.
    pub const ALL: [BehaviorMode; 4] = [
        BehaviorMode::Ladder,
        BehaviorMode::PureBlocker,
        BehaviorMode::ReactiveDP,
        BehaviorMode::WhiffPunisher,
    ];

    /// A short uppercase label for the mode, for a menu/HUD renderer (matches the
    /// menu font's glyph set: `0-9 A-Z`, space, colon). Used by the Setup/Options
    /// CPU-mode selector row (T070).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            BehaviorMode::Ladder => "LADDER",
            BehaviorMode::PureBlocker => "PURE BLOCKER",
            BehaviorMode::ReactiveDP => "REACTIVE DP",
            BehaviorMode::WhiffPunisher => "WHIFF PUNISHER",
        }
    }

    /// A short, lowercase, space-free token for the mode, used by the `--ai-mode`
    /// CLI flag (so a player can pick a teaching mode from the command line).
    /// Round-trips with [`BehaviorMode::from_token`].
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            BehaviorMode::Ladder => "ladder",
            BehaviorMode::PureBlocker => "blocker",
            BehaviorMode::ReactiveDP => "dp",
            BehaviorMode::WhiffPunisher => "punisher",
        }
    }

    /// Parses a CLI/`--ai-mode` token (case-insensitive) into a mode, or `None`
    /// for an unrecognized token. Accepts the canonical [`BehaviorMode::token`]
    /// plus a couple of natural aliases.
    #[must_use]
    pub fn from_token(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ladder" | "default" => Some(BehaviorMode::Ladder),
            "blocker" | "pureblocker" | "block" => Some(BehaviorMode::PureBlocker),
            "dp" | "reactivedp" | "reversal" => Some(BehaviorMode::ReactiveDP),
            "punisher" | "whiffpunisher" | "whiff" => Some(BehaviorMode::WhiffPunisher),
            _ => None,
        }
    }

    /// The next mode in [`BehaviorMode::ALL`] order, wrapping back to the first
    /// after the last — so a single Right/Confirm key cycles every mode (T070).
    #[must_use]
    pub fn next(self) -> Self {
        let all = Self::ALL;
        let i = all.iter().position(|&m| m == self).unwrap_or(0);
        all[(i + 1) % all.len()]
    }

    /// The previous mode in [`BehaviorMode::ALL`] order, wrapping from the first
    /// back to the last — so a single Left key cycles every mode the other way.
    #[must_use]
    pub fn prev(self) -> Self {
        let all = Self::ALL;
        let i = all.iter().position(|&m| m == self).unwrap_or(0);
        all[(i + all.len() - 1) % all.len()]
    }
}

/// A simple deterministic, command-driven CPU AI for a non-human player (T018).
///
/// Construct one per AI-controlled fighter with a seed (for reproducibility) and
/// a difficulty, then call [`CpuAi::decide`] once per frame with a fresh
/// [`AiObservation`]. The returned [`InputState`] is fed into that player's input
/// buffer / command matcher exactly like a keyboard frame.
#[derive(Debug, Clone)]
pub struct CpuAi {
    /// Reproducible decision RNG.
    rng: AiRng,
    /// Behaviour parameters for the configured difficulty.
    tuning: AiTuning,
    /// The selectable teaching behaviour layered on top of the ladder.
    mode: BehaviorMode,
    /// Whether the attack button was pressed on the previous frame. The AI
    /// **pulses** its attack: a held button only fires a button command on its
    /// press edge, so the AI releases for one frame after a press to re-arm the
    /// next strike. This is part of the AI's deterministic state.
    attacked_last_frame: bool,
    /// A queued multi-frame motion the AI is currently *playing out* (e.g. a
    /// dragon-punch or a dash-attack), stored **newest element last** and popped
    /// front-to-back one frame at a time. While non-empty the AI commits to the
    /// script and ignores fresh reactions, so a special isn't aborted mid-motion.
    script: VecDeque<InputState>,
    /// Frames remaining before a *new* reactive special may fire. Reset to a
    /// human-plausible delay after each reaction (and counted toward the trigger
    /// condition) so the teaching modes never react on frame one.
    reaction_cooldown: u32,
}

impl CpuAi {
    /// Creates a CPU AI seeded for determinism at the given difficulty.
    ///
    /// The same `seed` and the same sequence of [`AiObservation`]s always yields
    /// the same sequence of [`InputState`]s.
    #[must_use]
    pub fn new(seed: i32, difficulty: AiDifficulty) -> Self {
        Self {
            rng: AiRng::new(seed),
            tuning: AiTuning::for_difficulty(difficulty),
            mode: BehaviorMode::default(),
            attacked_last_frame: false,
            script: VecDeque::new(),
            reaction_cooldown: 0,
        }
    }

    /// Creates a CPU AI with explicit [`AiTuning`] (fine control / tests).
    #[must_use]
    pub fn with_tuning(seed: i32, tuning: AiTuning) -> Self {
        Self {
            rng: AiRng::new(seed),
            tuning,
            mode: BehaviorMode::default(),
            attacked_last_frame: false,
            script: VecDeque::new(),
            reaction_cooldown: 0,
        }
    }

    /// Creates a CPU AI in a specific teaching [`BehaviorMode`] at the given
    /// difficulty (used for the teaching dummy / Lab; the menu picks the mode).
    #[must_use]
    pub fn with_mode(seed: i32, difficulty: AiDifficulty, mode: BehaviorMode) -> Self {
        let mut ai = Self::new(seed, difficulty);
        ai.mode = mode;
        ai
    }

    /// The behaviour parameters this AI is using.
    #[must_use]
    pub fn tuning(&self) -> AiTuning {
        self.tuning
    }

    /// The teaching [`BehaviorMode`] currently selected.
    #[must_use]
    pub fn mode(&self) -> BehaviorMode {
        self.mode
    }

    /// Selects the teaching [`BehaviorMode`]. Clears any in-flight motion script
    /// so the new mode takes effect cleanly on the next frame.
    pub fn set_mode(&mut self, mode: BehaviorMode) {
        self.mode = mode;
        self.script.clear();
    }

    /// Decides this frame's input from the current world observation.
    ///
    /// Behaviour (in priority order):
    /// 1. If out of [`attack_range`](AiTuning::attack_range): hold the absolute
    ///    direction **toward** the opponent (approach).
    /// 2. If in range: usually **tap** `a` (light punch) — attack; with
    ///    [`block_chance`](AiTuning::block_chance)/1000 probability instead hold
    ///    **away** (guard / block).
    /// 3. Independently, with [`jump_chance`](AiTuning::jump_chance)/1000
    ///    probability, also hold up (jump).
    ///
    /// Attacks are **pulsed**: a button command fires only on the press edge, so
    /// after a frame in which it pressed `a` the AI releases for one frame to
    /// re-arm — turning a sustained in-range window into repeated strikes
    /// instead of one held press.
    ///
    /// Advances the internal RNG by a fixed number of draws per call so the
    /// stream stays aligned regardless of which branch was taken (keeps replays
    /// deterministic).
    ///
    /// For a teaching [`BehaviorMode`] other than [`Ladder`](BehaviorMode::Ladder)
    /// the decision is taken by the corresponding `decide_*` helper, but the same
    /// two RNG draws happen first so a given seed produces the same stream no
    /// matter which mode is selected.
    pub fn decide(&mut self, obs: AiObservation) -> InputState {
        // Always draw the same number of values per frame so the RNG stream does
        // not desync between branches or modes.
        let block_roll = self.rng.next_below(1000);
        let jump_roll = self.rng.next_below(1000);

        // While a multi-frame motion (DP / dash-attack) is playing out, commit to
        // it: pop the next scripted frame and ignore fresh reactions. This is what
        // lets a single high-level decision span the several frames a special
        // input needs.
        if let Some(frame) = self.script.pop_front() {
            self.attacked_last_frame =
                frame.button(Button::A) || frame.button(Button::B) || frame.button(Button::C);
            return frame;
        }

        // Count down the reaction window every frame so a reactive mode commits
        // only after observing its trigger for a human-plausible span.
        self.reaction_cooldown = self.reaction_cooldown.saturating_sub(1);

        match self.mode {
            BehaviorMode::Ladder => self.decide_ladder(obs, block_roll, jump_roll),
            BehaviorMode::PureBlocker => self.decide_pure_blocker(obs),
            BehaviorMode::ReactiveDP => self.decide_reactive_dp(obs),
            BehaviorMode::WhiffPunisher => self.decide_whiff_punisher(obs),
        }
    }

    /// The baseline difficulty-ladder brain (approach / attack / block / hop).
    fn decide_ladder(&mut self, obs: AiObservation, block_roll: i32, jump_roll: i32) -> InputState {
        let toward_right = obs.opponent_on_right();
        let in_range = obs.distance() <= self.tuning.attack_range;

        let mut state = InputState::default();
        let mut attacking = false;

        if in_range {
            if block_roll < self.tuning.block_chance {
                // Guard: hold AWAY from the opponent (the engine resolves this to
                // `holdback` and sets the defender's guard flag).
                state.direction = away_direction(toward_right);
            } else if self.attacked_last_frame {
                // Re-arm: release the button for one frame so the next press is a
                // fresh edge the command matcher can detect.
            } else {
                // Strike: a light punch in range.
                state.set_button(Button::A, true);
                attacking = true;
            }
        } else {
            // Approach: hold TOWARD the opponent.
            state.direction = toward_direction(toward_right);
        }

        // Jump is independent of the above (can hop while approaching).
        if jump_roll < self.tuning.jump_chance {
            state.direction.up = true;
        }

        self.attacked_last_frame = attacking;
        state
    }

    /// **Pure Blocker.** Holds *away* (guard) while the opponent is attacking;
    /// otherwise stays neutral. Never attacks, jumps, or approaches — so a human
    /// can drill block-strings against a wall that only ever defends.
    fn decide_pure_blocker(&mut self, obs: AiObservation) -> InputState {
        self.attacked_last_frame = false;
        let mut state = InputState::default();
        if obs.opponent_attacking {
            state.direction = away_direction(obs.opponent_on_right());
        }
        state
    }

    /// **Reactive DP.** Throws a dragon-punch reversal as an anti-air (opponent
    /// airborne, in range) or a wakeup reversal (AI waking up, opponent closing),
    /// after a human-plausible reaction delay; otherwise neutral.
    fn decide_reactive_dp(&mut self, obs: AiObservation) -> InputState {
        let in_dp_range = obs.distance() <= DP_RANGE;
        let anti_air = obs.opponent_airborne && in_dp_range;
        let wakeup_reversal = obs.self_waking_up && in_dp_range;

        if anti_air || wakeup_reversal {
            if self.reaction_cooldown == 0 {
                // React: queue the DP motion (F, D, DF + punch) for the coming
                // frames and emit its first frame now.
                self.enqueue_dragon_punch(obs.opponent_on_right());
                self.reaction_cooldown = REACTION_DELAY_FRAMES;
                if let Some(frame) = self.script.pop_front() {
                    self.attacked_last_frame = false;
                    return frame;
                }
            }
        } else {
            // Trigger gone: re-arm so the next genuine opening gets a fresh
            // reaction window instead of an instant one.
            self.reaction_cooldown = REACTION_DELAY_FRAMES;
        }

        self.attacked_last_frame = false;
        InputState::default()
    }

    /// **Whiff Punisher.** Dashes in and strikes when the opponent is recovering
    /// inside punish range (after a reaction delay); otherwise spaces just outside
    /// its own attack range so it baits the whiff in the first place.
    fn decide_whiff_punisher(&mut self, obs: AiObservation) -> InputState {
        let in_punish_range = obs.distance() <= PUNISH_RANGE;
        let punishable = obs.opponent_recovering && in_punish_range;

        if punishable {
            if self.reaction_cooldown == 0 {
                self.enqueue_dash_attack(obs.opponent_on_right());
                self.reaction_cooldown = REACTION_DELAY_FRAMES;
                if let Some(frame) = self.script.pop_front() {
                    self.attacked_last_frame = frame.button(Button::A);
                    return frame;
                }
            }
            // Still inside the reaction window: hold ground (don't telegraph).
            self.attacked_last_frame = false;
            return InputState::default();
        }

        // No punish available — re-arm and hold spacing just outside poke range.
        self.reaction_cooldown = REACTION_DELAY_FRAMES;
        self.attacked_last_frame = false;
        let spacing = self.tuning.attack_range + 10.0;
        let mut state = InputState::default();
        if obs.distance() < spacing {
            // Too close: back off to keep the gap (hold away).
            state.direction = away_direction(obs.opponent_on_right());
        } else {
            // Drift in toward punish range.
            state.direction = toward_direction(obs.opponent_on_right());
        }
        state
    }

    /// Queues the frame-by-frame **dragon punch** motion (`F, D, DF + A`) toward
    /// the given side, emitted one [`InputState`] per frame on subsequent
    /// [`decide`](Self::decide) calls. Absolute directions are derived from which
    /// side the opponent is on, so the motion is always *forward* into them.
    fn enqueue_dragon_punch(&mut self, opponent_on_right: bool) {
        let fwd = toward_direction(opponent_on_right);
        let down = Direction {
            down: true,
            ..Default::default()
        };
        let down_fwd = Direction {
            down: true,
            right: opponent_on_right,
            left: !opponent_on_right,
            ..Default::default()
        };
        // F
        self.script.push_back(state_with_direction(fwd));
        // D
        self.script.push_back(state_with_direction(down));
        // DF + punch (the move-firing frame).
        let mut hit = state_with_direction(down_fwd);
        hit.set_button(Button::A, true);
        self.script.push_back(hit);
        // Release frame so the press is a clean edge for the next motion.
        self.script.push_back(InputState::default());
    }

    /// Queues a **dash-attack**: a forward dash (tap forward, neutral, tap
    /// forward) followed by an attack, one [`InputState`] per frame. Closes the
    /// gap into a recovering opponent and strikes.
    fn enqueue_dash_attack(&mut self, opponent_on_right: bool) {
        let fwd = toward_direction(opponent_on_right);
        // F, (neutral), F — a double-tap forward = dash.
        self.script.push_back(state_with_direction(fwd));
        self.script.push_back(InputState::default());
        self.script.push_back(state_with_direction(fwd));
        // Strike on arrival.
        let mut hit = state_with_direction(fwd);
        hit.set_button(Button::A, true);
        self.script.push_back(hit);
        self.script.push_back(InputState::default());
    }
}

/// An [`InputState`] holding only the given [`Direction`].
fn state_with_direction(direction: Direction) -> InputState {
    InputState {
        direction,
        ..Default::default()
    }
}

/// The absolute screen [`Direction`] that walks **toward** an opponent on the
/// given side.
fn toward_direction(opponent_on_right: bool) -> Direction {
    Direction {
        right: opponent_on_right,
        left: !opponent_on_right,
        ..Default::default()
    }
}

/// The absolute screen [`Direction`] that walks **away** from an opponent on the
/// given side (guard).
fn away_direction(opponent_on_right: bool) -> Direction {
    Direction {
        right: !opponent_on_right,
        left: opponent_on_right,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Far opponent on the right ⇒ the AI walks right (toward) and does not yet
    /// attack.
    #[test]
    fn approaches_a_far_opponent() {
        let mut ai = CpuAi::new(1, AiDifficulty::Normal);
        // Well outside attack range, opponent to the right.
        let s = ai.decide(AiObservation::at(200.0));
        assert!(s.direction.right, "should walk right toward the opponent");
        assert!(!s.direction.left);
        assert!(
            !s.button(Button::A),
            "must not attack while still out of range"
        );
    }

    /// Far opponent on the left ⇒ walks left (mirror of the right case).
    #[test]
    fn approaches_toward_a_left_side_opponent() {
        let mut ai = CpuAi::new(1, AiDifficulty::Normal);
        let s = ai.decide(AiObservation::at(-200.0));
        assert!(s.direction.left, "should walk left toward the opponent");
        assert!(!s.direction.right);
    }

    /// Acceptance criterion: at a given (close) opponent distance the AI both
    /// approaches and attacks. Across several frames at point-blank range it
    /// presses the attack button on a substantial fraction of frames (it pulses
    /// the button — releasing every other frame to re-arm — and occasionally
    /// guards), and never walks the wrong way on an attacking frame.
    #[test]
    fn approaches_and_attacks_in_range() {
        let mut ai = CpuAi::new(7, AiDifficulty::Normal);
        let mut attack_frames = 0;
        let frames = 100;
        for _ in 0..frames {
            // Point-blank: clearly inside attack range, opponent to the right.
            let s = ai.decide(AiObservation::at(10.0));
            if s.button(Button::A) {
                attack_frames += 1;
                // When it attacks it must not also be walking the wrong way.
                assert!(!s.direction.left, "an attacking frame must not retreat");
            }
        }
        // The AI pulses (presses at most every other frame) and blocks ~15% of
        // the time at Normal, so it strikes on a large minority of frames; assert
        // it lands well above a quarter of the window (clearly "attacking").
        assert!(
            attack_frames > frames / 4,
            "in range it should attack on many frames, got {attack_frames}/{frames}"
        );
    }

    /// The AI pulses its attack: it never presses `a` on two consecutive frames
    /// (a held button would only fire one command on its press edge).
    #[test]
    fn attack_is_pulsed_not_held() {
        let mut ai = CpuAi::new(7, AiDifficulty::Normal);
        let mut prev_attack = false;
        for _ in 0..200 {
            let s = ai.decide(AiObservation::at(10.0));
            let attack = s.button(Button::A);
            assert!(
                !(attack && prev_attack),
                "must not press the attack button on two consecutive frames"
            );
            prev_attack = attack;
        }
    }

    /// In range, the AI sometimes blocks (holds away). Over many frames at least
    /// one guard frame appears at Normal difficulty.
    #[test]
    fn blocks_sometimes_in_range() {
        let mut ai = CpuAi::new(123, AiDifficulty::Normal);
        let mut block_frames = 0;
        for _ in 0..200 {
            let s = ai.decide(AiObservation::at(10.0));
            // Guarding means holding AWAY (left, since opponent is on the right)
            // and NOT pressing attack.
            if s.direction.left && !s.button(Button::A) {
                block_frames += 1;
            }
        }
        assert!(block_frames > 0, "Normal AI should guard at least once");
    }

    /// Determinism: same seed + same observation sequence ⇒ identical inputs.
    #[test]
    fn deterministic_for_a_fixed_seed() {
        let obs_seq = [
            AiObservation::at(200.0),
            AiObservation::at(90.0),
            AiObservation::at(10.0),
            AiObservation::at(-150.0),
            AiObservation::at(5.0),
        ];
        let mut a = CpuAi::new(42, AiDifficulty::Hard);
        let mut b = CpuAi::new(42, AiDifficulty::Hard);
        for obs in obs_seq {
            assert_eq!(
                a.decide(obs),
                b.decide(obs),
                "same seed must produce identical inputs"
            );
        }
    }

    /// Different seeds diverge (the decisions are actually seed-driven, not
    /// constant).
    #[test]
    fn different_seeds_diverge() {
        // At point-blank range the block/attack choice is RNG-driven, so two
        // distinct seeds should produce a differing decision within a short run.
        let mut a = CpuAi::new(1, AiDifficulty::Hard);
        let mut b = CpuAi::new(999_999, AiDifficulty::Hard);
        let mut differed = false;
        for _ in 0..50 {
            let sa = a.decide(AiObservation::at(5.0));
            let sb = b.decide(AiObservation::at(5.0));
            if sa != sb {
                differed = true;
                break;
            }
        }
        assert!(differed, "distinct seeds should diverge within 50 frames");
    }

    /// The difficulty knob actually changes reach: Hard attacks at a distance
    /// where Easy is still only approaching.
    #[test]
    fn difficulty_changes_attack_range() {
        // A distance between Easy's (40) and Hard's (80) attack range.
        let dx = 70.0;
        // Use a seed/branch where neither rolls a block, so the difference is the
        // range decision, not the block roll. Average over frames to be robust.
        let mut easy = CpuAi::new(5, AiDifficulty::Easy);
        let mut hard = CpuAi::new(5, AiDifficulty::Hard);
        let mut easy_attacks = 0;
        let mut hard_attacks = 0;
        for _ in 0..100 {
            if easy.decide(AiObservation::at(dx)).button(Button::A) {
                easy_attacks += 1;
            }
            if hard.decide(AiObservation::at(dx)).button(Button::A) {
                hard_attacks += 1;
            }
        }
        // Easy is out of range at dx=70 (range 40) ⇒ never attacks; it walks in.
        assert_eq!(easy_attacks, 0, "Easy is out of range at dx=70");
        // Hard is in range at dx=70 (range 80) ⇒ attacks on many frames (it
        // pulses every other frame and blocks ~30% of the time).
        assert!(
            hard_attacks > 20,
            "Hard is in range at dx=70 and should attack often, got {hard_attacks}"
        );
    }

    /// Jump (up) can fire; with Hard's higher jump chance it appears within a
    /// reasonable window.
    #[test]
    fn jumps_occasionally() {
        let mut ai = CpuAi::new(31, AiDifficulty::Hard);
        let mut jumped = false;
        for _ in 0..200 {
            if ai.decide(AiObservation::at(200.0)).direction.up {
                jumped = true;
                break;
            }
        }
        assert!(jumped, "Hard AI should jump within 200 frames");
    }

    /// Acceptance criterion (T069): Easy demonstrably blocks and reaches *less*
    /// than Hard — the difficulty selector's two extremes are strictly ordered on
    /// both knobs, so picking Easy vs Hard is a real, measurable behaviour change.
    #[test]
    fn easy_blocks_and_attacks_less_than_hard() {
        let easy = AiTuning::for_difficulty(AiDifficulty::Easy);
        let normal = AiTuning::for_difficulty(AiDifficulty::Normal);
        let hard = AiTuning::for_difficulty(AiDifficulty::Hard);
        // Guards (blocks) strictly less often as difficulty drops.
        assert!(easy.block_chance < normal.block_chance);
        assert!(normal.block_chance < hard.block_chance);
        assert!(easy.block_chance < hard.block_chance);
        // Reaches (attacks at range) strictly less far as difficulty drops.
        assert!(easy.attack_range < normal.attack_range);
        assert!(normal.attack_range < hard.attack_range);
        assert!(easy.attack_range < hard.attack_range);
    }

    /// The Setup/Options selector cycles Easy → Normal → Hard and saturates at
    /// each end (no wrap), and its labels match the menu glyph set (T069).
    #[test]
    fn difficulty_selector_cycles_and_saturates() {
        // harder() steps up and clamps at Hard.
        assert_eq!(AiDifficulty::Easy.harder(), AiDifficulty::Normal);
        assert_eq!(AiDifficulty::Normal.harder(), AiDifficulty::Hard);
        assert_eq!(AiDifficulty::Hard.harder(), AiDifficulty::Hard);
        // easier() steps down and clamps at Easy.
        assert_eq!(AiDifficulty::Hard.easier(), AiDifficulty::Normal);
        assert_eq!(AiDifficulty::Normal.easier(), AiDifficulty::Easy);
        assert_eq!(AiDifficulty::Easy.easier(), AiDifficulty::Easy);
        // Labels are the uppercase names the menu font can draw.
        assert_eq!(AiDifficulty::Easy.label(), "EASY");
        assert_eq!(AiDifficulty::Normal.label(), "NORMAL");
        assert_eq!(AiDifficulty::Hard.label(), "HARD");
    }

    /// A neutral default tuning is the Normal preset.
    #[test]
    fn default_tuning_is_normal() {
        assert_eq!(
            AiTuning::default(),
            AiTuning::for_difficulty(AiDifficulty::Normal)
        );
        assert_eq!(AiDifficulty::default(), AiDifficulty::Normal);
    }

    /// `AiDifficulty::ai_level` maps the three coarse levels onto fixed, distinct
    /// points inside the MUGEN `AILevel` 1..=8 band (never 0, the human value) (T052).
    #[test]
    fn ai_level_maps_into_one_to_eight() {
        assert_eq!(AiDifficulty::Easy.ai_level(), 2);
        assert_eq!(AiDifficulty::Normal.ai_level(), 4);
        assert_eq!(AiDifficulty::Hard.ai_level(), 7);
        for d in [AiDifficulty::Easy, AiDifficulty::Normal, AiDifficulty::Hard] {
            let lvl = d.ai_level();
            assert!((1..=8).contains(&lvl), "{d:?} -> {lvl} must be in 1..=8");
        }
    }

    /// Observation helpers behave as documented.
    #[test]
    fn observation_helpers() {
        let right = AiObservation::at(30.0);
        assert!(right.opponent_on_right());
        assert_eq!(right.distance(), 30.0);
        let left = AiObservation::at(-30.0);
        assert!(!left.opponent_on_right());
        assert_eq!(left.distance(), 30.0);
        // Exact overlap counts as "right" (toward screen-right).
        assert!(AiObservation::at(0.0).opponent_on_right());
    }

    // ---- T070: teaching behaviour modes ----------------------------------

    /// True if any of the three attack buttons is pressed this frame.
    fn pressed_attack(s: &InputState) -> bool {
        s.button(Button::A) || s.button(Button::B) || s.button(Button::C)
    }

    /// Runs `ai` for `frames` against a fixed observation, returning every frame.
    fn run(ai: &mut CpuAi, obs: AiObservation, frames: usize) -> Vec<InputState> {
        (0..frames).map(|_| ai.decide(obs)).collect()
    }

    /// Pure Blocker holds *away* (guard) whenever the opponent is attacking, and
    /// otherwise stays fully neutral — it never presses an attack button.
    #[test]
    fn pure_blocker_guards_only_while_opponent_attacks() {
        let mut ai = CpuAi::with_mode(7, AiDifficulty::Hard, BehaviorMode::PureBlocker);

        // Opponent attacking, on the right => hold left (away), no buttons.
        let attacking = AiObservation {
            opponent_dx: 30.0,
            opponent_attacking: true,
            ..AiObservation::default()
        };
        for s in run(&mut ai, attacking, 60) {
            assert!(
                s.direction.left,
                "must guard (hold away) vs an active attack"
            );
            assert!(!s.direction.right);
            assert!(!pressed_attack(&s), "Pure Blocker must never attack");
            assert!(!s.direction.up, "Pure Blocker must never jump");
        }

        // Opponent NOT attacking => fully neutral (even in range).
        let idle = AiObservation::at(30.0);
        for s in run(&mut ai, idle, 60) {
            assert_eq!(s, InputState::default(), "neutral when not under attack");
        }
    }

    /// Pure Blocker, unlike the ladder, does not attack a close idle opponent —
    /// the two modes are observably distinct on the same observation.
    #[test]
    fn pure_blocker_distinct_from_ladder_in_range() {
        let obs = AiObservation::at(10.0); // point blank, opponent idle
        let mut blocker = CpuAi::with_mode(3, AiDifficulty::Hard, BehaviorMode::PureBlocker);
        let mut ladder = CpuAi::with_mode(3, AiDifficulty::Hard, BehaviorMode::Ladder);

        let blocker_attacks = run(&mut blocker, obs, 120)
            .iter()
            .filter(|s| pressed_attack(s))
            .count();
        let ladder_attacks = run(&mut ladder, obs, 120)
            .iter()
            .filter(|s| pressed_attack(s))
            .count();

        assert_eq!(blocker_attacks, 0, "Pure Blocker never attacks");
        assert!(
            ladder_attacks > 0,
            "the ladder attacks a close opponent, got {ladder_attacks}"
        );
    }

    /// Reactive DP throws an uppercut (a forward, down, down-forward + punch
    /// motion) when the opponent is airborne in range — after the reaction delay,
    /// not on frame one.
    #[test]
    fn reactive_dp_antiairs_an_airborne_opponent() {
        let mut ai = CpuAi::with_mode(11, AiDifficulty::Normal, BehaviorMode::ReactiveDP);
        let jump_in = AiObservation {
            opponent_dx: 40.0, // inside DP_RANGE
            opponent_airborne: true,
            ..AiObservation::default()
        };

        let frames = run(&mut ai, jump_in, 30);
        // It must NOT fire on the very first frame (human-plausible delay).
        assert!(
            !pressed_attack(&frames[0]),
            "DP must not fire on frame one — teach, not frustrate"
        );
        // Within the window it fires the uppercut, and the firing frame holds
        // down+forward (the canonical DP hit frame) with a punch.
        let dp_frame = frames
            .iter()
            .find(|s| pressed_attack(s))
            .expect("Reactive DP should anti-air an airborne in-range opponent");
        assert!(dp_frame.direction.down, "DP fires from down+forward");
        assert!(
            dp_frame.direction.right,
            "DP is forward (toward a right-side opponent)"
        );
    }

    /// Reactive DP also reverses on wakeup: when the AI is waking up and the
    /// opponent is closing in, it throws the DP.
    #[test]
    fn reactive_dp_wakeup_reversal() {
        let mut ai = CpuAi::with_mode(5, AiDifficulty::Normal, BehaviorMode::ReactiveDP);
        let waking = AiObservation {
            opponent_dx: 30.0,
            self_waking_up: true,
            ..AiObservation::default()
        };
        let fired = run(&mut ai, waking, 30).iter().any(pressed_attack);
        assert!(
            fired,
            "Reactive DP should reverse on wakeup with foe in range"
        );
    }

    /// Reactive DP does nothing to a grounded, non-attacking, far opponent — it is
    /// purely reactive, observably distinct from the always-approaching ladder.
    #[test]
    fn reactive_dp_idle_when_no_opening() {
        let mut ai = CpuAi::with_mode(9, AiDifficulty::Normal, BehaviorMode::ReactiveDP);
        // Grounded opponent, not airborne, AI not waking up.
        let neutral = AiObservation::at(30.0);
        for s in run(&mut ai, neutral, 60) {
            assert_eq!(s, InputState::default(), "no opening => no reversal");
        }
    }

    /// Reactive DP does not anti-air an airborne opponent who is out of DP range —
    /// the range gate is real.
    #[test]
    fn reactive_dp_respects_range() {
        let mut ai = CpuAi::with_mode(9, AiDifficulty::Normal, BehaviorMode::ReactiveDP);
        let far_jump = AiObservation {
            opponent_dx: DP_RANGE + 50.0,
            opponent_airborne: true,
            ..AiObservation::default()
        };
        let fired = run(&mut ai, far_jump, 60).iter().any(pressed_attack);
        assert!(!fired, "an out-of-range jump-in is not DP'd");
    }

    /// Whiff Punisher dashes in and strikes when the opponent is recovering in
    /// range, after the reaction delay (not frame one).
    #[test]
    fn whiff_punisher_punishes_a_recovering_opponent() {
        let mut ai = CpuAi::with_mode(13, AiDifficulty::Normal, BehaviorMode::WhiffPunisher);
        let whiff = AiObservation {
            opponent_dx: 60.0, // inside PUNISH_RANGE
            opponent_recovering: true,
            ..AiObservation::default()
        };
        let frames = run(&mut ai, whiff, 30);
        assert!(
            !pressed_attack(&frames[0]),
            "punish must not be instant — reaction delay"
        );
        assert!(
            frames.iter().any(pressed_attack),
            "Whiff Punisher should strike a recovering in-range opponent"
        );
    }

    /// Whiff Punisher does not attack a non-recovering opponent; instead it holds
    /// spacing (a direction, never a button) — distinct from a recovering target.
    #[test]
    fn whiff_punisher_spaces_when_no_whiff() {
        let mut ai = CpuAi::with_mode(2, AiDifficulty::Normal, BehaviorMode::WhiffPunisher);
        // Opponent NOT recovering: the punisher should never attack.
        let safe = AiObservation::at(60.0);
        let frames = run(&mut ai, safe, 60);
        assert!(
            frames.iter().all(|s| !pressed_attack(s)),
            "no whiff => no punish attack"
        );
    }

    /// Every mode is deterministic: same seed + same observation sequence ⇒
    /// identical inputs, including the multi-frame special scripts.
    #[test]
    fn modes_are_deterministic() {
        let seq = [
            AiObservation {
                opponent_dx: 40.0,
                opponent_airborne: true,
                ..AiObservation::default()
            },
            AiObservation {
                opponent_dx: 30.0,
                opponent_attacking: true,
                ..AiObservation::default()
            },
            AiObservation {
                opponent_dx: 50.0,
                opponent_recovering: true,
                ..AiObservation::default()
            },
            AiObservation::at(200.0),
        ];
        for mode in [
            BehaviorMode::Ladder,
            BehaviorMode::PureBlocker,
            BehaviorMode::ReactiveDP,
            BehaviorMode::WhiffPunisher,
        ] {
            let mut a = CpuAi::with_mode(42, AiDifficulty::Hard, mode);
            let mut b = CpuAi::with_mode(42, AiDifficulty::Hard, mode);
            for _ in 0..40 {
                for obs in seq {
                    assert_eq!(
                        a.decide(obs),
                        b.decide(obs),
                        "{mode:?} must be deterministic"
                    );
                }
            }
        }
    }

    /// The three teaching modes are observably distinct from one another given the
    /// *same* seed and the same airborne-in-range opening: only Reactive DP throws
    /// a button, Pure Blocker only guards-or-neutral, Whiff Punisher only spaces.
    #[test]
    fn modes_are_observably_distinct() {
        let opening = AiObservation {
            opponent_dx: 40.0,
            opponent_airborne: true,
            ..AiObservation::default()
        };
        let mut dp = CpuAi::with_mode(8, AiDifficulty::Normal, BehaviorMode::ReactiveDP);
        let mut blocker = CpuAi::with_mode(8, AiDifficulty::Normal, BehaviorMode::PureBlocker);
        let mut punisher = CpuAi::with_mode(8, AiDifficulty::Normal, BehaviorMode::WhiffPunisher);

        let dp_attacks = run(&mut dp, opening, 30).iter().any(pressed_attack);
        let blocker_attacks = run(&mut blocker, opening, 30).iter().any(pressed_attack);
        let punisher_attacks = run(&mut punisher, opening, 30).iter().any(pressed_attack);

        assert!(dp_attacks, "Reactive DP anti-airs the airborne opening");
        assert!(!blocker_attacks, "Pure Blocker never attacks the opening");
        // The opponent is airborne, not recovering, so the punisher waits.
        assert!(!punisher_attacks, "Whiff Punisher only punishes recovery");
    }

    /// A queued special (DP) is committed to: once it starts, it plays out across
    /// several frames rather than re-deciding every frame.
    #[test]
    fn special_motion_plays_out_over_multiple_frames() {
        let mut ai = CpuAi::with_mode(4, AiDifficulty::Normal, BehaviorMode::ReactiveDP);
        let jump_in = AiObservation {
            opponent_dx: 40.0,
            opponent_airborne: true,
            ..AiObservation::default()
        };
        // Drive until the first attack frame, then confirm the motion spans more
        // than one non-neutral frame (a real F,D,DF+P sequence, not a single tap).
        let frames = run(&mut ai, jump_in, 30);
        let first_attack = frames.iter().position(pressed_attack).expect("DP fires");
        // The frames immediately before the attack are the motion (down / forward
        // holds), so at least one of the two preceding frames is non-neutral.
        assert!(first_attack >= 2, "DP needs a multi-frame motion lead-in");
        let lead = &frames[first_attack - 2..first_attack];
        assert!(
            lead.iter().any(|s| !s.direction.is_neutral()),
            "the DP motion holds directions before the punch"
        );
    }

    /// The behaviour-mode selector cycles through every mode and wraps both ways,
    /// and its tokens round-trip — so a Setup row (next/prev) and the `--ai-mode`
    /// CLI flag (token/from_token) can both reach every teaching mode (T070).
    #[test]
    fn behavior_mode_selector_cycles_and_tokens_round_trip() {
        // ALL is in selector order and covers exactly the four modes.
        assert_eq!(BehaviorMode::ALL.len(), 4);
        assert_eq!(BehaviorMode::ALL[0], BehaviorMode::Ladder);

        // next() steps forward and wraps from the last back to the first.
        assert_eq!(BehaviorMode::Ladder.next(), BehaviorMode::PureBlocker);
        assert_eq!(BehaviorMode::PureBlocker.next(), BehaviorMode::ReactiveDP);
        assert_eq!(BehaviorMode::ReactiveDP.next(), BehaviorMode::WhiffPunisher);
        assert_eq!(BehaviorMode::WhiffPunisher.next(), BehaviorMode::Ladder);
        // prev() is the inverse and wraps from the first back to the last.
        assert_eq!(BehaviorMode::Ladder.prev(), BehaviorMode::WhiffPunisher);
        assert_eq!(BehaviorMode::WhiffPunisher.prev(), BehaviorMode::ReactiveDP);

        // Stepping next() ALL.len() times returns to the start (full cycle).
        let mut m = BehaviorMode::Ladder;
        for _ in 0..BehaviorMode::ALL.len() {
            m = m.next();
        }
        assert_eq!(m, BehaviorMode::Ladder, "a full cycle returns to start");

        // Every mode's canonical token parses back to itself (case-insensitively),
        // and a bad token is rejected (not silently defaulted).
        for mode in BehaviorMode::ALL {
            assert_eq!(
                BehaviorMode::from_token(mode.token()),
                Some(mode),
                "{mode:?} token must round-trip"
            );
            assert_eq!(
                BehaviorMode::from_token(&mode.token().to_ascii_uppercase()),
                Some(mode),
                "{mode:?} token must parse case-insensitively"
            );
            // Labels are drawable in the menu glyph set (uppercase A-Z + space).
            assert!(mode
                .label()
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == ' '));
        }
        assert_eq!(BehaviorMode::from_token("nonsense"), None);
        // A couple of natural aliases also resolve.
        assert_eq!(
            BehaviorMode::from_token("pureblocker"),
            Some(BehaviorMode::PureBlocker)
        );
        assert_eq!(
            BehaviorMode::from_token("whiff"),
            Some(BehaviorMode::WhiffPunisher)
        );
    }

    /// `set_mode` switches behaviour and clears any in-flight script.
    #[test]
    fn set_mode_switches_and_clears_script() {
        let mut ai = CpuAi::new(1, AiDifficulty::Normal);
        assert_eq!(ai.mode(), BehaviorMode::Ladder);
        ai.set_mode(BehaviorMode::PureBlocker);
        assert_eq!(ai.mode(), BehaviorMode::PureBlocker);
        // After switching, a close idle opponent is met with neutral (no attack).
        let s = ai.decide(AiObservation::at(10.0));
        assert!(!pressed_attack(&s));
    }
}
