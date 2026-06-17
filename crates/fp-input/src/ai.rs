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

/// The Park–Miller "minimal standard" multiplier (`7^5`).
const PARK_MILLER_MUL: i64 = 16807;
/// The Park–Miller modulus (`2^31 - 1`, a Mersenne prime).
const PARK_MILLER_MOD: i64 = 2_147_483_647;

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
/// Deliberately tiny and engine-agnostic: it carries only what the brain needs to
/// pick a direction and decide whether it is in range. The caller (the engine /
/// app) fills it from the live fighters each frame. The "toward" direction is
/// derived purely from [`opponent_dx`], so the AI needs no notion of its own
/// facing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AiObservation {
    /// Opponent X minus AI X, in world pixels. Positive ⇒ the opponent is to the
    /// AI's **right** (so "toward" is screen-right); negative ⇒ to the left.
    pub opponent_dx: f32,
}

impl AiObservation {
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
    /// Whether the attack button was pressed on the previous frame. The AI
    /// **pulses** its attack: a held button only fires a button command on its
    /// press edge, so the AI releases for one frame after a press to re-arm the
    /// next strike. This is part of the AI's deterministic state.
    attacked_last_frame: bool,
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
            attacked_last_frame: false,
        }
    }

    /// Creates a CPU AI with explicit [`AiTuning`] (fine control / tests).
    #[must_use]
    pub fn with_tuning(seed: i32, tuning: AiTuning) -> Self {
        Self {
            rng: AiRng::new(seed),
            tuning,
            attacked_last_frame: false,
        }
    }

    /// The behaviour parameters this AI is using.
    #[must_use]
    pub fn tuning(&self) -> AiTuning {
        self.tuning
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
    pub fn decide(&mut self, obs: AiObservation) -> InputState {
        // Always draw the same number of values per frame so the RNG stream does
        // not desync between branches.
        let block_roll = self.rng.next_below(1000);
        let jump_roll = self.rng.next_below(1000);

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
        let s = ai.decide(AiObservation { opponent_dx: 200.0 });
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
        let s = ai.decide(AiObservation {
            opponent_dx: -200.0,
        });
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
            let s = ai.decide(AiObservation { opponent_dx: 10.0 });
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
            let s = ai.decide(AiObservation { opponent_dx: 10.0 });
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
            let s = ai.decide(AiObservation { opponent_dx: 10.0 });
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
            AiObservation { opponent_dx: 200.0 },
            AiObservation { opponent_dx: 90.0 },
            AiObservation { opponent_dx: 10.0 },
            AiObservation {
                opponent_dx: -150.0,
            },
            AiObservation { opponent_dx: 5.0 },
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
            let sa = a.decide(AiObservation { opponent_dx: 5.0 });
            let sb = b.decide(AiObservation { opponent_dx: 5.0 });
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
            if easy
                .decide(AiObservation { opponent_dx: dx })
                .button(Button::A)
            {
                easy_attacks += 1;
            }
            if hard
                .decide(AiObservation { opponent_dx: dx })
                .button(Button::A)
            {
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
            if ai.decide(AiObservation { opponent_dx: 200.0 }).direction.up {
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
        let right = AiObservation { opponent_dx: 30.0 };
        assert!(right.opponent_on_right());
        assert_eq!(right.distance(), 30.0);
        let left = AiObservation { opponent_dx: -30.0 };
        assert!(!left.opponent_on_right());
        assert_eq!(left.distance(), 30.0);
        // Exact overlap counts as "right" (toward screen-right).
        assert!(AiObservation { opponent_dx: 0.0 }.opponent_on_right());
    }
}
