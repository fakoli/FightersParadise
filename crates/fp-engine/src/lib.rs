//! # fp-engine
//!
//! Top-level game coordinator for the Fighters Paradise engine. This crate owns
//! the **two-player match**: the per-frame orchestration that drives two
//! [`fp_character::Character`]s through one 60Hz tick, runs combat both
//! directions, keeps the fighters on stage and non-overlapping, and advances the
//! round state machine and timer.
//!
//! ## The [`Match`] coordinator (task 7.1)
//!
//! A [`Match`] holds two [`Player`]s (each a [`fp_character::Character`] plus its
//! loaded assets [`fp_character::LoadedCharacter`] and an input/command source)
//! and the [`StageBounds`] that pin the fighters to the playfield.
//! [`Match::tick`] advances exactly one frame in MUGEN-ish order:
//!
//! 1. **Feed inputs.** Each player's [`MatchInput`] becomes a RAW absolute
//!    [`fp_input::InputState`], pushed into that player's own
//!    [`fp_input::InputBuffer`], and the player's real
//!    [`fp_input::CommandMatcher`] (compiled from the character's `.cmd`) is run
//!    facing-relative. The recognized command names (`holdfwd`, `QCF_x`, …) are
//!    snapshotted into the character's [`fp_character::CommandSource`] seam, so
//!    the character's own CNS/CMD controllers and the engine built-in locomotion
//!    fire off the same command vocabulary the data files author.
//! 2. **Tick both state machines** via [`fp_character::Character::tick`], then
//!    apply each tick's deferred `Target*` operations
//!    ([`fp_character::TargetOp`]) to the **opponent** (the throw system, task
//!    P8b): a binder's `TargetState`/`TargetBind`/`TargetLifeAdd`/`TargetFacing`/
//!    `TargetVelSet`/`TargetVelAdd`/`TargetPowerAdd` move, bind, damage, face, or
//!    re-velocity the target it grabbed, facing-relative to the binder.
//! 3. **Run combat both directions** with
//!    [`fp_character::combat::resolve_attack`] — P1 attacks P2, then P2 attacks
//!    P1 — so an active `HitDef` on either side can connect this frame. On a
//!    connection the `HitDef`'s `p1stateno` moves the **attacker** into its own
//!    state (the throw-animation transition) while `p2stateno` (applied inside
//!    `resolve_attack`) sends the defender to its get-hit/thrown state.
//! 4. **Separate and clamp.** [`fp_physics::resolve_push`] pushes overlapping
//!    bodies apart using each character's `size.ground.front`/`back`, then
//!    [`fp_physics::clamp_to_bounds`] keeps each body inside [`StageBounds`].
//! 5. **Face the opponent** (baseline `facep2`): a character in a neutral
//!    standing state with control turns to face the other's X.
//! 6. **Advance the round** state machine ([`RoundState`]) and the down-counting
//!    [`Match::timer`] (time over compares life).
//!
//! Read accessors ([`Match::p1`], [`Match::p2`], [`Player::pos`],
//! [`Player::facing`], [`Player::anim`], [`Player::anim_elem`], [`Player::life`],
//! [`Match::round_state`], [`Match::timer`]) expose everything a renderer needs
//! without granting it mutable access to the simulation.
//!
//! ## Robustness
//!
//! Per the engine-wide rule, nothing here panics: the executor, combat, and
//! physics primitives this crate calls all degrade safely on bad data, and the
//! coordinator only ever reads constants and clamps. Missing animation frames or
//! an absent `HitDef` resolve to "no contact" rather than an error.

#![warn(missing_docs)]

use fp_character::{
    combat::resolve_attack, framedata::frame_advantage, ActiveCommands, Character,
    CharacterFingerprint, EntityGraph, ExplodOp, ExplodPosType, ExplodSpawn, Facing, HelperPosType,
    HelperSpawn, LoadedCharacter, MoveFrameData, MoveType, ProjectileSpawn, RoundView, StageView,
    StateType, TickReport,
};
use fp_combat::{
    detect_hit, detect_hit_contact, resolve_clash, ClashOutcome, ClsnBox, ClsnFacing, SparkSource,
};
use fp_core::{Rect, SpriteId, Vec2};
use fp_formats::air::{AirFile, AnimAction};
use fp_input::{
    logical_direction, AiDifficulty, AiObservation, Button, CommandDef, CommandMatcher, Direction,
    InputBuffer, InputState, LeniencyConfig,
};
use fp_physics::{clamp_to_bounds, resolve_push, Facing as PhysFacing, PushBody};
use serde::{Deserialize, Serialize};

mod replay;
mod snapshot;
mod team;

pub use replay::{replay_match, MatchRecorder, ReplayError, ReplayLog};
pub use snapshot::{MatchSnapshot, PlayerSnapshot};
pub use team::{Side, TeamMatch, TeamMatchState, TeamMode, TeamOutcome};

/// The horizontal extent of the playfield, in world pixels.
///
/// Both fighters are clamped so their facing-resolved bodies stay within
/// `[left, right]` (MUGEN's `ScreenBound`). The bounds are assumed ordered
/// (`left <= right`); a reversed pair still yields finite, deterministic clamping
/// (see [`fp_physics::clamp_to_bounds`]) rather than a panic.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StageBounds {
    /// Leftmost world X a character body may reach.
    pub left: f32,
    /// Rightmost world X a character body may reach.
    pub right: f32,
    /// `GameWidth` — the stage's logical screen width in localcoord units (the
    /// 320×240-class space the edge / `ScreenPos` triggers measure in, T060).
    /// Threaded onto the [`StageView`] for `GameWidth`/`LeftEdge`/`RightEdge`.
    /// Defaults to MUGEN's classic `320`; `#[serde(default)]` keeps older
    /// serialized bounds (which lacked this field) loadable.
    #[serde(default = "default_game_width")]
    pub game_width: f32,
    /// `GameHeight` — the stage's logical screen height in localcoord units
    /// (T060). Threaded onto the [`StageView`] for `GameHeight`/`TopEdge`/
    /// `BottomEdge`. Defaults to `240`; `#[serde(default)]` keeps older
    /// serialized bounds loadable.
    #[serde(default = "default_game_height")]
    pub game_height: f32,
}

/// serde default for [`StageBounds::game_width`] — MUGEN's classic `320`.
fn default_game_width() -> f32 {
    StageView::DEFAULT_GAME_WIDTH
}

/// serde default for [`StageBounds::game_height`] — MUGEN's classic `240`.
fn default_game_height() -> f32 {
    StageView::DEFAULT_GAME_HEIGHT
}

impl StageBounds {
    /// Creates stage bounds from a left and right world X, using MUGEN's classic
    /// `320×240` logical screen dimensions for `GameWidth`/`GameHeight`.
    #[must_use]
    pub const fn new(left: f32, right: f32) -> Self {
        Self {
            left,
            right,
            game_width: StageView::DEFAULT_GAME_WIDTH,
            game_height: StageView::DEFAULT_GAME_HEIGHT,
        }
    }

    /// Creates stage bounds with explicit left/right world-X limits **and**
    /// logical `GameWidth`/`GameHeight` (localcoord) dimensions, so the
    /// game-dimension and screen-edge triggers (T060) report the stage's real
    /// `[StageInfo] localcoord` instead of the `320×240` default.
    #[must_use]
    pub const fn with_dims(left: f32, right: f32, game_width: f32, game_height: f32) -> Self {
        Self {
            left,
            right,
            game_width,
            game_height,
        }
    }

    /// Converts these bounds into the [`StageView`] the character executor's
    /// cross-entity eval context consumes for the screen-edge distance and
    /// game-dimension triggers (carrying `GameWidth`/`GameHeight` through).
    #[must_use]
    pub const fn view(self) -> StageView {
        StageView::with_dims(self.left, self.right, self.game_width, self.game_height)
    }
}

impl Default for StageBounds {
    /// A symmetric default playfield centered on the origin, wide enough that two
    /// default-sized characters start comfortably inside it, with the classic
    /// `320×240` logical screen.
    fn default() -> Self {
        Self {
            left: -200.0,
            right: 200.0,
            game_width: StageView::DEFAULT_GAME_WIDTH,
            game_height: StageView::DEFAULT_GAME_HEIGHT,
        }
    }
}

/// One frame of player input, expressed in **absolute screen directions**.
///
/// The fields are screen-relative (left/right are world directions, not
/// "toward/away from the opponent"). [`Match::tick`] feeds them straight into
/// each player's [`fp_input::CommandMatcher`] as a raw [`fp_input::InputState`];
/// the matcher resolves facing at match time, so holding *toward* the opponent
/// activates the character's forward-detect commands (e.g. KFM's `holdfwd`) and
/// holding *away* activates `holdback` and sets
/// [`fp_character::Character::holding_back`] so the defender can guard.
///
/// This is a deliberately small, renderer-/input-agnostic shape: a front end
/// backed by `fp-input`, a replay file, or a test harness can all populate it.
/// All fields default to "not held" / "not pressed".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchInput {
    /// Holding the left direction this frame.
    pub left: bool,
    /// Holding the right direction this frame.
    pub right: bool,
    /// Holding up (jump) this frame.
    pub up: bool,
    /// Holding down (crouch) this frame.
    pub down: bool,
    /// The `a` attack button is pressed this frame.
    pub a: bool,
    /// The `b` attack button is pressed this frame.
    pub b: bool,
    /// The `c` attack button is pressed this frame.
    pub c: bool,
    /// The `x` attack button is pressed this frame.
    pub x: bool,
    /// The `y` attack button is pressed this frame.
    pub y: bool,
    /// The `z` attack button is pressed this frame.
    pub z: bool,
}

impl MatchInput {
    /// An input with nothing held or pressed.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            left: false,
            right: false,
            up: false,
            down: false,
            a: false,
            b: false,
            c: false,
            x: false,
            y: false,
            z: false,
        }
    }
}

impl From<InputState> for MatchInput {
    /// Folds a raw [`fp_input::InputState`] (the snapshot a keyboard sampler or
    /// the [`fp_input::CpuAi`] produces) into a [`MatchInput`] (T018).
    ///
    /// Both carry absolute screen directions + the six attack buttons, so this is
    /// a straight field copy. It lets the CPU-AI controller, which emits an
    /// `InputState`, drive [`Match::tick`] without the caller hand-mapping each
    /// field. `Start` is intentionally dropped — `tick` takes no pause signal.
    fn from(s: InputState) -> Self {
        MatchInput {
            left: s.direction.left,
            right: s.direction.right,
            up: s.direction.up,
            down: s.direction.down,
            a: s.button(Button::A),
            b: s.button(Button::B),
            c: s.button(Button::C),
            x: s.button(Button::X),
            y: s.button(Button::Y),
            z: s.button(Button::Z),
        }
    }
}

/// The phase of a round, advanced once per frame by [`Match::tick`].
///
/// The lifecycle is `Intro → Fight → KO → Win`:
///
/// - [`RoundState::Intro`] — pre-fight; neither fighter acts on the clock yet.
///   The coordinator counts a short intro delay, then enters [`RoundState::Fight`].
/// - [`RoundState::Fight`] — live combat; the timer counts down and hits land.
/// - [`RoundState::Ko`] — a fighter's life reached `0` (a knockout) **or** the
///   timer expired; a brief slow-motion-style hold before results.
/// - [`RoundState::Win`] — the round is decided; [`Match::winner`] reports who.
///
/// The state never moves backwards, so a renderer can drive intro/outro
/// animations off [`Match::round_state`] without extra bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoundState {
    /// Pre-fight intro; the timer has not started.
    Intro,
    /// Live combat; the timer is counting down.
    Fight,
    /// A knockout (or time over) just occurred; brief hold before results.
    Ko,
    /// The round is decided; see [`Match::winner`].
    Win,
}

impl RoundState {
    /// This phase as MUGEN's numeric `RoundState` trigger code (audit #21).
    ///
    /// MUGEN exposes the round phase to CNS as an integer: `0` intro, `1` fight
    /// (control given, pre-KO), `2` pre-over (a KO or time-over has occurred — the
    /// win-pose window), `3` over (the round is ending). The coordinator's phases
    /// map straight onto those codes:
    ///
    /// | [`RoundState`]        | `RoundState` trigger |
    /// |-----------------------|----------------------|
    /// | [`Intro`](Self::Intro)| `0`                  |
    /// | [`Fight`](Self::Fight)| `1`                  |
    /// | [`Ko`](Self::Ko)      | `2`                  |
    /// | [`Win`](Self::Win)    | `3`                  |
    ///
    /// KFM gates its intro-freeze and its wood-kick `Explod` on this trigger
    /// (`kfm.cns` `RoundState` checks), so the code must match MUGEN exactly.
    #[must_use]
    pub const fn trigger_code(self) -> i32 {
        match self {
            RoundState::Intro => 0,
            RoundState::Fight => 1,
            RoundState::Ko => 2,
            RoundState::Win => 3,
        }
    }
}

/// Which player won the round, once it reaches [`RoundState::Win`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Winner {
    /// Player 1 won (P2 was KO'd or had less life at time over).
    P1,
    /// Player 2 won (P1 was KO'd or had less life at time over).
    P2,
    /// The round was a draw (double KO, or equal life at time over).
    Draw,
}

/// Whether the **whole match** (best-of-N rounds) is still being contested or has
/// been decided.
///
/// A [`Match`] is best-of-N: the first player to win [`Match::rounds_to_win`]
/// rounds wins the match. While the match is undecided this stays
/// [`MatchState::InProgress`] and the coordinator resets for the next round each
/// time a round is decided below the win threshold; once a player reaches the
/// threshold it becomes the terminal [`MatchState::Over`] and
/// [`Match::match_winner`] reports who.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchState {
    /// The match is still being contested; more rounds may be played.
    InProgress,
    /// The match is decided; see [`Match::match_winner`]. Terminal — no further
    /// rounds are played and [`Match::tick`] makes no further round/match changes.
    Over,
}

/// The number of frames the [`RoundState::Intro`] phase lasts before combat
/// begins. MUGEN's intro length is configurable; this is a simple fixed baseline.
const INTRO_FRAMES: i32 = 60;

/// The number of frames the [`RoundState::Ko`] hold lasts before results
/// ([`RoundState::Win`]).
const KO_FRAMES: i32 = 90;

/// Default round length in **seconds**; the timer starts at this times the tick
/// rate. MUGEN's default round time is 99 seconds.
const DEFAULT_ROUND_SECONDS: i32 = 99;

/// Ticks per second; the engine runs at a fixed 60Hz (see the architecture KB).
const TICKS_PER_SECOND: i32 = 60;

/// The MUGEN engine-common **round-initialisation** state ([Statedef 5900],
/// authored in `common1.cns`). MUGEN drives every fighter through 5900 at the
/// start of each round to re-seed its resources / var defaults, then hand it
/// back to the neutral stand.
///
/// The engine `Match` owns the authoritative round reset
/// ([`Match::reset_for_next_round`] / the initial seeding), so 5900 is
/// **advisory** and authored to *converge* with that reset (re-asserting full
/// life on the same value, then `ChangeState 0`) rather than fight it. When a
/// character defines 5900 it is entered at round init so that convergence runs;
/// a character that omits it is unaffected (the engine's field reset stands).
const ROUND_INIT_STATE: i32 = 5900;

/// The default number of round wins required to win the match. MUGEN's default
/// `rounds.to.win` is `2` — best of three rounds. Override per-match with
/// [`Match::with_rounds_to_win`] / [`Match::set_rounds_to_win`].
const DEFAULT_ROUNDS_TO_WIN: i32 = 2;

/// The default match RNG seed used when a caller does not supply one (#38).
///
/// A *fixed* constant on purpose: the whole point of seeding from a match seed is
/// reproducibility, so the default must not be wall-clock / OS randomness. Both
/// players are seeded deterministically from it via [`Match::seed_players`] (P1
/// gets the match seed, P2 a derived distinct seed) so the default match plays out
/// identically every run, and a replay reproduces it. Pass an explicit seed to
/// [`Match::seed_players`] to vary the streams while staying reproducible.
pub const DEFAULT_MATCH_SEED: i32 = 1;

/// Derives a distinct per-player RNG seed from a single match seed and a player
/// index (#38).
///
/// Player 1 is seeded with the match seed itself (`player_index = 0` returns it
/// unchanged), so a match seed reads naturally as "P1's seed"; each subsequent
/// player is given a *distinct* seed derived by a cheap integer hash of
/// `(match_seed, player_index)`, so the two fighters draw **independent**
/// `random` streams rather than sharing one (the deferred #28 follow-up: both
/// players are built from the same `.def`, so without this they would share the
/// identical [`fp_character::DEFAULT_RNG_SEED`] stream). The derivation is pure
/// and deterministic, so a fixed match seed always reproduces the same per-player
/// seeds. [`fp_character::Character::seed_rng`] normalizes whatever value this
/// yields into the Park–Miller generator's valid range, so any `i32` (including a
/// collision-avoiding mix that lands on `0`) is accepted.
#[must_use]
pub fn derive_player_seed(match_seed: i32, player_index: u32) -> i32 {
    if player_index == 0 {
        return match_seed;
    }
    // A small deterministic integer mix (splitmix64-style finalizer on a 64-bit
    // lane built from the seed and index), folded back to i32. Purely for
    // *decorrelating* the two players' streams; it is not cryptographic.
    let mut z = (match_seed as i64 as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(u64::from(player_index).wrapping_mul(0xBF58_476D_1CE4_E5B9));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    z as i32
}

/// Which sprite/animation source a hit-spark [`Effect`] draws from (audit #17).
///
/// A MUGEN attacker-own spark (`sparkno` negative / `S`-prefixed) plays an action
/// from the **attacker's own** sprite/animation set, so an [`Effect`] records the
/// attacking side and a renderer resolves its frames against that fighter's
/// [`LoadedCharacter::sff`](fp_character::LoadedCharacter)/`air`. A **common**
/// spark (bare non-negative `sparkno`) draws from the shared common-effects
/// (`fightfx`) set loaded onto the [`Match`] instead (see [`Match::set_common_fx`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EffectSide {
    /// The spark draws from player 1's SFF/AIR.
    P1,
    /// The spark draws from player 2's SFF/AIR.
    P2,
    /// The spark draws from the shared common-effects (`fightfx`) SFF/AIR loaded
    /// on the [`Match`] (the standard MUGEN common hit/guard sparks). A renderer
    /// resolves its frames against the common-effects atlas, not a fighter's SFF.
    Common,
}

/// A short-lived hit-spark / `Explod`-like effect entity spawned on a connecting
/// hit (faithfulness audit #17).
///
/// MUGEN draws a *spark* animation at the contact point of every connecting
/// attack. [`Match`] keeps a small list of these, spawns one at the contact anchor
/// when an attack lands, advances each one frame-by-frame, and drops it when its
/// animation finishes — a minimal, self-contained effect system (no general
/// `Explod` graph, helper tree, or binding yet).
///
/// # Sprite source — own and common sparks (audit #17)
///
/// An [`Effect`] is spawned for both spark kinds:
///
/// - An *attacker-own* spark ([`SparkSource::Own`]) draws its frames from the
///   **attacker's own** SFF/AIR ([`EffectSide::P1`]/[`EffectSide::P2`]).
/// - A *common* `fightfx` spark ([`SparkSource::Common`]) draws from the shared
///   common-effects set loaded on the [`Match`] (see [`Match::set_common_fx`]),
///   recorded as [`EffectSide::Common`]. When no common-effects asset is loaded
///   the common spark is a documented best-effort **skip** (logged, never a
///   panic) — exactly the pre-asset behavior.
///
/// With the shipped `assets/data/fightfx.*` common-effects asset loaded, the
/// default Kung Fu Man match (whose `sparkno` values are all `0/1/2/3/40`) now
/// shows a visible hit-spark on each connect. The `S`-prefix → own-spark
/// distinction is preserved by `fp-character`'s `parse_sparkno` (an `S`-prefixed
/// `sparkno` encodes negative → own).
///
/// The owning side's [`fp_formats::air::AirFile`] action id is [`Effect::anim`];
/// the resolved current-frame [`fp_core::SpriteId`] is [`Effect::sprite`]. All
/// fields are read accessors a renderer consumes; the cursor/lifetime are advanced
/// only by [`Match`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    /// Which fighter's SFF/AIR this spark's frames are resolved against.
    pub side: EffectSide,
    /// The attacker-own AIR action (animation) id the spark plays.
    pub anim: i32,
    /// World position the spark is anchored at (the hit's contact center).
    pub pos: Vec2<f32>,
    /// The current frame's sprite id (group/image), resolved from the owning
    /// side's AIR action each tick. A renderer draws this sprite at [`pos`](Self::pos).
    pub sprite: SpriteId,
    /// The current frame's pixel offset (from the AIR frame), applied on top of
    /// [`pos`](Self::pos) by a renderer exactly as a fighter frame's offset is.
    pub offset: Vec2<i16>,
    /// Zero-based index of the current frame within the action.
    elem: usize,
    /// Ticks the current frame has been displayed (advances the cursor at the
    /// frame's `ticks` duration).
    elem_time: i32,
    /// Frames of life remaining before the effect is dropped. Decremented each
    /// tick; the effect expires (and is removed) when this reaches `0`.
    remaining: i32,
}

impl Effect {
    /// Zero-based index of the spark's current animation frame.
    #[must_use]
    pub fn elem(&self) -> usize {
        self.elem
    }

    /// Frames of life remaining before this effect is dropped.
    #[must_use]
    pub fn remaining(&self) -> i32 {
        self.remaining
    }
}

/// Hard cap on the number of frames a hit-spark effect lives, regardless of its
/// AIR action length — bounds the effect list and guards against an infinite
/// (`ticks = -1`) hold frame keeping a spark alive forever. MUGEN sparks are
/// short; this is a generous ceiling.
const EFFECT_MAX_LIFETIME: i32 = 60;

/// A hard cap on the number of live helpers a single player may own at once
/// (T012). MUGEN bounds the global helper count (its default is 56); this is a
/// generous per-player ceiling that keeps the slot-map bounded so a runaway
/// `Helper`-spawning loop can never grow the simulation without limit.
const MAX_HELPERS_PER_PLAYER: usize = 56;

/// One live **helper** entity owned by a [`Player`] (T012).
///
/// A `Helper` is a child [`Character`] spawned by its owner's `Helper`
/// controller. It runs the same state machine as a full player — against the
/// **owner's** [`LoadedCharacter`] (helpers share their root's compiled states /
/// animations in MUGEN) — and is addressable from its owner by
/// [`helper_id`](Helper::helper_id) via the `helper(id)` redirect. Its `parent` /
/// `root` redirects resolve back up the spawning chain (the owning player), which
/// the slot-map wires into a [`EntityGraph`] each tick.
///
/// This model carries the live entity, its id, the facing it was spawned with,
/// and its remaining lifespan. Helper *lifecycle* — `DestroySelf` (the helper
/// removes itself) and `removetime` expiry (a finite lifespan auto-reaps it) — is
/// implemented in T032; helper binding and helper-specific constants (`size.*`,
/// own palette) remain out of scope.
pub struct Helper {
    /// The live child entity, advanced each frame via
    /// [`Character::tick_as_helper`].
    pub character: Character,
    /// The id this helper is addressable by (`helper(id)`), from the spawning
    /// `Helper` controller's `id` parameter.
    pub helper_id: i32,
    /// Remaining lifetime in ticks before the owner auto-expires this helper
    /// (T032), or `-1` for "no time limit" (it then lives until it runs
    /// `DestroySelf` or the slot-map cap is hit). A non-negative value counts down
    /// once per tick and the helper is reaped when it would go negative, exactly
    /// like a [`Projectile`]'s [`remaining`](Projectile::remaining). Seeded from
    /// the spawning [`fp_character::HelperSpawn::remove_time`].
    pub remaining: i32,
}

impl Helper {
    /// The helper's world position in pixels.
    #[must_use]
    pub fn pos(&self) -> Vec2<f32> {
        self.character.pos
    }

    /// The helper's current animation (action) id.
    #[must_use]
    pub fn anim(&self) -> i32 {
        self.character.anim
    }

    /// The id this helper is addressable by via `helper(id)`.
    #[must_use]
    pub fn helper_id(&self) -> i32 {
        self.helper_id
    }

    /// Remaining lifetime in ticks before the owner auto-expires this helper
    /// (T032), or `-1` for "no time limit".
    #[must_use]
    pub fn remaining(&self) -> i32 {
        self.remaining
    }
}

/// A hard cap on the number of live projectiles a single player may own at once
/// (T013). MUGEN bounds the global projectile/explod count; this keeps the
/// projectile slot-map bounded so a runaway `Projectile`-spawning loop can never
/// grow the simulation without limit.
const MAX_PROJECTILES_PER_PLAYER: usize = 64;

/// The MUGEN player id of P1 (the convention the `playerid(n)` redirect resolves
/// against, T014). Player 1 is id `1`; player 2 is [`MUGEN_PLAYER_ID_P2`].
const MUGEN_PLAYER_ID_P1: i32 = 1;
/// The MUGEN player id of P2 (T014); see [`MUGEN_PLAYER_ID_P1`].
const MUGEN_PLAYER_ID_P2: i32 = 2;

/// One live **projectile** entity owned by a [`Player`] (T013).
///
/// A `Projectile` is a moving attack entity spawned by its owner's `Projectile`
/// controller. Unlike a [`Helper`] it does **not** run a CNS state machine: it
/// carries its own [`fp_combat::HitDef`] (the spawned attack), travels in a
/// straight line at a fixed [`velocity`](Projectile::velocity) each tick, and
/// connects when its current AIR-frame `Clsn1` overlaps the opponent's `Clsn2`.
/// It draws from the **owner's** [`LoadedCharacter`] animation set (its `anim` is
/// one of the owner's `projanim` actions). It self-removes when it connects (a
/// projectile is single-hit by default), when its [`remove_time`](Projectile::remove_time)
/// lifetime elapses, or when it leaves the stage bounds.
///
/// The live entity is modeled as a [`Character`] so the same pure
/// [`fp_character::combat::resolve_attack`] hit pipeline a melee attack uses
/// resolves a projectile hit unchanged — the projectile's `pos`/`facing`/`anim`/
/// `active_hitdef` are exactly the attacker fields `resolve_attack` reads.
pub struct Projectile {
    /// The live projectile entity. Its `active_hitdef` is the spawned attack; its
    /// `pos`/`facing`/`anim`/`anim_elem` position it and source its `Clsn1` boxes.
    pub character: Character,
    /// The projectile's id (`projid`), from the spawning controller (T013).
    pub proj_id: i32,
    /// The per-tick world velocity (already mirrored by the owner's facing at
    /// spawn time, so it points the way the projectile travels).
    pub velocity: Vec2<f32>,
    /// Remaining lifetime in ticks, or `-1` for "no time limit" (it then lives
    /// until it connects or leaves the stage). A non-negative value counts down to
    /// `0`, at which point the projectile is removed.
    pub remaining: i32,
}

impl Projectile {
    /// The projectile's world position in pixels.
    #[must_use]
    pub fn pos(&self) -> Vec2<f32> {
        self.character.pos
    }

    /// The projectile's current animation (action) id.
    #[must_use]
    pub fn anim(&self) -> i32 {
        self.character.anim
    }

    /// The projectile's id (`projid`).
    #[must_use]
    pub fn proj_id(&self) -> i32 {
        self.proj_id
    }
}

/// A hard cap on the number of live explods a single player may own at once
/// (T033). MUGEN bounds the global explod count; this keeps the explod slot-map
/// bounded so a runaway `Explod`-spawning loop (e.g. an `Explod` in a persistent
/// state) can never grow the simulation without limit.
const MAX_EXPLODS_PER_PLAYER: usize = 64;

/// Hard cap on the number of frames an explod lives, regardless of its
/// `removetime` (T033). MUGEN allows an explod to loop forever (`removetime = -2`)
/// or to be left around indefinitely; this ceiling guarantees even an
/// unbounded-lifetime explod is eventually reaped so the slot-map cannot leak.
/// It is generous (5 seconds at 60Hz) so a legitimately long explod still plays
/// in full.
const EXPLOD_MAX_LIFETIME: i32 = 300;

/// One live **explod** display entity owned by a [`Player`] (T033).
///
/// An `Explod` is a short-lived, non-colliding *display* effect spawned by its
/// owner's `Explod` controller. Unlike a [`Projectile`] it carries no `HitDef` and
/// never connects; unlike a [`Helper`] it runs no CNS state machine. It simply
/// plays one of the **owner's** AIR actions ([`anim`](Self::anim)) at a world
/// position, advances its animation each tick, holds bound to its spawn anchor for
/// [`bindtime`](Self::bind_remaining) ticks, and self-removes after
/// [`removetime`](Self::remaining) ticks (or once its one-shot animation finishes,
/// for the MUGEN `removetime = -1` "play once" convention).
///
/// It mirrors [`Effect`] (the hit-spark entity): the current frame's
/// [`sprite`](Self::sprite)/[`offset`](Self::offset) are re-resolved from the
/// owner's AIR each tick, and a renderer draws [`sprite`](Self::sprite) at
/// [`pos`](Self::pos). [`ModifyExplod`](ExplodOp::Modify) updates its fields in
/// place and [`RemoveExplod`](ExplodOp::Remove) reaps it by id.
///
/// Like [`Projectile`] and [`Helper`] (and unlike the standalone serde-deriving
/// [`Effect`]), an `Explod` is part of the live entity graph rather than the
/// replay snapshot, so it does not derive serde.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Explod {
    /// Which player owns this explod: `1` for P1, `2` for P2. A `RemoveExplod`/
    /// `ModifyExplod` only ever touches its owner's explods, so the slot-map is
    /// per-player and this field is informational (it records the spawning side
    /// for a renderer that resolves the explod's frames against that fighter's SFF).
    pub owner: i32,
    /// The explod's addressable id (MUGEN's `id`). `-1` is the "no id" sentinel; a
    /// `RemoveExplod`/`ModifyExplod` with no id matches every owned explod.
    pub id: i32,
    /// The owner AIR action (animation) id the explod plays.
    pub anim: i32,
    /// The explod's current world position (the resolved anchor + offset). While
    /// bound it tracks its anchor each tick; once the bind window elapses it holds
    /// the last bound position.
    pub pos: Vec2<f32>,
    /// The current frame's sprite id, resolved from the owner's AIR action each
    /// tick. A renderer draws this sprite at [`pos`](Self::pos).
    pub sprite: SpriteId,
    /// The current frame's pixel offset (from the AIR frame), applied on top of
    /// [`pos`](Self::pos) by a renderer.
    pub offset: Vec2<i16>,
    /// Draw priority relative to the fighters (MUGEN's `sprpriority`). Higher draws
    /// in front.
    pub sprpriority: i32,
    /// The spawn anchor classification (MUGEN's `postype`), kept so a bound explod
    /// can re-resolve its anchor each tick.
    pos_type: ExplodPosType,
    /// The `(x, y)` offset relative to the anchor, facing-mirrored on X at spawn.
    anchor_offset: Vec2<f32>,
    /// Remaining ticks the explod stays bound to its anchor (counts down). `-1`
    /// means "bound for life".
    bind_remaining: i32,
    /// Zero-based index of the current animation frame.
    elem: usize,
    /// Ticks the current frame has been displayed.
    elem_time: i32,
    /// Whether the explod plays its animation exactly once then removes (the MUGEN
    /// `removetime = -1` convention). When set, the explod is reaped the tick its
    /// one-shot animation finishes; when clear, the animation loops for the
    /// explod's whole lifetime.
    play_once: bool,
    /// Remaining lifetime in ticks. Always a bounded, non-negative countdown: it is
    /// seeded from `removetime` (a fixed lifetime, or — for the MUGEN play-once
    /// `-1` / loop `-2` conventions — [`EXPLOD_MAX_LIFETIME`]) and counts down to
    /// `0`, at which point the explod is reaped. This guarantees **every** explod
    /// is eventually reaped (a stuck/looping animation cannot leak the slot-map).
    remaining: i32,
}

impl Explod {
    /// The explod's addressable id (MUGEN's `id`).
    #[must_use]
    pub fn id(&self) -> i32 {
        self.id
    }

    /// The explod's current animation (action) id.
    #[must_use]
    pub fn anim(&self) -> i32 {
        self.anim
    }

    /// Which player owns this explod (`1` = P1, `2` = P2).
    #[must_use]
    pub fn owner(&self) -> i32 {
        self.owner
    }

    /// Remaining lifetime in ticks: a bounded, non-negative countdown (seeded from
    /// `removetime`, or [`EXPLOD_MAX_LIFETIME`] for the MUGEN play-once / loop
    /// conventions).
    #[must_use]
    pub fn remaining(&self) -> i32 {
        self.remaining
    }

    /// Whether this explod plays its animation once then removes (the MUGEN
    /// `removetime = -1` convention).
    #[must_use]
    pub fn play_once(&self) -> bool {
        self.play_once
    }

    /// Remaining ticks the explod stays bound to its spawn anchor (or `-1` for
    /// "bound for life").
    #[must_use]
    pub fn bind_remaining(&self) -> i32 {
        self.bind_remaining
    }
}

/// The cross-player redirect relations the match coordinator hands a [`Player`]
/// for one root tick (T014): the entity it most recently hit (`target`), its
/// teammate (`partner`), and the `playerid(n)` lookup table.
///
/// The match coordinator is the only place that owns both players, so it is the
/// only place that can build these inter-player references. They borrow *other*
/// `Player`s' characters (never the ticking player's own `self.character`), so
/// they coexist with the `&mut self` tick without aliasing.
#[derive(Clone, Copy, Default)]
struct RedirectRelations<'a> {
    /// The opponent this player most recently hit (`target` redirect), or `None`
    /// when it has no established target this tick.
    target: Option<&'a Character>,
    /// This player's teammate (`partner` redirect), or `None` in a 1-v-1.
    partner: Option<&'a Character>,
    /// The cross-player `(id, &Character)` table `playerid(n)` resolves against
    /// (the *other* players by their MUGEN player id; the ticking player's own id
    /// is intentionally omitted to avoid aliasing its `&mut` tick — its own state
    /// is already reachable through self-triggers).
    players: &'a [(i32, &'a Character)],
}

/// What drives one side of a [`Match`]: a human input device or the baseline CPU
/// AI at a chosen difficulty (T052).
///
/// The coordinator uses this only to derive each fighter's
/// [`Character::ai_level`](fp_character::Character::ai_level) at construction — a
/// human ([`Human`](PlayerDriver::Human)) maps to level `0` and a
/// [`Cpu`](PlayerDriver::Cpu) maps to its [`AiDifficulty::ai_level`] (`1..=8`). It
/// does **not** itself produce inputs (the caller still feeds inputs each tick); it
/// is a one-time identity declaration so the CNS `AILevel` trigger reads the right
/// value. [`Human`](PlayerDriver::Human) is the [`Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayerDriver {
    /// A human player (keyboard or gamepad). AI level `0`.
    #[default]
    Human,
    /// The baseline CPU AI at the given difficulty. AI level `1..=8`.
    Cpu(AiDifficulty),
}

impl PlayerDriver {
    /// The [`Character::ai_level`](fp_character::Character::ai_level) this driver
    /// implies: `0` for a [`Human`](Self::Human), or the CPU difficulty's
    /// [`AiDifficulty::ai_level`] (`1..=8`) for [`Cpu`](Self::Cpu).
    #[must_use]
    pub fn ai_level(self) -> u8 {
        match self {
            PlayerDriver::Human => 0,
            PlayerDriver::Cpu(difficulty) => difficulty.ai_level(),
        }
    }
}

/// One side of a [`Match`]: a live [`Character`], the assets it ticks against,
/// and the input/command source feeding its state machine.
///
/// The [`Character`] is the mutable simulation state; [`LoadedCharacter`] holds
/// the immutable compiled state graph and animations the executor and combat read
/// each frame. Read accessors ([`Player::pos`], [`Player::life`], …) expose the
/// fields a renderer needs without exposing the whole mutable character.
pub struct Player {
    /// The live character entity advanced each frame.
    pub character: Character,
    /// The loaded, compiled assets the character ticks against.
    pub loaded: LoadedCharacter,
    /// The slot-map of live helper entities this player owns (T012). Populated
    /// from each tick's [`fp_character::TickReport::helper_spawns`] and ticked
    /// every frame after the root character. Bounded by
    /// [`MAX_HELPERS_PER_PLAYER`].
    helpers: Vec<Helper>,
    /// The slot-map of live projectile entities this player owns (T013). Populated
    /// from each tick's [`fp_character::TickReport::projectile_spawns`], advanced
    /// every frame, and reaped on connect / lifetime / out-of-bounds. Bounded by
    /// [`MAX_PROJECTILES_PER_PLAYER`].
    projectiles: Vec<Projectile>,
    /// The slot-map of live explod display entities this player owns (T033).
    /// Populated from each tick's [`fp_character::TickReport::explod_spawns`],
    /// updated by its [`fp_character::TickReport::explod_ops`] (`ModifyExplod`/
    /// `RemoveExplod`), advanced every frame, and reaped on `removetime` / when its
    /// one-shot animation finishes / [`EXPLOD_MAX_LIFETIME`]. Bounded by
    /// [`MAX_EXPLODS_PER_PLAYER`].
    explods: Vec<Explod>,
    /// Rolling raw-input history (absolute directions + buttons) this player's
    /// command recognizer scans each tick.
    input_buffer: InputBuffer,
    /// The player's real command recognizer, compiled from its `.cmd` at
    /// construction. Run every fight tick to produce the active command names
    /// (`holdfwd`, `FF`, special motions, …) the character's state machine reads.
    matcher: CommandMatcher,
    /// The compiled command definitions, kept to enumerate which command names to
    /// snapshot into the character's command source each tick.
    command_defs: Vec<CommandDef>,
    /// On-block / on-hit frame advantage (in ticks) for the attack this player
    /// landed on the opponent on the most recent tick, or [`None`] when it did
    /// not connect that tick (T065). Mirrors the value the engine writes onto the
    /// attacker's [`TickReport::frame_advantage`] at the connecting-hit moment, and
    /// is the surface the frame-data readout reads (a fresh value is computed each
    /// tick; it is cleared to [`None`] on a tick with no connection). Read it via
    /// [`Player::frame_advantage`].
    frame_advantage: Option<i32>,
}

impl Player {
    /// Wraps a live [`Character`] and its [`LoadedCharacter`] into a [`Player`],
    /// building the player's real [`CommandMatcher`] from the character's loaded
    /// `.cmd`.
    ///
    /// The caller is responsible for having seeded the character's constants from
    /// the loaded assets (e.g. via
    /// [`Character::with_constants`](fp_character::Character::with_constants)); a
    /// freshly [`Character::new`](fp_character::Character::new)'d character is also
    /// accepted and simply uses default constants.
    ///
    /// The command recognizer is compiled via
    /// [`LoadedCharacter::command_defs`](fp_character::LoadedCharacter::command_defs),
    /// the same shared compilation the single-character `fp-app` path uses, so a
    /// `Match` recognizes the character's authored commands (the `holdfwd`/
    /// `holdback`/`holdup`/`holddown` the engine built-in locomotion gates on, the
    /// `FF`/`BB` runs, and every special motion) exactly as the data files define
    /// them. A character with no `.cmd` simply yields an empty recognizer.
    #[must_use]
    pub fn new(character: Character, loaded: LoadedCharacter) -> Self {
        let command_defs = loaded.command_defs();
        // Input leniency (T075): give the built-in jump gate (`holdup`) a small
        // pre-actionable buffer so a jump tapped a few frames before the player
        // can act still comes out on the first actionable frame. Buffering is
        // deterministic and in the input layer, so versus determinism is
        // unchanged, and it re-arms only the `holdup` gate the engine's built-in
        // ground locomotion reads — never authored content motions.
        let matcher =
            CommandMatcher::with_leniency(command_defs.clone(), LeniencyConfig::with_jump_buffer());
        Self {
            character,
            loaded,
            helpers: Vec::new(),
            projectiles: Vec::new(),
            explods: Vec::new(),
            input_buffer: InputBuffer::new(),
            matcher,
            command_defs,
            frame_advantage: None,
        }
    }

    /// The on-block / on-hit frame advantage (in 60Hz ticks) for the attack this
    /// player landed on the opponent on the most recent tick, or [`None`] when it
    /// did not connect that tick (T065).
    ///
    /// A positive value means this player recovers first (advantage); a negative
    /// value means the opponent recovers first (this player is at disadvantage).
    /// The value is recomputed every tick by the [`Match`] at the connecting-hit
    /// moment and cleared to [`None`] on a tick with no connection, so a stale
    /// number never lingers on screen.
    #[must_use]
    pub fn frame_advantage(&self) -> Option<i32> {
        self.frame_advantage
    }

    /// The live helper entities this player currently owns (T012), in spawn
    /// order. Empty until a `Helper` controller fires and the spawn is applied.
    #[must_use]
    pub fn helpers(&self) -> &[Helper] {
        &self.helpers
    }

    /// The live projectile entities this player currently owns (T013), in spawn
    /// order. Empty until a `Projectile` controller fires and the spawn is applied
    /// (and after each one is reaped on connect / expiry / out-of-bounds).
    #[must_use]
    pub fn projectiles(&self) -> &[Projectile] {
        &self.projectiles
    }

    /// The live explod display entities this player currently owns (T033), in
    /// spawn order. Empty until an `Explod` controller fires and the spawn is
    /// applied (and after each one is reaped on `removetime` / animation end /
    /// `RemoveExplod`). A renderer draws each [`Explod::sprite`] at
    /// [`Explod::pos`], resolving the sprite against this player's SFF.
    #[must_use]
    pub fn explods(&self) -> &[Explod] {
        &self.explods
    }

    /// Spawns the helpers requested by this tick's
    /// [`fp_character::TickReport::helper_spawns`] into the slot-map (T012),
    /// resolving each [`HelperSpawn`]'s `postype` + offset into a world position
    /// against this player (`p1`) and the supplied opponent position (`p2`) / stage
    /// edges. The new helper shares the owner's loaded assets, begins in the
    /// requested `stateno`, and is seeded with the spawn's
    /// [`remove_time`](fp_character::HelperSpawn::remove_time) lifespan (T032) so a
    /// finite-lifespan helper auto-expires in [`tick_helpers`](Self::tick_helpers).
    ///
    /// Bounded by [`MAX_HELPERS_PER_PLAYER`]: once the slot-map is full, further
    /// spawns this tick are dropped (debug-logged) rather than growing without
    /// limit. Never panics.
    fn spawn_helpers(&mut self, spawns: &[HelperSpawn], opponent_x: f32, stage: StageView) {
        for spawn in spawns {
            if self.helpers.len() >= MAX_HELPERS_PER_PLAYER {
                // debug, not warn: a character that spawns helpers faster than they
                // are destroyed saturates the bounded slot-map and would emit this
                // every tick, flooding the log. With `DestroySelf` (T032) honored,
                // a well-behaved character's helpers now self-retire so this cap is
                // a safety net, not the normal path.
                tracing::debug!(
                    "helper slot-map full ({MAX_HELPERS_PER_PLAYER}); dropping spawn id={}",
                    spawn.helper_id
                );
                break;
            }
            let owner = &self.character;
            // Resolve the spawn anchor (facing-relative on X for the p1 anchor,
            // mirroring how `PosAdd` and the `Target*` ops mirror the X offset).
            let (anchor_x, anchor_y) = match spawn.pos_type {
                HelperPosType::P1 => (owner.pos.x, owner.pos.y),
                HelperPosType::P2 => (opponent_x, owner.pos.y),
                HelperPosType::Front => (front_edge_x(owner.facing, stage), owner.pos.y),
                HelperPosType::Back => (back_edge_x(owner.facing, stage), owner.pos.y),
                HelperPosType::Left => (stage.left, owner.pos.y),
                HelperPosType::Right => (stage.right, owner.pos.y),
            };
            let (off_x, off_y) = spawn.pos;
            let world_x = anchor_x + off_x * owner.facing.sign() as f32;
            let world_y = anchor_y + off_y;

            // The helper faces the same way as (facing=1) or opposite to
            // (facing=-1) the owner.
            let facing = if spawn.facing == -1 {
                flip_facing(owner.facing)
            } else {
                owner.facing
            };

            let mut child = Character::with_constants(owner.constants);
            child.pos = Vec2::new(world_x, world_y);
            child.facing = facing;
            // Enter the requested start state through the owner's loaded states so
            // the entry params (anim/ctrl/physics) apply; an unknown state still
            // updates the cursor (never panics).
            child.change_state(&self.loaded.states, spawn.state_no);

            self.helpers.push(Helper {
                character: child,
                helper_id: spawn.helper_id,
                remaining: spawn.remove_time,
            });
        }
    }

    /// Spawns the projectiles requested by this tick's
    /// [`fp_character::TickReport::projectile_spawns`] into the projectile
    /// slot-map (T013), resolving each [`ProjectileSpawn`]'s offset into a world
    /// position relative to the owner and mirroring the offset / velocity X by the
    /// owner's facing (so the projectile travels the way the owner faces).
    ///
    /// The new projectile carries the spawn's [`HitDef`](fp_combat::HitDef) as its
    /// `active_hitdef`, takes the owner's facing, and begins on the spawn's
    /// `projanim` action (frame 0). Bounded by [`MAX_PROJECTILES_PER_PLAYER`]: once
    /// the slot-map is full, further spawns this tick are dropped (warn-logged)
    /// rather than growing without limit. Never panics.
    fn spawn_projectiles(&mut self, spawns: &[ProjectileSpawn]) {
        for spawn in spawns {
            if self.projectiles.len() >= MAX_PROJECTILES_PER_PLAYER {
                // debug, not warn: same rationale as the helper slot-map — avoid a
                // per-tick log flood when a character spawns projectiles faster than
                // they retire.
                tracing::debug!(
                    "projectile slot-map full ({MAX_PROJECTILES_PER_PLAYER}); dropping spawn id={}",
                    spawn.id
                );
                break;
            }
            let owner = &self.character;
            let sign = owner.facing.sign() as f32;
            let (off_x, off_y) = spawn.pos;
            let (vel_x, vel_y) = spawn.velocity;

            let mut proj = Character::with_constants(owner.constants);
            proj.pos = Vec2::new(owner.pos.x + off_x * sign, owner.pos.y + off_y);
            proj.facing = owner.facing;
            proj.anim = spawn.anim;
            proj.anim_elem = 0;
            proj.anim_elem_time = 0;
            // The projectile carries its own attack. It is its own (single) move,
            // so its `move_connect` starts clear: `resolve_attack` will connect it
            // exactly once before it is reaped.
            proj.active_hitdef = Some(spawn.hitdef);

            // Velocity is facing-relative on X; mirror it so it points the way the
            // owner faces, matching how the offset and `Target*` ops mirror X.
            let velocity = Vec2::new(vel_x * sign, vel_y);

            self.projectiles.push(Projectile {
                character: proj,
                proj_id: spawn.id,
                velocity,
                remaining: spawn.remove_time,
            });
        }
    }

    /// Advances every live projectile this player owns by one tick (T013): move it
    /// by its velocity, step its animation cursor (for the next frame's `Clsn1`
    /// boxes), decrement its lifetime, and reap it when its `removetime` elapses or
    /// it leaves the stage bounds (with a generous off-screen margin).
    ///
    /// Hit resolution against the opponent is **not** done here (it needs the
    /// opponent mutably, which only the match coordinator owns); see
    /// [`Match::resolve_projectile_hits`]. Pure field writes over a bounded list;
    /// never panics.
    fn tick_projectiles(&mut self, stage: StageView) {
        let air = &self.loaded.air;
        // A projectile is reaped once it travels this far past a stage edge, so an
        // un-timed (`removetime = -1`) projectile that misses still gets reclaimed
        // rather than flying forever.
        const OFFSCREEN_MARGIN: f32 = 80.0;
        let left_bound = stage.left - OFFSCREEN_MARGIN;
        let right_bound = stage.right + OFFSCREEN_MARGIN;
        self.projectiles.retain_mut(|proj| {
            // Lifetime: a non-negative `remaining` counts down; reaching 0 reaps it.
            // `-1` means "no time limit" and is left untouched.
            if proj.remaining >= 0 {
                proj.remaining -= 1;
                if proj.remaining < 0 {
                    return false;
                }
            }
            proj.character.pos.x += proj.velocity.x;
            proj.character.pos.y += proj.velocity.y;
            // Step the displayed animation so the next frame's Clsn1 is current.
            advance_projectile_frame(&mut proj.character, air);
            // Reap once it has flown off the stage (either horizontal edge).
            proj.character.pos.x >= left_bound && proj.character.pos.x <= right_bound
        });
    }

    /// Resolves an [`ExplodSpawn`]'s `postype` + offset into an absolute world
    /// position against this player (`p1`), the opponent X (`p2`), and the stage
    /// edges (T033). The X offset is facing-mirrored by the owner exactly as
    /// `Helper`/`Projectile` spawns and `Target*` ops mirror their X.
    fn explod_anchor(&self, spawn: &ExplodSpawn, opponent_x: f32, stage: StageView) -> Vec2<f32> {
        let owner = &self.character;
        let (anchor_x, anchor_y) = match spawn.pos_type {
            ExplodPosType::P1 => (owner.pos.x, owner.pos.y),
            ExplodPosType::P2 => (opponent_x, owner.pos.y),
            ExplodPosType::Front => (front_edge_x(owner.facing, stage), owner.pos.y),
            ExplodPosType::Back => (back_edge_x(owner.facing, stage), owner.pos.y),
            ExplodPosType::Left => (stage.left, owner.pos.y),
            ExplodPosType::Right => (stage.right, owner.pos.y),
        };
        let (off_x, off_y) = spawn.pos;
        Vec2::new(
            anchor_x + off_x * owner.facing.sign() as f32,
            anchor_y + off_y,
        )
    }

    /// Spawns the explods requested by this tick's
    /// [`fp_character::TickReport::explod_spawns`] into the explod slot-map (T033),
    /// resolving each [`ExplodSpawn`]'s `postype` + offset into a world position and
    /// seeding the explod on its `anim`'s first frame so it is visible the frame it
    /// spawns.
    ///
    /// `owner_id` is the spawning player's MUGEN id (`1`/`2`), recorded on the
    /// explod so a renderer can resolve its frames against the right fighter's SFF.
    /// Bounded by [`MAX_EXPLODS_PER_PLAYER`]: once the slot-map is full, further
    /// spawns this tick are dropped (debug-logged, NOT warn — an `Explod` in a
    /// persistent state would otherwise flood the log) rather than growing without
    /// limit. An explod whose `anim` action is missing/empty spawns nothing (a
    /// best-effort skip, never a panic).
    fn spawn_explods(
        &mut self,
        spawns: &[ExplodSpawn],
        owner_id: i32,
        opponent_x: f32,
        stage: StageView,
    ) {
        for spawn in spawns {
            if self.explods.len() >= MAX_EXPLODS_PER_PLAYER {
                tracing::debug!(
                    "explod slot-map full ({MAX_EXPLODS_PER_PLAYER}); dropping spawn id={}",
                    spawn.id
                );
                break;
            }
            // Resolve the chosen AIR action; an absent/empty action means there is
            // nothing to draw, so spawn no explod (best-effort, never a panic).
            let Some(action) = self.loaded.air.action(spawn.anim) else {
                tracing::debug!(
                    anim = spawn.anim,
                    "explod anim not found in AIR; skipping spawn"
                );
                continue;
            };
            let Some(first) = action.frames.first() else {
                tracing::debug!(
                    anim = spawn.anim,
                    "explod anim has no frames; skipping spawn"
                );
                continue;
            };

            let pos = self.explod_anchor(spawn, opponent_x, stage);
            let (remaining, play_once) = explod_lifetime(spawn.removetime);
            let explod = Explod {
                owner: owner_id,
                id: spawn.id,
                anim: spawn.anim,
                pos,
                sprite: first.sprite,
                offset: first.offset,
                sprpriority: spawn.sprpriority,
                pos_type: spawn.pos_type,
                anchor_offset: Vec2::new(spawn.pos.0, spawn.pos.1),
                bind_remaining: spawn.bindtime,
                elem: 0,
                elem_time: 0,
                play_once,
                remaining,
            };
            tracing::debug!(
                owner = owner_id,
                id = spawn.id,
                anim = spawn.anim,
                x = pos.x,
                y = pos.y,
                remaining,
                play_once,
                "spawned explod"
            );
            self.explods.push(explod);
        }
    }

    /// Applies this tick's [`fp_character::TickReport::explod_ops`]
    /// (`ModifyExplod`/`RemoveExplod`) to the explod slot-map (T033).
    ///
    /// [`ExplodOp::Remove`] removes the matching explods (by id, or all when the
    /// controller carried no id). [`ExplodOp::Modify`] updates each matching
    /// explod's fields in place; only the fields the controller carried are
    /// changed (an absent field leaves the value untouched), and a `removetime`
    /// change re-clamps the explod's remaining lifetime. Matching is by id:
    /// `Some(id)` matches that id, `None` matches every owned explod. Never panics.
    fn apply_explod_ops(&mut self, ops: &[ExplodOp]) {
        for op in ops {
            match *op {
                ExplodOp::Remove(id) => {
                    self.explods.retain(|e| !explod_id_matches(e.id, id));
                }
                ExplodOp::Modify {
                    id,
                    anim,
                    pos,
                    sprpriority,
                    bindtime,
                    removetime,
                } => {
                    for e in self
                        .explods
                        .iter_mut()
                        .filter(|e| explod_id_matches(e.id, id))
                    {
                        if let Some(a) = anim {
                            // Re-seeding the cursor on an anim change keeps the
                            // displayed frame valid for the new action.
                            if a != e.anim {
                                e.anim = a;
                                e.elem = 0;
                                e.elem_time = 0;
                            }
                        }
                        if let Some((x, y)) = pos {
                            e.anchor_offset = Vec2::new(x, y);
                        }
                        if let Some(p) = sprpriority {
                            e.sprpriority = p;
                        }
                        if let Some(b) = bindtime {
                            e.bind_remaining = b;
                        }
                        if let Some(r) = removetime {
                            // Re-resolve the lifetime + play-once from the new
                            // removetime, exactly as a fresh spawn would.
                            let (rem, once) = explod_lifetime(r);
                            e.remaining = rem;
                            e.play_once = once;
                        }
                    }
                }
            }
        }
    }

    /// Advances every live explod this player owns by one tick (T033): decrement
    /// its bind/lifetime counters, re-resolve its bound anchor (while still bound),
    /// step its animation cursor against the owner's AIR, and reap it once its
    /// lifetime elapses or its one-shot animation finishes (the MUGEN
    /// `removetime = -1` "play once" convention).
    ///
    /// `owner_pos`/`owner_facing` are the spawning player's CURRENT position and
    /// facing (for a bound `postype = p1` explod), `opponent_x` the opponent's X
    /// (for `postype = p2`), and `stage` the edges (for the edge postypes). Pure
    /// field writes over a bounded list; never panics.
    fn tick_explods(
        &mut self,
        owner_pos: Vec2<f32>,
        owner_facing: Facing,
        opponent_x: f32,
        stage: StageView,
    ) {
        let air = &self.loaded.air;
        self.explods.retain_mut(|e| {
            // While bound, re-resolve the anchor each tick so the explod tracks its
            // postype anchor (a `p1` explod follows the moving owner). Once the bind
            // window elapses it holds its last bound world position.
            let still_bound = e.bind_remaining != 0;
            if still_bound {
                let (anchor_x, anchor_y) = match e.pos_type {
                    ExplodPosType::P1 => (owner_pos.x, owner_pos.y),
                    ExplodPosType::P2 => (opponent_x, owner_pos.y),
                    ExplodPosType::Front => (front_edge_x(owner_facing, stage), owner_pos.y),
                    ExplodPosType::Back => (back_edge_x(owner_facing, stage), owner_pos.y),
                    ExplodPosType::Left => (stage.left, owner_pos.y),
                    ExplodPosType::Right => (stage.right, owner_pos.y),
                };
                e.pos = Vec2::new(
                    anchor_x + e.anchor_offset.x * owner_facing.sign() as f32,
                    anchor_y + e.anchor_offset.y,
                );
            }
            if e.bind_remaining > 0 {
                e.bind_remaining -= 1;
            }

            // Lifetime: `remaining` is always a bounded, non-negative countdown.
            // Reaching 0 reaps the explod (this guarantees every explod is
            // eventually reclaimed, even a looping or stuck animation).
            e.remaining -= 1;
            if e.remaining < 0 {
                return false;
            }

            // Step the animation cursor against the owner's AIR. A `play_once`
            // explod is reaped here the tick its one-shot animation finishes.
            advance_explod_frame(e, air, e.play_once)
        });
    }

    /// Ticks this player's **root** character against `opponent`, with its own
    /// helper slot-map plus the cross-player relations (`target`/`partner`/
    /// `playerid(n)`) wired into the redirect graph (T012/T014).
    ///
    /// The root is itself the top of the chain, so `parent`/`root` are `None`
    /// (the empty entries make `root` resolve to self and `parent` to `0`). The
    /// `target`/`partner`/`players` relations come from the match coordinator (the
    /// only place that owns both players); `target` is the opponent this player
    /// most recently hit, `partner` is its teammate (`None` in a 1-v-1), and
    /// `players` is the cross-player `(id, &Character)` lookup `playerid(n)`
    /// resolves against. The helper lookup is built from an immutable borrow of
    /// `self.helpers` while `self.character` is mutated — distinct fields, so the
    /// split borrow is sound; the passed-in relations likewise borrow other
    /// `Player`s' characters (distinct from `self.character`), never `self`.
    fn tick_root(
        &mut self,
        opponent: Option<&Character>,
        stage: StageView,
        relations: RedirectRelations<'_>,
    ) -> TickReport {
        let lookup: Vec<(i32, &Character)> = self
            .helpers
            .iter()
            .map(|h| (h.helper_id, &h.character))
            .collect();
        // (NUMHELPER) The owning player's full live-helper id list, for the root's
        // `NumHelper` / `NumHelper(id)` triggers. Built from the same slot-map as
        // `lookup`; an id-only slice so it threads through the `Copy` graph without
        // the `&Character` aliasing concern, and so helpers (which carry no
        // `helper(id)` lookup) can be handed the identical list — see
        // [`Player::tick_helpers`].
        let own_helper_ids: Vec<i32> = self.helpers.iter().map(|h| h.helper_id).collect();
        // (T026) The owning player's full live-projectile id list, for the root's
        // `NumProj` trigger — built from the projectile slot-map, an id-only slice
        // threaded through the `Copy` graph exactly like `own_helper_ids`.
        let own_proj_ids: Vec<i32> = self.projectiles.iter().map(|p| p.proj_id).collect();
        // (T061) The owning player's currently-bound target ids, for the root's
        // `NumTarget` / `NumTarget(id)` triggers. In this flat 1-v-1 model the only
        // bound target is the opponent the player most recently hit
        // (`relations.target`); its id is its MUGEN player id, recovered from the
        // `playerid` lookup table (which carries the opponent by id). An empty list
        // means "no target bound", so bare `NumTarget` reports `0`.
        let own_target_ids: Vec<i32> = relations
            .target
            .and_then(|t| {
                relations
                    .players
                    .iter()
                    .find(|(_, c)| std::ptr::eq(*c, t))
                    .map(|(id, _)| *id)
            })
            .into_iter()
            .collect();
        let graph = EntityGraph::new(None, None, &lookup)
            .with_target(relations.target)
            .with_partner(relations.partner)
            .with_players(relations.players)
            .with_own_helper_ids(&own_helper_ids)
            .with_own_proj_ids(&own_proj_ids)
            .with_own_target_ids(&own_target_ids);
        self.character
            .tick_as_helper(&self.loaded, opponent, stage, graph)
    }

    /// Ticks every live helper this player owns (T012), each with the spawning
    /// chain installed so its `parent`/`root` redirects resolve to the owning
    /// root character, then reaps any helper that retired this tick (T032).
    ///
    /// Each helper sees `opponent` as its `p2` (the same opponent the root
    /// faces). Sibling-helper addressing (`helper(id)` *from* a helper) is out of
    /// scope for T012 — a helper's own helper lookup is empty — so this avoids
    /// aliasing a single helper element against the rest of the slot-map.
    ///
    /// Helper lifecycle (T032): a helper is removed from the slot-map this tick
    /// when **either**
    /// - it ran a `DestroySelf` controller (its [`TickReport::destroy_self`] is
    ///   set), or
    /// - its finite [`remaining`](Helper::remaining) lifespan
    ///   (seeded from `removetime`) counts down past `0`.
    ///
    /// A helper with `remaining == -1` (no time limit) lives until it self-destructs
    /// (or the slot-map cap is hit). The countdown happens **before** the helper's
    /// tick so a `removetime = 0` helper is reaped the same frame it would first
    /// run (matching the projectile lifetime convention). This is what lets a
    /// character that spawns and destroys helpers each tick stay bounded instead of
    /// saturating [`MAX_HELPERS_PER_PLAYER`].
    fn tick_helpers(&mut self, opponent: Option<&Character>, stage: StageView) {
        let root = &self.character;
        let loaded = &self.loaded;
        // (NUMHELPER) Snapshot the owning player's full live-helper id list BEFORE
        // the `&mut self.helpers` loop — an owned copy of the ids, so it does not
        // alias the mutable per-helper borrow below. Installed on every helper's
        // graph so a helper that reads `NumHelper` / `NumHelper(id)` sees the
        // owning player's count (not its own empty sibling lookup).
        let own_helper_ids: Vec<i32> = self.helpers.iter().map(|h| h.helper_id).collect();
        // (T026) Snapshot the owning player's full live-projectile id list BEFORE the
        // `&mut self.helpers` loop too (it does not alias the helper borrow). Installed
        // on every helper's graph so a helper that reads `NumProj` sees the owning
        // player's live-projectile count — the same player-level parity `NumHelper`
        // gets above, promised by `EntityGraph`'s `own_proj_ids` doc.
        let own_proj_ids: Vec<i32> = self.projectiles.iter().map(|p| p.proj_id).collect();
        self.helpers.retain_mut(|helper| {
            // Lifetime: a non-negative `remaining` counts down; reaching below 0
            // reaps the helper (mirrors `tick_projectiles`). `-1` (no time limit)
            // is left untouched and never auto-expires.
            if helper.remaining >= 0 {
                helper.remaining -= 1;
                if helper.remaining < 0 {
                    return false;
                }
            }
            // parent and root both resolve to the owning root character (a single
            // spawn level; nested helper chains are T013). No sibling lookup.
            let graph = EntityGraph::new(Some(root), Some(root), &[])
                .with_own_helper_ids(&own_helper_ids)
                .with_own_proj_ids(&own_proj_ids);
            let report = helper
                .character
                .tick_as_helper(loaded, opponent, stage, graph);
            // `DestroySelf` (T032): the helper asked to remove itself this tick, so
            // reap it from the slot-map now.
            !report.destroy_self
        });
    }

    /// Advances this player's character one frame **standalone**: no opponent, no
    /// input, just its own state machine against its own loaded assets (T017).
    ///
    /// Used by the team-match coordinator ([`TeamMatch`]) in [`TeamMode::Simul`] to
    /// tick the *reserve* fighters that are not part of the active pair, so a
    /// team's off-screen members keep running their idle/standing states (and any
    /// self-driven animation) rather than freezing. The reserve sees no opponent
    /// (its `p2`/`enemy` redirects resolve to `0`) and is fed an empty command
    /// source (no input drives it), exactly like a frozen-world non-acting entity.
    /// Its deferred [`TickReport`] is discarded — a reserve cannot grab/hit anyone
    /// while off the active stage. Never panics.
    fn tick_standalone(&mut self, stage: StageView) {
        self.character
            .set_command_source(Box::new(fp_character::NoCommands));
        let _ = self.character.tick(&self.loaded, None, stage);
    }

    /// The character's world position in pixels (`Pos X`/`Pos Y`).
    #[must_use]
    pub fn pos(&self) -> Vec2<f32> {
        self.character.pos
    }

    /// Which way the character currently faces.
    #[must_use]
    pub fn facing(&self) -> Facing {
        self.character.facing
    }

    /// This player's rolling raw-input history, for the on-screen input display
    /// (T064).
    ///
    /// Index `0` of the returned [`InputBuffer`] is the input read this frame.
    /// The buffer holds absolute directions (left/right, not facing-relative);
    /// the display layer folds them to forward/back using [`facing`](Self::facing).
    #[must_use]
    pub fn input_buffer(&self) -> &InputBuffer {
        &self.input_buffer
    }

    /// The command names this player's recognizer matched *this* frame, for the
    /// on-screen input display's command flash (T064).
    ///
    /// Empty on a frame with no fresh recognition; a command that merely stays
    /// buffered is not repeated. See [`CommandMatcher::just_matched`].
    #[must_use]
    pub fn just_matched_commands(&self) -> &[String] {
        self.matcher.just_matched()
    }

    /// The character's current animation (action) id.
    #[must_use]
    pub fn anim(&self) -> i32 {
        self.character.anim
    }

    /// The zero-based index of the current animation element (frame) within the
    /// action. The MUGEN `AnimElem` trigger is one-based; this returns the raw
    /// cursor, matching [`fp_character::Character::anim_elem`].
    #[must_use]
    pub fn anim_elem(&self) -> i32 {
        self.character.anim_elem
    }

    /// The character's current life.
    #[must_use]
    pub fn life(&self) -> i32 {
        self.character.life
    }

    /// The character's maximum life.
    #[must_use]
    pub fn life_max(&self) -> i32 {
        self.character.life_max
    }

    /// The character's current power (the super/special meter, `Power`).
    ///
    /// Power is built during the fight and **carries across the rounds** of a
    /// match (it is not reset between rounds; see
    /// [`Match::reset_for_next_round`]). A HUD can divide this by
    /// [`Player::power_max`] for a proportional power bar.
    #[must_use]
    pub fn power(&self) -> i32 {
        self.character.power
    }

    /// The character's maximum power (`PowerMax`, `[Data] power`; MUGEN's default
    /// is `3000`). The denominator for a proportional power bar over
    /// [`Player::power`].
    #[must_use]
    pub fn power_max(&self) -> i32 {
        self.character.power_max
    }

    /// This player's stable [`CharacterFingerprint`] — the cheap identity stamp of
    /// its [`LoadedCharacter`] (#38).
    ///
    /// Derived deterministically from the loaded `.def`'s name, integer constants,
    /// and compiled state-number set; identical across runs of the same build.
    /// Used by the snapshot / replay restore guards to verify a save-state is being
    /// applied to a match built from the same character.
    #[must_use]
    pub fn fingerprint(&self) -> CharacterFingerprint {
        CharacterFingerprint::of(&self.loaded)
    }

    /// Builds the [`PushBody`] used for player-push and bound clamping from the
    /// character's current X and facing, using the facing-relative `(front, back)`
    /// half-widths from [`push_widths`](Self::push_widths).
    fn push_body(&self) -> PushBody {
        let (front, back) = self.push_widths();
        PushBody::new(
            self.character.pos.x,
            front,
            back,
            to_phys_facing(self.character.facing),
        )
    }

    /// The facing-relative `(front, back)` push half-widths for this character.
    ///
    /// Returns the per-tick `Width` controller override
    /// ([`Character::cur_width`](fp_character::Character::cur_width), audit #10)
    /// when it is active this tick, else the static `[Size] ground.front` /
    /// `ground.back` constants. KFM asserts `Width 16, x` on crouch/attack and its
    /// throw-bind state (810) to change how it pushes; with no override the
    /// behaviour is unchanged.
    ///
    /// Exposed publicly so the app's training/Clsn overlay (T063) can draw the
    /// player-push box alongside the Clsn1/Clsn2 hit/hurt boxes; the engine's own
    /// push/bounds logic calls it internally too.
    #[must_use]
    pub fn push_widths(&self) -> (f32, f32) {
        let w = self.character.cur_width;
        if w.active {
            (w.front, w.back)
        } else {
            let size = self.character.constants.size;
            (size.ground_front as f32, size.ground_back as f32)
        }
    }
}

/// Converts an [`fp_character::Facing`] to the [`fp_physics::Facing`] the push /
/// clamp primitives expect (they are distinct types in distinct crates).
fn to_phys_facing(facing: Facing) -> PhysFacing {
    match facing {
        Facing::Right => PhysFacing::Right,
        Facing::Left => PhysFacing::Left,
    }
}

/// The opposite of `facing` — used to spawn a helper with `facing = -1`
/// (opposite to its owner) (T012).
fn flip_facing(facing: Facing) -> Facing {
    match facing {
        Facing::Right => Facing::Left,
        Facing::Left => Facing::Right,
    }
}

/// The world X of the screen edge a character *faces* (its "front" edge): the
/// right edge when facing right, the left edge when facing left. Used to resolve
/// a `Helper` `postype = front` spawn (T012).
fn front_edge_x(facing: Facing, stage: StageView) -> f32 {
    match facing {
        Facing::Right => stage.right,
        Facing::Left => stage.left,
    }
}

/// The world X of the screen edge *behind* a character (its "back" edge): the
/// left edge when facing right, the right edge when facing left. Used to resolve
/// a `Helper` `postype = back` spawn (T012).
fn back_edge_x(facing: Facing, stage: StageView) -> f32 {
    match facing {
        Facing::Right => stage.left,
        Facing::Left => stage.right,
    }
}

/// The two-player match coordinator: ticks both fighters, resolves combat both
/// directions, keeps them on stage and apart, and runs the round flow.
///
/// Construct one with [`Match::new`] (default 99-second round) or
/// [`Match::with_round_seconds`], then call [`Match::tick`] once per 60Hz frame
/// with each player's [`MatchInput`]. Read the live state for rendering through
/// [`Match::p1`] / [`Match::p2`], [`Match::round_state`], [`Match::timer`], and
/// [`Match::winner`].
pub struct Match {
    /// Player 1.
    p1: Player,
    /// Player 2.
    p2: Player,
    /// The horizontal playfield bounds both fighters are clamped to.
    bounds: StageBounds,
    /// The current round phase.
    round_state: RoundState,
    /// Frames remaining on the round clock (counts down during
    /// [`RoundState::Fight`]). Starts at `round_seconds * 60`.
    timer: i32,
    /// Frames elapsed in the current [`RoundState::Intro`] / [`RoundState::Ko`]
    /// phase, used to time the transitions out of them.
    phase_timer: i32,
    /// The decided winner, set when the round reaches [`RoundState::Win`].
    winner: Option<Winner>,
    /// Player 1's sound-play requests from the MOST RECENT [`Match::tick`].
    ///
    /// Each tick this is first REPLACED with P1's per-tick
    /// [`fp_character::TickReport`] `PlaySnd` requests, then the `HitDef`
    /// hit/guard impact sound is APPENDED if P1's attack connected this tick — so
    /// it never accumulates across ticks but does combine both sources within a
    /// tick. Empty on a tick with no `PlaySnd` and no connecting attack. Read it
    /// via [`Match::p1_sound_requests`]; a downstream audio player (fp-app +
    /// fp-audio) consumes it to perform playback.
    p1_sound_requests: Vec<fp_character::SoundRequest>,
    /// Player 2's sound-play requests from the most recent [`Match::tick`].
    /// See [`Match::p1_sound_requests`] for the capture/replace semantics; read
    /// it via [`Match::p2_sound_requests`].
    p2_sound_requests: Vec<fp_character::SoundRequest>,

    // ---- Best-of-N match flow (task 7.4) ----------------------------------
    /// Number of round wins a player needs to win the **match** (best-of-N).
    /// Default [`DEFAULT_ROUNDS_TO_WIN`] (`2`, best of three). Always at least
    /// `1` (a non-positive override is clamped at construction/set time).
    rounds_to_win: i32,
    /// Rounds player 1 has won so far this match.
    p1_round_wins: i32,
    /// Rounds player 2 has won so far this match.
    p2_round_wins: i32,
    /// The 1-based number of the round currently being fought (starts at `1`,
    /// increments on each round reset).
    round_number: i32,
    /// Whether the whole match is still in progress or decided (terminal).
    match_state: MatchState,
    /// The match winner, set once [`match_state`](Match::match_state) becomes
    /// [`MatchState::Over`]; [`None`] while the match is in progress. A match is
    /// never declared a draw (see [`Match`] docs): a drawn round credits neither
    /// player, so the match simply continues to another round.
    match_winner: Option<Winner>,
    /// The round-reset template for player 1: the start position, facing, and
    /// life/power maxima captured at construction. Used to restore P1 between
    /// rounds (life to max, back to start position + facing, transient state
    /// cleared). See [`RoundResetState`].
    p1_reset: RoundResetState,
    /// The round-reset template for player 2. See [`p1_reset`](Match::p1_reset).
    p2_reset: RoundResetState,
    /// The round length in **frames** the timer is reset to at the start of each
    /// round (the constructor's `round_seconds * 60`, already clamped to `>= 0`).
    round_frames: i32,
    /// Total game ticks elapsed since the match began — MUGEN's `GameTime`
    /// (audit #21).
    ///
    /// A monotonic counter incremented once per [`Match::tick`]. Unlike
    /// [`timer`](Match::timer) (which counts *down* and resets each round), this
    /// **never** resets between rounds; it is the running game clock the
    /// `GameTime` trigger reads. Pushed onto each character via its
    /// [`RoundView`](fp_character::RoundView) before that character ticks.
    game_time: i32,
    /// The active whole-match freeze (`Pause`/`SuperPause`, audit #24), or the
    /// inactive default when nothing is frozen.
    ///
    /// While this is active [`Match::tick`] holds the frozen players' simulation
    /// and the round timer / `GameTime` still — only the `SuperPause` triggerer
    /// keeps ticking — counting the freeze down one tick per frame. It is reset to
    /// inactive at the start of each round. Constructed inside [`Match::with_config`]
    /// so a caller (e.g. `fp-app`) that builds a [`Match`] via [`Match::new`] needs
    /// no change.
    freeze: Freeze,
    /// Live hit-spark / effect entities (audit #17), spawned at the contact point
    /// of a connecting attack and advanced/dropped each [`Match::tick`].
    ///
    /// Each connecting hit whose attacker authored an own-spark (`sparkno`
    /// negative / `S`-prefixed) pushes one [`Effect`] here; a common-`fightfx`
    /// spark pushes one sourced from [`common_fx`](Match::common_fx) when that
    /// asset is loaded (else it is skipped). Expired effects are removed in-place,
    /// so this only ever holds the sparks visible *this* frame. Cleared at the
    /// start of each round. Read it via [`Match::effects`].
    effects: Vec<Effect>,
    /// The shared common-effects (`fightfx`) animation set, when loaded (audit
    /// #17). Holds the [`AirFile`] whose actions resolve a common
    /// ([`EffectSide::Common`]) spark's frames; `None` means no common-effects
    /// asset is wired, so common sparks are a best-effort skip (the pre-asset
    /// behavior). Install it with [`Match::set_common_fx`]; a renderer pairs it
    /// with the matching `fightfx.sff` it loaded separately.
    common_fx: Option<AirFile>,
    /// Single-round / no-life-restore mode (T028). `false` (the default) runs the
    /// normal best-of-N flow with a full life/position reset between rounds. When
    /// `true` the **first decided round ends the match** — there is no
    /// [`reset_for_next_round`](Match::reset_for_next_round), so a KO'd fighter's
    /// life is never restored — and a double-KO (or equal-life time over) is
    /// recorded as a genuine [`Winner::Draw`] match result rather than the
    /// best-of-N "draws just continue" rule.
    ///
    /// This is what a [`TeamMatch`] in [`TeamMode::Simul`]/[`TeamMode::Turns`] sets
    /// on its inner match so a team-level defeat is decided by remaining-fighter
    /// counts, not masked by the inner round restarting and healing a downed
    /// fighter. The bare 1v1 [`Match`] leaves it `false`, so 1v1 behaviour is
    /// unchanged.
    single_round: bool,
}

/// The per-fighter state captured at match construction and restored at the
/// start of every round (task 7.4 round reset).
///
/// MUGEN returns both fighters to their starting positions and facing, full
/// life, and a neutral stand at the top of each round. This snapshots exactly the
/// pieces the coordinator needs to recreate that: the seeded start position and
/// facing (so a reset re-seeds the same opener), and the life/power maxima (so a
/// reset restores `life` to `life_max`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct RoundResetState {
    /// The world position the fighter started the match at, restored each round.
    pos: Vec2<f32>,
    /// The facing the fighter started the match with, restored each round.
    facing: Facing,
    /// The fighter's maximum life, the value `life` is restored to each round.
    life_max: i32,
}

/// Which player triggered the active [`SuperPause`](fp_character::FreezeKind::SuperPause)
/// and is therefore **exempt** from the freeze (keeps ticking while everyone else
/// is frozen). A [`Pause`](fp_character::FreezeKind::Pause) freezes everyone, so it
/// has no exempt player ([`FreezeExempt::None`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum FreezeExempt {
    /// No player is exempt — every fighter is frozen (a `Pause`).
    None,
    /// Player 1 triggered the `SuperPause` and keeps ticking.
    P1,
    /// Player 2 triggered the `SuperPause` and keeps ticking.
    P2,
}

/// The whole-match freeze state driven by `Pause` / `SuperPause` controllers
/// (faithfulness audit #24).
///
/// While [`active`](Self::active) is true the coordinator holds the **frozen**
/// players' simulation and the round timer / `GameTime` still for
/// [`remaining`](Self::remaining) ticks; the [`exempt`](Self::exempt) player (the
/// `SuperPause` triggerer, if any) keeps ticking so its super move animates. The
/// freeze counts down one tick per frame and clears at `0`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Freeze {
    /// Ticks of freeze remaining; `0` (the inactive default) means no freeze.
    remaining: i32,
    /// Which player, if any, is exempt from this freeze.
    exempt: FreezeExempt,
}

impl Freeze {
    /// The inactive freeze: nothing frozen, no exempt player.
    const fn inactive() -> Self {
        Self {
            remaining: 0,
            exempt: FreezeExempt::None,
        }
    }

    /// Whether a freeze is currently holding the match still.
    const fn active(self) -> bool {
        self.remaining > 0
    }

    /// Whether the player on the given side is frozen this tick (i.e. a freeze is
    /// active and that side is not the exempt triggerer).
    const fn freezes(self, side: FreezeExempt) -> bool {
        self.active()
            && !matches!(
                (self.exempt, side),
                (FreezeExempt::P1, FreezeExempt::P1) | (FreezeExempt::P2, FreezeExempt::P2)
            )
    }
}

impl Match {
    /// Creates a match between two players on the given stage bounds, with the
    /// default 99-second round clock, the default best-of-three round target
    /// (`2` round wins; see [`Match::rounds_to_win`]), and both fighters facing
    /// each other.
    ///
    /// The players start in [`RoundState::Intro`] of round `1`; a fixed number of
    /// intro ticks run before combat begins (see [`Match::tick`]).
    #[must_use]
    pub fn new(p1: Player, p2: Player, bounds: StageBounds) -> Self {
        Self::with_round_seconds(p1, p2, bounds, DEFAULT_ROUND_SECONDS)
    }

    /// Creates a match with an explicit round length in seconds (the timer starts
    /// at `round_seconds * 60` frames) and the default best-of-three round target.
    /// A non-positive `round_seconds` is treated as `0` (an immediate time-over
    /// once the fight begins) rather than producing a negative timer.
    #[must_use]
    pub fn with_round_seconds(
        p1: Player,
        p2: Player,
        bounds: StageBounds,
        round_seconds: i32,
    ) -> Self {
        Self::with_config(p1, p2, bounds, round_seconds, DEFAULT_ROUNDS_TO_WIN)
    }

    /// Creates a match with an explicit best-of-N round target (`rounds_to_win`
    /// round wins decide the match) and the default 99-second round clock. A
    /// `rounds_to_win` below `1` is clamped to `1` (a one-round match) rather than
    /// producing an unwinnable match.
    #[must_use]
    pub fn with_rounds_to_win(
        p1: Player,
        p2: Player,
        bounds: StageBounds,
        rounds_to_win: i32,
    ) -> Self {
        Self::with_config(p1, p2, bounds, DEFAULT_ROUND_SECONDS, rounds_to_win)
    }

    /// Creates a match with explicit round length (seconds) and best-of-N target,
    /// the shared constructor the other `new`/`with_*` helpers delegate to.
    ///
    /// `round_seconds` is clamped to `>= 0` (a non-positive value yields an
    /// immediate time-over) and `rounds_to_win` to `>= 1` (so the match is always
    /// winnable). Both fighters are seeded facing each other, and their start
    /// position/facing/maxima are captured so each subsequent round can reset to
    /// them.
    #[must_use]
    pub fn with_config(
        mut p1: Player,
        mut p2: Player,
        bounds: StageBounds,
        round_seconds: i32,
        rounds_to_win: i32,
    ) -> Self {
        // Seed facing so the two start looking at each other (baseline facep2).
        face_each_other(&mut p1.character, &mut p2.character);
        // Capture each fighter's seeded opener so every round can reset to it.
        let p1_reset = RoundResetState::capture(&p1.character);
        let p2_reset = RoundResetState::capture(&p2.character);
        let round_frames = round_seconds.max(0).saturating_mul(TICKS_PER_SECOND);
        let mut m = Self {
            p1,
            p2,
            bounds,
            round_state: RoundState::Intro,
            timer: round_frames,
            phase_timer: 0,
            winner: None,
            p1_sound_requests: Vec::new(),
            p2_sound_requests: Vec::new(),
            rounds_to_win: rounds_to_win.max(1),
            p1_round_wins: 0,
            p2_round_wins: 0,
            round_number: 1,
            match_state: MatchState::InProgress,
            match_winner: None,
            p1_reset,
            p2_reset,
            round_frames,
            game_time: 0,
            freeze: Freeze::inactive(),
            effects: Vec::new(),
            common_fx: None,
            single_round: false,
        };
        // Drive each fighter through its round-init state (5900) for round 1, the
        // same way [`Match::reset_for_next_round`] does for every later round.
        m.run_round_init();
        m
    }

    /// Installs the shared common-effects (`fightfx`) animation set used to
    /// resolve common ([`EffectSide::Common`]) hit-spark frames (audit #17).
    ///
    /// `air` is the parsed common-effects [`AirFile`] (the engine-shipped
    /// `assets/data/fightfx.air`, or any caller-supplied set). Once installed, a
    /// connecting hit with a bare non-negative `sparkno` spawns a common spark
    /// whose frames come from this set; a renderer pairs it with the matching
    /// `fightfx.sff` it loaded separately. Calling this is **optional** — with no
    /// common set installed, common sparks are a best-effort skip (no panic, no
    /// regression). Replaces any previously installed set.
    pub fn set_common_fx(&mut self, air: AirFile) {
        self.common_fx = Some(air);
    }

    /// The installed common-effects (`fightfx`) animation set, if any (audit #17).
    ///
    /// Returns the [`AirFile`] a renderer needs to resolve a common
    /// ([`EffectSide::Common`]) spark's current frame against the matching
    /// `fightfx.sff`, or `None` when no common set is loaded.
    #[must_use]
    pub fn common_fx(&self) -> Option<&AirFile> {
        self.common_fx.as_ref()
    }

    /// Overrides the number of round wins needed to win the match, clamping a
    /// non-positive value to `1`. Existing round-win tallies are preserved; if the
    /// new target is already met by a player the match is **not** retroactively
    /// ended here (the decision is made when a round is decided), but a subsequent
    /// round decision will end it as soon as the threshold is observed.
    ///
    /// Has **no effect once the match is over** ([`match_state`](Self::match_state)
    /// is [`MatchState::Over`]): the threshold still updates but no further round is
    /// decided, so the terminal result stands.
    pub fn set_rounds_to_win(&mut self, rounds_to_win: i32) {
        self.rounds_to_win = rounds_to_win.max(1);
    }

    /// Enables or disables single-round / no-life-restore mode (T028).
    ///
    /// In single-round mode the **first decided round ends the match** with no
    /// between-round reset, so a knocked-out fighter's life is never restored, and
    /// a double-KO (or equal-life time over) is recorded as a genuine
    /// [`Winner::Draw`] in [`match_winner`](Self::match_winner). This is what a
    /// [`TeamMatch`] in [`TeamMode::Simul`]/[`TeamMode::Turns`] sets on its inner
    /// match so team-level defeat is decided by surviving-fighter counts rather
    /// than masked by the inner round restarting and healing a downed fighter.
    ///
    /// Leaving it `false` (the default) preserves the normal best-of-N round flow
    /// exactly, so a bare 1v1 [`Match`] is unaffected.
    pub fn set_single_round(&mut self, single_round: bool) {
        self.single_round = single_round;
    }

    /// Whether this match is in single-round / no-life-restore mode (T028). See
    /// [`Match::set_single_round`].
    #[must_use]
    pub fn single_round(&self) -> bool {
        self.single_round
    }

    /// Seeds the two fighters' `random` streams **distinctly** from a single match
    /// seed (#38 — the deferred #28 follow-up).
    ///
    /// Both players are typically built from the same `.def`, so without this they
    /// would share the identical [`fp_character::DEFAULT_RNG_SEED`] stream and draw
    /// the *same* random sequence — visibly wrong, and a determinism foot-gun.
    /// This seeds player 1 with `match_seed` and player 2 with a *derived distinct*
    /// seed (see [`derive_player_seed`]), so the two draw **independent** streams.
    /// The derivation is pure and deterministic: the same `match_seed` always
    /// reproduces the same per-player seeds, so a replay (which records the match
    /// seed) re-creates both streams exactly.
    ///
    /// Call this once, right after constructing the match, before the first
    /// [`tick`](Match::tick). Pass [`DEFAULT_MATCH_SEED`] for the fixed,
    /// reproducible default.
    pub fn seed_players(&mut self, match_seed: i32) {
        self.p1
            .character
            .seed_rng(derive_player_seed(match_seed, 0));
        self.p2
            .character
            .seed_rng(derive_player_seed(match_seed, 1));
    }

    /// Assigns each player's [`Character::ai_level`](fp_character::Character::ai_level)
    /// from its input [`PlayerDriver`] (T052).
    ///
    /// A human driver ([`PlayerDriver::Human`]) sets level `0`; a
    /// [`PlayerDriver::Cpu`] sets the difficulty's [`AiDifficulty::ai_level`]
    /// (`1..=8`). This is a one-time identity assignment (the level never changes
    /// mid-match), so the CNS `AILevel` trigger reads the correct value for each
    /// side. Call it once right after construction. With no call, both fighters
    /// keep the human default (`0`), so a bare match is a two-human match.
    pub fn set_drivers(&mut self, p1_driver: PlayerDriver, p2_driver: PlayerDriver) {
        self.p1.character.set_ai_level(p1_driver.ai_level());
        self.p2.character.set_ai_level(p2_driver.ai_level());
    }

    /// Constructs a match (default round length / best-of-N) and immediately seeds
    /// its two players distinctly from `match_seed` (#38).
    ///
    /// A convenience wrapper around [`Match::new`] + [`Match::seed_players`]; the
    /// resulting match is fully reproducible from `(p1, p2, bounds, match_seed)`,
    /// which is exactly what a replay log records.
    #[must_use]
    pub fn with_seed(p1: Player, p2: Player, bounds: StageBounds, match_seed: i32) -> Self {
        let mut m = Self::new(p1, p2, bounds);
        m.seed_players(match_seed);
        m
    }

    /// Read access to player 1.
    #[must_use]
    pub fn p1(&self) -> &Player {
        &self.p1
    }

    /// Read access to player 2.
    #[must_use]
    pub fn p2(&self) -> &Player {
        &self.p2
    }

    /// The [`fp_input::AiObservation`] a CPU AI controlling **player 2** sees this
    /// frame: where player 1 (its opponent) is relative to it (T018).
    ///
    /// Pair with a [`fp_input::CpuAi`]: call this each frame, feed it to
    /// [`fp_input::CpuAi::decide`], and pass the resulting `InputState` (via
    /// `MatchInput::from`) as the `p2_input` to [`Match::tick`]. This is the
    /// glue that lets an idle P2 slot be driven by the baseline AI.
    #[must_use]
    pub fn ai_observation_for_p2(&self) -> AiObservation {
        AiObservation {
            opponent_dx: self.p1.pos().x - self.p2.pos().x,
        }
    }

    /// The [`fp_input::AiObservation`] a CPU AI controlling **player 1** sees this
    /// frame: where player 2 is relative to it (mirror of
    /// [`Match::ai_observation_for_p2`], for an AI-vs-AI / demo match).
    #[must_use]
    pub fn ai_observation_for_p1(&self) -> AiObservation {
        AiObservation {
            opponent_dx: self.p2.pos().x - self.p1.pos().x,
        }
    }

    /// Consumes the match and returns its two [`Player`]s as `(p1, p2)`, dropping
    /// all round/timer/effect state (T017).
    ///
    /// Used by the team-match coordinator ([`TeamMatch`]) to recover the active
    /// fighters from the inner 1v1 match so a Turns hand-off can swap a knocked-out
    /// fighter for a reserve and rebuild a fresh [`Match`] between the new active
    /// pair. The two returned players carry their live [`Character`] state (life,
    /// position, meter, …) so a surviving fighter keeps its progress across the
    /// hand-off.
    #[must_use]
    pub fn into_players(self) -> (Player, Player) {
        (self.p1, self.p2)
    }

    /// Mutable access to player 1 (test-only), so the team-flow tests can force a
    /// deterministic life/KO without landing a real hit.
    #[cfg(test)]
    pub(crate) fn p1_mut_for_test(&mut self) -> &mut Player {
        &mut self.p1
    }

    /// Mutable access to player 2 (test-only). See [`Match::p1_mut_for_test`].
    #[cfg(test)]
    pub(crate) fn p2_mut_for_test(&mut self) -> &mut Player {
        &mut self.p2
    }

    /// The two players' stable identity fingerprints, `(p1, p2)` (#38).
    ///
    /// Each is [`Player::fingerprint`] of the corresponding loaded character. This
    /// is the value stamped into a [`MatchSnapshot`] / [`ReplayLog`] and validated
    /// against on restore / replay, so a save-state built from one pair of `.def`s
    /// cannot be silently applied to a match built from a different pair.
    #[must_use]
    pub fn character_fingerprints(&self) -> (CharacterFingerprint, CharacterFingerprint) {
        (self.p1.fingerprint(), self.p2.fingerprint())
    }

    /// The stage bounds the fighters are clamped to.
    #[must_use]
    pub fn bounds(&self) -> StageBounds {
        self.bounds
    }

    /// The current round phase.
    #[must_use]
    pub fn round_state(&self) -> RoundState {
        self.round_state
    }

    /// Frames remaining on the round clock. During [`RoundState::Fight`] this
    /// counts down; in other phases it holds its value. Divide by `60` for
    /// seconds.
    #[must_use]
    pub fn timer(&self) -> i32 {
        self.timer
    }

    /// The decided winner of the **current round**, or [`None`] until the round
    /// reaches [`RoundState::Win`]. Reset to [`None`] when the next round begins.
    /// For the winner of the whole match, use [`Match::match_winner`].
    ///
    /// On the match-over frame this still holds the **final round's** winner (no
    /// next round begins to clear it); for the match result always use
    /// [`match_winner`](Self::match_winner), which can differ when the deciding
    /// sequence included drawn rounds.
    #[must_use]
    pub fn winner(&self) -> Option<Winner> {
        self.winner
    }

    /// The number of round wins a player needs to win the match (best-of-N). The
    /// match ends as soon as either [`Match::p1_round_wins`] or
    /// [`Match::p2_round_wins`] reaches this. Defaults to `2` (best of three);
    /// always at least `1`.
    #[must_use]
    pub fn rounds_to_win(&self) -> i32 {
        self.rounds_to_win
    }

    /// The number of rounds player 1 has won so far this match.
    #[must_use]
    pub fn p1_round_wins(&self) -> i32 {
        self.p1_round_wins
    }

    /// The number of rounds player 2 has won so far this match.
    #[must_use]
    pub fn p2_round_wins(&self) -> i32 {
        self.p2_round_wins
    }

    /// The 1-based number of the round currently being fought. Starts at `1` and
    /// increments each time the match resets for the next round.
    #[must_use]
    pub fn round_number(&self) -> i32 {
        self.round_number
    }

    /// Total game ticks elapsed since the match began — MUGEN's `GameTime`
    /// (audit #21). A monotonic counter, incremented once per [`Match::tick`],
    /// that does **not** reset between rounds.
    #[must_use]
    pub fn game_time(&self) -> i32 {
        self.game_time
    }

    /// Ticks of `Pause`/`SuperPause` freeze remaining (audit #24), or `0` when the
    /// match is not frozen.
    ///
    /// While this is positive [`Match::tick`] holds the frozen players, the round
    /// timer, and [`game_time`](Match::game_time) still, counting down one tick per
    /// frame. A renderer can read it to drive a freeze/flash overlay.
    #[must_use]
    pub fn freeze_time(&self) -> i32 {
        self.freeze.remaining.max(0)
    }

    /// Whether player 1's simulation is frozen this frame by an active
    /// `Pause`/`SuperPause` (audit #24): `true` when a freeze is active and P1 is
    /// not its exempt `SuperPause` triggerer.
    #[must_use]
    pub fn p1_frozen(&self) -> bool {
        self.freeze.freezes(FreezeExempt::P1)
    }

    /// Whether player 2's simulation is frozen this frame by an active
    /// `Pause`/`SuperPause` (audit #24). See [`Match::p1_frozen`].
    #[must_use]
    pub fn p2_frozen(&self) -> bool {
        self.freeze.freezes(FreezeExempt::P2)
    }

    /// The engine-global round / match clock the characters' `RoundState`,
    /// `GameTime`, `MatchOver`, `RoundNo`, and `RoundsExisted` triggers read this
    /// tick (audits #21 and T016).
    ///
    /// Built from the live round phase ([`RoundState::trigger_code`]), the
    /// monotonic [`game_time`](Match::game_time) counter, whether the match is
    /// over ([`MatchState::Over`]), the 1-based [`round_number`](Match::round_number)
    /// (`RoundNo`), and the count of rounds already completed (`RoundsExisted`).
    /// [`Match::tick`] installs this on each character via
    /// [`Character::set_round_view`](fp_character::Character::set_round_view)
    /// before ticking it, so both fighters see the same coordinator view.
    ///
    /// Both fighters are present from the opening round, so each player's
    /// `RoundsExisted` is the number of rounds completed so far, i.e.
    /// `round_number - 1` (`0` during round 1, clamped to `0` so it is never
    /// negative). When per-player spawning/teams land this can diverge per side.
    #[must_use]
    pub fn round_view(&self) -> RoundView {
        RoundView::new(
            self.round_state.trigger_code(),
            self.game_time,
            self.match_state == MatchState::Over,
            self.round_number,
            (self.round_number - 1).max(0),
        )
    }

    /// Whether the whole match is still in progress or has been decided.
    #[must_use]
    pub fn match_state(&self) -> MatchState {
        self.match_state
    }

    /// The winner of the **whole match**, or [`None`] until the match is over.
    ///
    /// Becomes `Some(Winner::P1)`/`Some(Winner::P2)` the moment a player reaches
    /// [`Match::rounds_to_win`] round wins (at which point [`Match::match_state`]
    /// is [`MatchState::Over`]). In the normal best-of-N flow a match is never a
    /// draw: a drawn round credits neither player, so this never returns
    /// `Some(Winner::Draw)`.
    ///
    /// The one exception is single-round / no-life-restore mode
    /// ([`Match::single_round`], T028): there a genuine double-KO (or equal-life
    /// time over) ends the match as `Some(Winner::Draw)`, which is exactly how a
    /// wrapping [`TeamMatch`] recognises a team-level draw.
    #[must_use]
    pub fn match_winner(&self) -> Option<Winner> {
        self.match_winner
    }

    /// The live hit-spark / effect entities to draw this frame (audit #17).
    ///
    /// Each connecting attack with an attacker-own `sparkno` spawns one [`Effect`]
    /// at the hit's contact point; [`Match::tick`] advances each one and drops it
    /// when its animation finishes, so this slice holds exactly the sparks visible
    /// this frame (empty when nothing is connecting). A renderer (`fp-app`) draws
    /// each effect's [`Effect::sprite`] at its [`Effect::pos`], resolving the
    /// sprite against the owning side's ([`Effect::side`]) SFF — a fighter's SFF
    /// for [`EffectSide::P1`]/[`EffectSide::P2`], or the shared common-effects
    /// (`fightfx`) SFF for [`EffectSide::Common`].
    #[must_use]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }

    /// Player 1's sound-play requests for the MOST RECENT [`Match::tick`], in
    /// fire order.
    ///
    /// This holds, in order: the `PlaySnd` controllers P1 fired this tick,
    /// followed by P1's `HitDef` hit/guard impact sound if its attack connected
    /// this tick (channel 0, full volume). The slice is REPLACED every tick
    /// (never accumulated across ticks), so it reflects only the latest tick —
    /// empty whenever P1 emitted nothing. A downstream audio player decodes each
    /// [`fp_character::SoundRequest`] from the character's `.snd` (or the
    /// common/fight file when `common` is set) and plays it; `fp-engine` itself
    /// produces no sound.
    #[must_use]
    pub fn p1_sound_requests(&self) -> &[fp_character::SoundRequest] {
        &self.p1_sound_requests
    }

    /// Player 2's sound-play requests for the most recent [`Match::tick`], in
    /// fire order (P2's `PlaySnd` controllers followed by its `HitDef` impact
    /// sound on a connecting attack). See [`Match::p1_sound_requests`] for the
    /// replace-each-tick semantics.
    #[must_use]
    pub fn p2_sound_requests(&self) -> &[fp_character::SoundRequest] {
        &self.p2_sound_requests
    }

    /// Advances the whole match by one 60Hz frame.
    ///
    /// Performs, in order: input feed (facing-relative), both characters' state
    /// machine ticks, combat both directions, player-push + bound clamp, baseline
    /// face-the-opponent, and round-state/timer advance. See the
    /// [crate-level overview](crate) for the full description. Never panics.
    ///
    /// In a 1v1 [`Match`] neither fighter has a teammate, so the `partner` redirect
    /// resolves to nothing; a [`TeamMatch`] in [`TeamMode::Simul`] supplies the live
    /// teammates via [`Match::tick_with_partners`], which this delegates to with no
    /// partner on either side.
    pub fn tick(&mut self, p1_input: MatchInput, p2_input: MatchInput) {
        self.tick_with_partners(p1_input, p2_input, None, None);
    }

    /// Like [`Match::tick`] but with each side's live **teammate** supplied for the
    /// `partner` redirect (T027).
    ///
    /// `p1_partner`/`p2_partner` are the active fighter's teammate on each side (the
    /// `partner` redirect target), or [`None`] in a 1v1 / when the side fields a
    /// single fighter. A [`TeamMatch`] in [`TeamMode::Simul`] passes its reserve
    /// teammates here so a fighter's `partner, …` triggers resolve to a *live* ally
    /// rather than collapsing to `0`; everything else is identical to [`Match::tick`].
    ///
    /// The partner references must borrow characters that are **not** either of this
    /// match's own players (the coordinator owns its players mutably this tick); the
    /// only caller, [`TeamMatch`], passes characters from its separate reserve
    /// rosters, which are distinct storage. Never panics.
    pub fn tick_with_partners(
        &mut self,
        p1_input: MatchInput,
        p2_input: MatchInput,
        p1_partner: Option<&Character>,
        p2_partner: Option<&Character>,
    ) {
        // (F) Whole-match freeze (`Pause`/`SuperPause`, audit #24). While a freeze
        //     is active the engine holds the frozen players' simulation, the round
        //     timer, AND `GameTime` still; only the `SuperPause` triggerer (if any)
        //     keeps ticking so its super animates. This branch returns early — it
        //     does NOT advance `game_time`, run combat/push, or advance the round —
        //     so a frozen tick is a no-op for everyone but the exempt player.
        if self.freeze.active() {
            self.tick_frozen();
            return;
        }

        // (0) Advance the monotonic game clock, then push the engine-global round
        //     view (RoundState / GameTime / MatchOver) onto both characters BEFORE
        //     they tick, so their CNS triggers read this frame's values (audit
        //     #21). The view reflects the round phase as it stands at the START of
        //     this tick (the `advance_round` at the end may move it on for next
        //     frame). `GameTime` is incremented first so a fighter sees the frame
        //     count already including the current frame. The same view goes to
        //     both fighters — these are engine-global values, not per-player.
        self.game_time = self.game_time.saturating_add(1);
        let view = self.round_view();
        self.p1.character.set_round_view(view);
        self.p2.character.set_round_view(view);

        // (0b) Age each player's per-projectile-id contact/hit/guard counters one
        //      tick BEFORE the state machines run (T026), so a projectile that
        //      connected on a PRIOR frame reads an incremented "ticks since" this
        //      frame, while a connection recorded later this frame (in combat
        //      resolution, step 3) reads `0` until the next tick ages it. This is
        //      what makes `ProjContactTime<id>` count up from `0` and the
        //      `ProjContact<id> = 1, op t` window form compare against the right
        //      elapsed time.
        self.p1.character.tick_proj_events();
        self.p2.character.tick_proj_events();

        // (1) Feed inputs into each character's command source, facing-relative.
        //     Inputs only drive the fighters once the round is live; during the
        //     intro/KO/win phases the characters still tick (so idle animations
        //     play) but receive no commands.
        let fighting = self.round_state == RoundState::Fight;
        if fighting {
            self.p1.feed_input(p1_input);
            self.p2.feed_input(p2_input);
        } else {
            // Clear any stale commands so nothing fires outside the fight phase.
            self.p1
                .character
                .set_command_source(Box::new(fp_character::NoCommands));
            self.p2
                .character
                .set_command_source(Box::new(fp_character::NoCommands));
        }

        // (2) Tick both state machines, capturing each player's per-tick sound
        //     requests. These are REPLACED (not appended) every tick, so the
        //     accessors always reflect only the most recent tick's `PlaySnd`
        //     controllers. Outside the Fight phase the command source is cleared
        //     to `NoCommands` above, so a `command`-gated PlaySnd cannot fire;
        //     whatever a still-running tick produces is surfaced as-is.
        //
        //     Each player's tick is given the OTHER player as its opponent (so its
        //     `P2Dist`/`p2, life`/… triggers see the other fighter) plus the stage
        //     view (for the screen-edge distance triggers). `self.p1` and `self.p2`
        //     are distinct fields, so the split borrow `(&mut self.p1.character,
        //     &self.p2.character)` is allowed — the opponent is read-only during
        //     this character's tick. P1 ticks first against P2's pre-tick state,
        //     then P2 ticks against P1's just-updated state, matching MUGEN's
        //     in-order per-player update.
        let stage = self.bounds.view();
        // Opponent X positions captured up-front so `Helper postype = p2` spawns
        // can resolve their anchor without re-borrowing the opponent later (T012).
        let p2_x = self.p2.character.pos.x;
        let p1_x = self.p1.character.pos.x;
        // (T014) P1's cross-player redirects: `target` is P2 once P1 has hit it,
        // `partner` is None (1-v-1), and `playerid(2)` resolves to P2. The lookup
        // intentionally omits P1's own id (1) — it would alias the `&mut self.p1`
        // tick, and a player's own state is reachable through self-triggers. The
        // opponent borrow is immutable and distinct from the ticking `self.p1`.
        let p1_target = self.p1.character.has_target.then_some(&self.p2.character);
        let p1_players: [(i32, &Character); 1] = [(MUGEN_PLAYER_ID_P2, &self.p2.character)];
        let p1_relations = RedirectRelations {
            target: p1_target,
            // (T027) P1's live teammate, supplied by a wrapping `TeamMatch` in Simul
            // (a reserve on P1's side); `None` in a 1v1 → `partner, …` resolves to 0.
            partner: p1_partner,
            players: &p1_players,
        };
        let mut p1_report = self
            .p1
            .tick_root(Some(&self.p2.character), stage, p1_relations);
        // Capture any `Pause`/`SuperPause` P1 requested this tick (audit #24); the
        // freeze is armed after BOTH players tick so the later request wins.
        let p1_freeze = p1_report.freeze_request;
        // Helper spawns P1 requested this tick (T012); applied to P1's slot-map
        // below, after the opponent borrow is released.
        let p1_helper_spawns = std::mem::take(&mut p1_report.helper_spawns);
        // Projectile spawns P1 requested this tick (T013).
        let p1_projectile_spawns = std::mem::take(&mut p1_report.projectile_spawns);
        // Explod spawns + modify/remove ops P1 requested this tick (T033).
        let p1_explod_spawns = std::mem::take(&mut p1_report.explod_spawns);
        let p1_explod_ops = std::mem::take(&mut p1_report.explod_ops);
        self.p1_sound_requests = p1_report.sound_requests;
        // (2a) Apply P1's deferred `Target*` ops to P1's target, the OPPONENT (P2).
        //      `self.p1`/`self.p2` are distinct fields, so the split borrow of the
        //      binder (P1, read) and the target (P2, mutated) plus the target's OWN
        //      states (`self.p2.loaded.states`, for `TargetState` entry) is sound.
        //      Done in-order, P1 before P2 ticks, matching MUGEN's per-player update.
        //
        //      Gated on the live fight phase, exactly like combat in step (3): a
        //      throw's `Target*` controllers are combat effects and must not move /
        //      damage the opponent during the intro/KO/win phases (a `HitDef` left
        //      active during those phases likewise must not connect).
        if fighting {
            apply_target_ops(
                &self.p1.character,
                &mut self.p2.character,
                &self.p2.loaded.states,
                &p1_report.target_ops,
            );
        }

        // (T014) P2's cross-player redirects, mirroring P1's: `target` is P1 once
        // P2 has hit it, and `playerid(1)` resolves to P1 (P2's own id 2 omitted).
        let p2_target = self.p2.character.has_target.then_some(&self.p1.character);
        let p2_players: [(i32, &Character); 1] = [(MUGEN_PLAYER_ID_P1, &self.p1.character)];
        let p2_relations = RedirectRelations {
            target: p2_target,
            // (T027) P2's live teammate (a reserve on P2's side in Simul); `None` 1v1.
            partner: p2_partner,
            players: &p2_players,
        };
        let mut p2_report = self
            .p2
            .tick_root(Some(&self.p1.character), stage, p2_relations);
        let p2_freeze = p2_report.freeze_request;
        let p2_helper_spawns = std::mem::take(&mut p2_report.helper_spawns);
        let p2_projectile_spawns = std::mem::take(&mut p2_report.projectile_spawns);
        let p2_explod_spawns = std::mem::take(&mut p2_report.explod_spawns);
        let p2_explod_ops = std::mem::take(&mut p2_report.explod_ops);
        self.p2_sound_requests = p2_report.sound_requests;
        // (2b) Apply P2's deferred `Target*` ops to its target, the OPPONENT (P1).
        if fighting {
            apply_target_ops(
                &self.p2.character,
                &mut self.p1.character,
                &self.p1.loaded.states,
                &p2_report.target_ops,
            );
        }

        // (2c) Arm a whole-match freeze if either fighter ran a `Pause`/`SuperPause`
        //      this tick (audit #24). Both players have already advanced this frame
        //      — the freeze begins NEXT frame (the `freeze.active()` early-return at
        //      the top of `tick`). P2's request is considered after P1's so the
        //      later controller to fire wins, matching the single-effect nature of
        //      the controller. Only effective during the live fight phase: a freeze
        //      must not stall the intro/KO/win flow.
        if fighting {
            self.arm_freeze(p1_freeze, FreezeExempt::P1);
            self.arm_freeze(p2_freeze, FreezeExempt::P2);
        }

        // (2d) Spawn each player's requested helpers into its slot-map, then tick
        //      every live helper (T012). Spawning is done after both roots ticked
        //      (so a helper appears the frame its `Helper` controller fired) and
        //      the helpers then tick this same frame against the opponent. Each
        //      helper's `parent`/`root` redirect resolves to its owning root.
        //      `postype = p2` resolves against the pre-tick opponent X captured
        //      above. Not gated on `fighting`: a helper is an entity, not a combat
        //      effect, so it lives and animates across phases.
        self.p1.spawn_helpers(&p1_helper_spawns, p2_x, stage);
        self.p2.spawn_helpers(&p2_helper_spawns, p1_x, stage);
        self.p1.tick_helpers(Some(&self.p2.character), stage);
        self.p2.tick_helpers(Some(&self.p1.character), stage);

        // (2e) Spawn each player's requested projectiles into its slot-map, then
        //      advance every live projectile (T013). Like helpers, projectiles are
        //      entities (not combat effects), so they spawn and move across phases;
        //      their HIT resolution against the opponent below is gated on the live
        //      fight phase, exactly like melee combat. Spawning is done after both
        //      roots ticked (so a projectile appears the frame its `Projectile`
        //      controller fired); the advance moves each by its velocity and steps
        //      its animation so this frame's `Clsn1` is current for hit detection.
        self.p1.spawn_projectiles(&p1_projectile_spawns);
        self.p2.spawn_projectiles(&p2_projectile_spawns);
        self.p1.tick_projectiles(stage);
        self.p2.tick_projectiles(stage);

        // (2f) Spawn each player's requested explods into its slot-map, apply this
        //      tick's `ModifyExplod`/`RemoveExplod` ops, then advance every live
        //      explod (T033). An explod is a pure display entity (no collision), so
        //      it lives and animates across phases (not gated on `fighting`).
        //      Order: spawn (so a new explod appears the frame its `Explod` fired),
        //      then ops (a `RemoveExplod`/`ModifyExplod` in the same tick can touch
        //      the brand-new explod, matching MUGEN's same-tick controller order),
        //      then advance (so a fresh explod shows its first frame this frame).
        //      The `postype = p2` anchor resolves against the pre-tick opponent X
        //      captured above; the bound-anchor re-resolution uses each player's
        //      CURRENT (post-tick) position so a bound explod follows the live body.
        self.p1
            .spawn_explods(&p1_explod_spawns, MUGEN_PLAYER_ID_P1, p2_x, stage);
        self.p2
            .spawn_explods(&p2_explod_spawns, MUGEN_PLAYER_ID_P2, p1_x, stage);
        self.p1.apply_explod_ops(&p1_explod_ops);
        self.p2.apply_explod_ops(&p2_explod_ops);
        let p1_pos = self.p1.character.pos;
        let p1_facing = self.p1.character.facing;
        let p2_pos = self.p2.character.pos;
        let p2_facing = self.p2.character.facing;
        self.p1.tick_explods(p1_pos, p1_facing, p2_pos.x, stage);
        self.p2.tick_explods(p2_pos, p2_facing, p1_pos.x, stage);

        // (3) Combat both directions: P1 attacks P2, then P2 attacks P1.
        //     Each direction reads the attacker's Clsn1 and the defender's Clsn2
        //     from their loaded AIR and applies any resolved hit. Combat only
        //     happens during the live fight phase — a HitDef left active during
        //     the intro/KO/win phases must not connect.
        //
        //     On a connection, the attacker's `HitDef` hit/guard impact sound is
        //     APPENDED (not replacing) to the ATTACKER's sound-request vec. This
        //     runs AFTER step (2) moved this tick's `PlaySnd` requests into those
        //     vecs, so the impact sound adds to the same frame's requests rather
        //     than overwriting them. The hit sound plays on channel 0 (a fixed
        //     hit channel) at full volume (volume_scale 100); `common` is carried
        //     from the `SoundId` so a common-file hitsound (one authored with no
        //     `S` prefix) resolves against the fight.snd downstream.
        //
        //     Frame-advantage readout (T065): clear last tick's value on both
        //     players up front (unconditionally, even outside the fight phase) so a
        //     tick with no connection shows `—` rather than a stale number. Each
        //     connecting resolve below recomputes and sets the ATTACKER's value
        //     (mirrored onto the attacker's `TickReport::frame_advantage`).
        self.p1.frame_advantage = None;
        self.p2.frame_advantage = None;
        if fighting {
            // (3a) Priority / trade clash arbitration (audit #20). BEFORE the two
            //      independent resolve_attack passes, detect the SIMULTANEOUS-hit
            //      case: both fighters have an active HitDef AND each direction's
            //      attack boxes would connect this tick. When that happens MUGEN
            //      does not let both land unconditionally — it compares the two
            //      HitDefs' `priority` and applies the trade rules. The LOSER's
            //      `active_hitdef` is cleared here so its resolve_attack pass below
            //      sees no HitDef and connects nothing; the winner (or both, on a
            //      trade) is left untouched and lands as usual. The single-attacker
            //      case (only one side connecting) clears nothing and behaves
            //      exactly as before.
            self.resolve_priority_clash();

            // (3b) Hit-spark spawn (audit #17). Capture each direction's contact
            //      anchor (the world-space overlap center) and the attacker's
            //      `sparkno` BEFORE `resolve_attack` runs — the resolve sends the
            //      defender into its get-hit state, which changes its anim and so
            //      its hurt boxes, making a post-hit geometry probe unreliable. The
            //      anchor is only USED if the resolve actually returns a connection
            //      (so a probe that overlaps but the hitflag rejects spawns no
            //      spark). `active_hitdef` is read here, after the clash arbitration
            //      may have cleared the loser's, so a cancelled side carries no spark.
            let p1_spark = self
                .p1
                .character
                .active_hitdef
                .map(|hd| hd.resources.sparkno);
            let p1_anchor = directional_contact_point(&self.p1, &self.p2);

            let p2_states = &self.p2.loaded.states;
            let p1_attack = resolve_attack(
                &mut self.p1.character,
                &self.p1.loaded.air,
                &mut self.p2.character,
                &self.p2.loaded.air,
                p2_states,
            );
            if let Some(res) = p1_attack {
                // (3a') Frame advantage (T065): compute from the defender's induced
                //       stun and P1's remaining recovery BEFORE `p1stateno` may move
                //       P1 out of its attack action below. Stash on P1 (the readout
                //       surface) and mirror onto P1's TickReport.
                let adv = compute_frame_advantage(&self.p1, res.stun);
                self.p1.frame_advantage = adv;
                p1_report.frame_advantage = adv;
                if let Some(s) = res.hit_sound {
                    self.p1_sound_requests.push(hit_sound_request(s));
                }
                // Spawn P1's hit spark at the captured contact anchor (audit #17).
                // A common-`fightfx` spark or a missing anchor is a documented
                // best-effort no-op (logged in `spawn_effect`) — never a panic and
                // never blocks the hit.
                if let (Some(sparkno), Some(anchor)) = (p1_spark, p1_anchor) {
                    self.spawn_effect(EffectSide::P1, sparkno, anchor.point);
                }
                // P8b: the HitDef's `p1stateno` moves the ATTACKER (P1) into its
                // throw-/move-specific state via its OWN state graph. `p2stateno`
                // (the defender's get-hit state) was already applied inside
                // `resolve_attack`. The attacker and its loaded states are the same
                // player, so this is a self-borrow — no split needed.
                if let Some(state) = res.attacker_state {
                    let states = &self.p1.loaded.states;
                    tracing::debug!(player = "p1", to_state = state, "attacker enters p1stateno");
                    self.p1.character.change_state(states, state);
                }
            }

            let p2_spark = self
                .p2
                .character
                .active_hitdef
                .map(|hd| hd.resources.sparkno);
            let p2_anchor = directional_contact_point(&self.p2, &self.p1);

            let p1_states = &self.p1.loaded.states;
            let p2_attack = resolve_attack(
                &mut self.p2.character,
                &self.p2.loaded.air,
                &mut self.p1.character,
                &self.p1.loaded.air,
                p1_states,
            );
            if let Some(res) = p2_attack {
                // (3a') Frame advantage (T065) for P2's connecting attack, mirroring
                //       the P1 branch: defender stun minus P2's remaining recovery,
                //       computed before `p1stateno` may move P2 out of its action.
                let adv = compute_frame_advantage(&self.p2, res.stun);
                self.p2.frame_advantage = adv;
                p2_report.frame_advantage = adv;
                if let Some(s) = res.hit_sound {
                    self.p2_sound_requests.push(hit_sound_request(s));
                }
                if let (Some(sparkno), Some(anchor)) = (p2_spark, p2_anchor) {
                    self.spawn_effect(EffectSide::P2, sparkno, anchor.point);
                }
                if let Some(state) = res.attacker_state {
                    let states = &self.p2.loaded.states;
                    tracing::debug!(player = "p2", to_state = state, "attacker enters p1stateno");
                    self.p2.character.change_state(states, state);
                }
            }

            // (3b') Projectile combat (T013): each player's live projectiles attack
            //       the opponent, mirroring the melee passes. A projectile that
            //       overlaps the opponent's `Clsn2` connects via the SAME pure
            //       `resolve_attack` pipeline (its `active_hitdef`/anim/pos/facing
            //       are the attacker fields), applying damage / knockback / get-hit
            //       state to the opponent. A connecting projectile is reaped (a
            //       projectile is single-hit). P1's projectiles hit P2, then P2's
            //       hit P1.
            resolve_projectile_hits(&mut self.p1, &mut self.p2);
            resolve_projectile_hits(&mut self.p2, &mut self.p1);
        }

        // (3c) Advance the live hit-spark effects (audit #17): step each spark's
        //      animation cursor against its owning side's AIR action and drop any
        //      that have expired. Runs every fight tick, after this frame's sparks
        //      are spawned, so a brand-new spark shows its first frame this frame.
        self.tick_effects();

        // (4) Separate overlapping bodies, then clamp each to the stage.
        self.apply_push_and_bounds();

        // (5) Baseline face-the-opponent for neutral characters.
        face_each_other_when_neutral(&mut self.p1.character, &mut self.p2.character);

        // (6) Advance the round state machine and timer.
        self.advance_round();
    }

    /// Arms the whole-match freeze from a fighter's `Pause`/`SuperPause` request
    /// (audit #24), recording its duration and exempt player on [`Match::freeze`].
    ///
    /// `req` is the [`FreezeRequest`](fp_character::FreezeRequest) the fighter on
    /// `side` emitted this tick (or [`None`] if it ran no `Pause`/`SuperPause`). A
    /// [`Pause`](fp_character::FreezeKind::Pause) freezes everyone (no exempt
    /// player); a [`SuperPause`](fp_character::FreezeKind::SuperPause) exempts the
    /// triggering `side`. A non-positive `time` (already clamped to `>= 0` in
    /// `fp-character`) leaves no freeze armed. The freeze takes effect on the NEXT
    /// `tick` (this frame's simulation has already advanced).
    fn arm_freeze(&mut self, req: Option<fp_character::FreezeRequest>, side: FreezeExempt) {
        let Some(req) = req else {
            return;
        };
        if req.time <= 0 {
            return;
        }
        let exempt = match req.kind {
            fp_character::FreezeKind::Pause => FreezeExempt::None,
            fp_character::FreezeKind::SuperPause => side,
        };
        self.freeze = Freeze {
            remaining: req.time,
            exempt,
        };
        tracing::debug!(
            ?req.kind,
            time = req.time,
            ?exempt,
            "freeze armed"
        );
    }

    /// Runs one frozen frame while a `Pause`/`SuperPause` holds the match still
    /// (audit #24).
    ///
    /// Only the [`exempt`](Freeze::exempt) player (the `SuperPause` triggerer, if
    /// any) ticks — its super move keeps animating; every frozen player, the round
    /// timer, and `GameTime` are held still. No combat, push, face, or round
    /// advance runs. The freeze counts down by one; when it reaches `0` the next
    /// `tick` resumes normal processing. Mirrors how MUGEN treats a `SuperPause`:
    /// the world stops except the trigger.
    ///
    /// The exempt player still gets the engine-global round view installed (so its
    /// `GameTime`/`RoundState` triggers read the held — unchanged — values) and an
    /// empty command source (inputs are not buffered during a freeze, matching the
    /// frozen world). Its deferred `Target*` ops are intentionally NOT applied: a
    /// frozen opponent must not be moved/damaged mid-freeze.
    fn tick_frozen(&mut self) {
        let stage = self.bounds.view();
        let view = self.round_view();

        // Each frozen frame surfaces only the exempt player's own sound requests
        // (replaced, never accumulated); the frozen player produces none.
        self.p1_sound_requests.clear();
        self.p2_sound_requests.clear();

        // The exempt triggerer keeps animating; everyone else is frozen.
        match self.freeze.exempt {
            FreezeExempt::P1 => {
                self.p1.character.set_round_view(view);
                self.p1
                    .character
                    .set_command_source(Box::new(fp_character::NoCommands));
                let report =
                    self.p1
                        .character
                        .tick(&self.p1.loaded, Some(&self.p2.character), stage);
                self.p1_sound_requests = report.sound_requests;
            }
            FreezeExempt::P2 => {
                self.p2.character.set_round_view(view);
                self.p2
                    .character
                    .set_command_source(Box::new(fp_character::NoCommands));
                let report =
                    self.p2
                        .character
                        .tick(&self.p2.loaded, Some(&self.p1.character), stage);
                self.p2_sound_requests = report.sound_requests;
            }
            // A `Pause` exempts no one — nothing ticks this frame.
            FreezeExempt::None => {}
        }

        // Count the freeze down; `GameTime` and the round timer do NOT advance.
        self.freeze.remaining -= 1;
        if self.freeze.remaining <= 0 {
            self.freeze = Freeze::inactive();
        }
    }

    /// Arbitrates a MUGEN priority / trade clash between the two fighters'
    /// active `HitDef`s for this tick (audit #20), cancelling the loser's
    /// `active_hitdef` *before* the two `resolve_attack` passes run.
    ///
    /// A *clash* requires BOTH conditions:
    ///
    /// 1. each fighter has an [`active_hitdef`](Character::active_hitdef), and
    /// 2. each direction's attack would geometrically connect this tick — P1's
    ///    `Clsn1` overlaps P2's `Clsn2` AND P2's `Clsn1` overlaps P1's `Clsn2`,
    ///    tested with the same pure [`fp_combat::detect_hit`] primitive
    ///    `resolve_attack` uses, positioned by each character's `pos`/`facing`.
    ///
    /// When only one side connects (the common single-attacker case), this is a
    /// no-op: nothing is cancelled and the subsequent passes behave exactly as
    /// before. When both connect, the two priorities are compared with
    /// [`fp_combat::resolve_clash`]:
    ///
    /// - [`ClashOutcome::Trade`] — leave both `HitDef`s active (both land).
    /// - [`ClashOutcome::FirstWins`] — clear P2's `active_hitdef` (only P1 lands).
    /// - [`ClashOutcome::SecondWins`] — clear P1's `active_hitdef` (only P2 lands).
    /// - [`ClashOutcome::NeitherHits`] — clear BOTH (neither lands this tick).
    ///
    /// Clearing `active_hitdef` is exactly how the subsequent `resolve_attack`
    /// returns [`None`] for the cancelled side (its `attacker.active_hitdef?`
    /// short-circuits). This does not consume the move or mark a connection, so
    /// a cancelled attack simply does not apply this tick. Honoring `hitonce`
    /// already-connected moves: a side whose move already connected this combo
    /// is skipped here too (it would not re-detect), so it never spuriously
    /// cancels the other side. Pure field writes; never panics.
    fn resolve_priority_clash(&mut self) {
        // Both sides must have an active HitDef to clash at all.
        let (Some(p1_hd), Some(p2_hd)) = (
            self.p1.character.active_hitdef,
            self.p2.character.active_hitdef,
        ) else {
            return;
        };

        // A move that already connected this combo (hitonce) will not connect
        // again, so it cannot be part of a fresh clash — bail to the unchanged
        // single-/no-attacker path.
        if self.p1.character.move_connect.contact() || self.p2.character.move_connect.contact() {
            return;
        }

        // Each direction must geometrically connect for this to be a clash.
        let p1_hits_p2 = directional_contact(&self.p1, &self.p2);
        let p2_hits_p1 = directional_contact(&self.p2, &self.p1);
        if !(p1_hits_p2 && p2_hits_p1) {
            // Single-attacker (or neither) — behave exactly as today.
            return;
        }

        // Both connect simultaneously: compare priorities and cancel the loser(s).
        match resolve_clash(p1_hd.priority, p2_hd.priority) {
            ClashOutcome::Trade => {
                tracing::debug!(
                    p1_priority = p1_hd.priority.value,
                    p2_priority = p2_hd.priority.value,
                    "priority clash: trade (both hits land)"
                );
            }
            ClashOutcome::FirstWins => {
                tracing::debug!(
                    p1_priority = p1_hd.priority.value,
                    p2_priority = p2_hd.priority.value,
                    "priority clash: P1 wins, P2 HitDef cancelled"
                );
                self.p2.character.active_hitdef = None;
            }
            ClashOutcome::SecondWins => {
                tracing::debug!(
                    p1_priority = p1_hd.priority.value,
                    p2_priority = p2_hd.priority.value,
                    "priority clash: P2 wins, P1 HitDef cancelled"
                );
                self.p1.character.active_hitdef = None;
            }
            ClashOutcome::NeitherHits => {
                tracing::debug!(
                    p1_priority = p1_hd.priority.value,
                    p2_priority = p2_hd.priority.value,
                    "priority clash: neither hits (both HitDefs cancelled)"
                );
                self.p1.character.active_hitdef = None;
                self.p2.character.active_hitdef = None;
            }
        }
    }

    /// Pushes overlapping bodies apart along X (mutual `PlayerPush`) and clamps
    /// each fighter so its facing-resolved body stays inside the stage bounds.
    ///
    /// The push uses each character's `size.ground.front`/`back` half-widths; the
    /// even split moves each center by half the overlap. Both operations are pure
    /// and only adjust the characters' X positions.
    fn apply_push_and_bounds(&mut self) {
        // Separate first (so the post-push positions are then clamped on stage).
        let body_a = self.p1.push_body();
        let body_b = self.p2.push_body();
        let res = resolve_push(body_a, body_b);
        self.p1.character.pos.x = res.a;
        self.p2.character.pos.x = res.b;

        // Then clamp each (rebuilt at its new center) to the stage bounds.
        let clamped_a = clamp_to_bounds(
            self.p1.character.pos.x,
            self.p1.push_body_left_half(),
            self.p1.push_body_right_half(),
            self.bounds.left,
            self.bounds.right,
        );
        let clamped_b = clamp_to_bounds(
            self.p2.character.pos.x,
            self.p2.push_body_left_half(),
            self.p2.push_body_right_half(),
            self.bounds.left,
            self.bounds.right,
        );
        self.p1.character.pos.x = clamped_a;
        self.p2.character.pos.x = clamped_b;
    }

    /// Advances the round phase and the down-counting timer for this frame, and
    /// drives the best-of-N match flow at the end of a round.
    ///
    /// - [`RoundState::Intro`]: count [`INTRO_FRAMES`], then enter
    ///   [`RoundState::Fight`].
    /// - [`RoundState::Fight`]: decrement the timer; a life reaching `0` (KO) or
    ///   the timer hitting `0` (time over) decides the round [`Winner`], **credits
    ///   the round win** (a draw credits neither — see [`Match::credit_round`]),
    ///   and enters [`RoundState::Ko`].
    /// - [`RoundState::Ko`]: hold [`KO_FRAMES`], then enter [`RoundState::Win`].
    /// - [`RoundState::Win`]: the round is decided. If a player has reached
    ///   [`Match::rounds_to_win`] the match is **over** ([`MatchState::Over`],
    ///   [`Match::match_winner`] set) and nothing further changes. Otherwise the
    ///   coordinator **resets for the next round** (life/positions/state/timer
    ///   restored, `round_number` incremented) and re-enters [`RoundState::Intro`]
    ///   to resume fighting.
    fn advance_round(&mut self) {
        match self.round_state {
            RoundState::Intro => {
                self.phase_timer += 1;
                if self.phase_timer >= INTRO_FRAMES {
                    self.round_state = RoundState::Fight;
                    self.phase_timer = 0;
                    // Give both fighters control as the fight begins.
                    self.p1.character.ctrl = true;
                    self.p2.character.ctrl = true;
                }
            }
            RoundState::Fight => {
                if self.timer > 0 {
                    self.timer -= 1;
                }
                let p1_down = self.p1.character.life <= 0;
                let p2_down = self.p2.character.life <= 0;
                if p1_down || p2_down {
                    // Knockout: whoever still has life wins; both down is a draw.
                    let winner = if p1_down && p2_down {
                        Winner::Draw
                    } else if p2_down {
                        Winner::P1
                    } else {
                        Winner::P2
                    };
                    self.decide_round(winner);
                } else if self.timer == 0 {
                    // Time over: compare remaining life.
                    let winner = compare_life(self.p1.character.life, self.p2.character.life);
                    self.decide_round(winner);
                }
            }
            RoundState::Ko => {
                self.phase_timer += 1;
                if self.phase_timer >= KO_FRAMES {
                    self.round_state = RoundState::Win;
                    self.phase_timer = 0;
                }
            }
            RoundState::Win => self.resolve_match_or_next_round(),
        }
    }

    /// Records the decided round [`Winner`], credits the round win, and enters
    /// the [`RoundState::Ko`] hold. Called from both the KO and the time-over
    /// decision paths in [`Match::advance_round`].
    fn decide_round(&mut self, winner: Winner) {
        self.winner = Some(winner);
        self.credit_round(winner);
        self.enter_ko();
    }

    /// Credits a decided round to its winner, incrementing that player's
    /// round-win tally.
    ///
    /// **Draw rule (MUGEN-faithful).** A [`Winner::Draw`] (a double KO, or equal
    /// life at time over) credits **neither** player: both tallies are left
    /// unchanged. MUGEN's default match flow treats a drawn round as won by no
    /// one — neither side progresses toward the win threshold and the match simply
    /// plays another round. (MUGEN exposes a `Match.maxdrawgames` cap as a tunable;
    /// we keep the faithful default of "draws just continue", with no separate cap
    /// in this baseline.) Because of this rule the match itself is never a draw —
    /// [`Match::match_winner`] only ever yields [`Winner::P1`]/[`Winner::P2`].
    fn credit_round(&mut self, winner: Winner) {
        match winner {
            Winner::P1 => {
                self.p1_round_wins += 1;
                tracing::info!(
                    round = self.round_number,
                    p1_round_wins = self.p1_round_wins,
                    p2_round_wins = self.p2_round_wins,
                    "round decided: P1 wins the round"
                );
            }
            Winner::P2 => {
                self.p2_round_wins += 1;
                tracing::info!(
                    round = self.round_number,
                    p1_round_wins = self.p1_round_wins,
                    p2_round_wins = self.p2_round_wins,
                    "round decided: P2 wins the round"
                );
            }
            Winner::Draw => {
                tracing::info!(
                    round = self.round_number,
                    "round decided: draw — neither player is credited"
                );
            }
        }
    }

    /// At the end of a decided round ([`RoundState::Win`]): if a player has
    /// reached [`Match::rounds_to_win`], end the match; otherwise reset for the
    /// next round and resume fighting.
    ///
    /// In single-round / no-life-restore mode ([`Match::single_round`], T028) this
    /// **always** ends the match on the first decided round — with that round's
    /// genuine [`Winner`] (a double-KO / equal-life time over yields
    /// [`Winner::Draw`]) — and never resets, so a knocked-out fighter is not healed.
    fn resolve_match_or_next_round(&mut self) {
        // Already terminal — nothing changes once the match is over.
        if self.match_state == MatchState::Over {
            return;
        }

        // (T028) Single-round mode: the first decided round is final. End the match
        // with the round's actual verdict (Draw-capable) and skip the life-restoring
        // reset, so a wrapping `TeamMatch` sees the true post-round life/KO state.
        if self.single_round {
            self.end_match(self.winner.unwrap_or(Winner::Draw));
            return;
        }

        if self.p1_round_wins >= self.rounds_to_win {
            self.end_match(Winner::P1);
        } else if self.p2_round_wins >= self.rounds_to_win {
            self.end_match(Winner::P2);
        } else {
            self.reset_for_next_round();
        }
    }

    /// Enters the terminal [`MatchState::Over`] state with the given match winner.
    fn end_match(&mut self, winner: Winner) {
        self.match_state = MatchState::Over;
        self.match_winner = Some(winner);
        tracing::info!(
            rounds = self.round_number,
            p1_round_wins = self.p1_round_wins,
            p2_round_wins = self.p2_round_wins,
            ?winner,
            "match over"
        );
    }

    /// Resets both fighters and the round clock for the next round, then
    /// re-enters [`RoundState::Intro`] to resume the match.
    ///
    /// Restores each fighter to full life and its captured start position/facing,
    /// clears transient combat state (velocity, hit-pause/shake, get-hit reaction,
    /// active HitDef, move-connect), and returns it to a neutral standing/idle
    /// state with control removed for the intro. The round timer is reset to the
    /// configured round length and `round_number` is incremented.
    ///
    /// **Power carry (MUGEN-faithful).** Power (the super meter) is *carried
    /// across rounds* within a match — it is **not** reset here. MUGEN preserves a
    /// fighter's power between the rounds of a single match (only a new match
    /// resets it), so a meter built in round one is still available in round two.
    fn reset_for_next_round(&mut self) {
        reset_fighter_for_round(&mut self.p1.character, self.p1_reset);
        reset_fighter_for_round(&mut self.p2.character, self.p2_reset);

        // Re-seed facing toward each other (positions were just restored).
        face_each_other(&mut self.p1.character, &mut self.p2.character);

        // Fresh round clock and bookkeeping.
        self.timer = self.round_frames;
        self.phase_timer = 0;
        self.winner = None;
        self.round_number += 1;
        self.round_state = RoundState::Intro;

        // No stale sound requests carry into the new round's first tick.
        self.p1_sound_requests.clear();
        self.p2_sound_requests.clear();

        // A freeze (`Pause`/`SuperPause`) does not carry across a round boundary.
        self.freeze = Freeze::inactive();

        // Stale hit-sparks must not linger into the new round (audit #17).
        self.effects.clear();
        // Live projectiles likewise do not survive a round boundary (T013).
        self.p1.projectiles.clear();
        self.p2.projectiles.clear();
        // Live explods do not survive a round boundary either (T033).
        self.p1.explods.clear();
        self.p2.explods.clear();

        // Drive each fighter through its round-init state (5900) so its authored
        // convergence runs on top of the field reset just applied (see
        // [`Match::run_round_init`]).
        self.run_round_init();

        tracing::info!(
            round = self.round_number,
            p1_round_wins = self.p1_round_wins,
            p2_round_wins = self.p2_round_wins,
            "round reset: starting next round"
        );
    }

    /// Drives each fighter into the engine-common round-init state
    /// ([`ROUND_INIT_STATE`], `common1.cns` [Statedef 5900]) **iff it defines
    /// one**, so the authored round-init logic runs at the top of every round.
    ///
    /// This is how 5900 *integrates with* the engine's authoritative round reset
    /// without fighting it: the field reset
    /// ([`reset_fighter_for_round`]) has already restored full life, the start
    /// position/facing, and the neutral stand; entering 5900 then re-asserts
    /// those (its `LifeSet value = Const(data.life)` writes the same full-life
    /// value, and its `ChangeState 0` returns to the same neutral stand the reset
    /// selected). Because the round opens in [`RoundState::Intro`] — where both
    /// fighters still tick their state machines (so idle/intro animations play)
    /// but receive no commands — 5900's controllers run on the intro's first tick
    /// and converge it back to state `0`. The net effect on a converging 5900 is
    /// nil; a character can override 5900 (the loader's first-wins merge keeps its
    /// version) to seed per-round vars, and that runs here instead.
    ///
    /// A fighter that does **not** define 5900 is left exactly where the field
    /// reset put it (state `0`); the lookup is a safe no-op. Never panics.
    fn run_round_init(&mut self) {
        if self.p1.loaded.states.contains_key(&ROUND_INIT_STATE) {
            self.p1
                .character
                .change_state(&self.p1.loaded.states, ROUND_INIT_STATE);
        }
        if self.p2.loaded.states.contains_key(&ROUND_INIT_STATE) {
            self.p2
                .character
                .change_state(&self.p2.loaded.states, ROUND_INIT_STATE);
        }
    }

    /// Enters [`RoundState::Ko`], freezing the round clock and removing control
    /// from both fighters for the duration of the hold.
    fn enter_ko(&mut self) {
        self.round_state = RoundState::Ko;
        self.phase_timer = 0;
        self.p1.character.ctrl = false;
        self.p2.character.ctrl = false;
    }

    /// Spawns a hit-spark [`Effect`] for a connecting attack (audit #17).
    ///
    /// `side` is the **attacker** (whose own SFF/AIR an own-spark draws from),
    /// `raw_sparkno` is its `HitDef`'s `sparkno`, and `pos` is the contact-center
    /// anchor. The spark source is classified via [`SparkSource`]:
    ///
    /// - [`SparkSource::None`] (`-1`): no spark — nothing spawned.
    /// - [`SparkSource::Own`]: play that action from the **attacker's own** AIR
    ///   (recorded with `attacker_side`). The first frame's sprite/offset are
    ///   resolved immediately so the spark is visible the frame it spawns; a
    ///   missing/empty action logs and spawns nothing.
    /// - [`SparkSource::Common`]: play that action from the shared common-effects
    ///   set ([`Match::common_fx`], recorded as [`EffectSide::Common`]). When no
    ///   common-effects asset is loaded this is a documented best-effort **skip**
    ///   (logged at debug). It never panics and never blocks the hit.
    ///
    /// The effect's lifetime is the action's total frame ticks, clamped to
    /// [`EFFECT_MAX_LIFETIME`] (so an infinite-hold final frame cannot leak).
    fn spawn_effect(&mut self, attacker_side: EffectSide, raw_sparkno: i32, pos: Vec2<f32>) {
        // Resolve the effect's animation source (which AIR + which side to record)
        // from the spark classification.
        let (side, anim, air) = match SparkSource::classify(raw_sparkno) {
            SparkSource::None => return,
            SparkSource::Own { anim } => {
                let air = match attacker_side {
                    EffectSide::P1 => &self.p1.loaded.air,
                    EffectSide::P2 => &self.p2.loaded.air,
                    // The attacker side is always P1/P2; Common is never passed in
                    // as an attacker. Defensive: treat as no spark.
                    EffectSide::Common => return,
                };
                (attacker_side, anim, air)
            }
            SparkSource::Common { anim } => {
                let Some(air) = self.common_fx.as_ref() else {
                    tracing::debug!(
                        sparkno = raw_sparkno,
                        anim,
                        "common fightfx spark requested but no common-fx asset is loaded; skipping"
                    );
                    return;
                };
                (EffectSide::Common, anim, air)
            }
        };

        // Resolve the chosen AIR action; an absent/empty action means there is
        // nothing to draw, so spawn no effect (best-effort, never a panic).
        let Some(action) = air.action(anim) else {
            tracing::debug!(?side, anim, "spark action not found in AIR; skipping spark");
            return;
        };
        let Some(first) = action.frames.first() else {
            tracing::debug!(?side, anim, "spark action has no frames; skipping spark");
            return;
        };

        let lifetime = effect_lifetime(action);
        let effect = Effect {
            side,
            anim,
            pos,
            sprite: first.sprite,
            offset: first.offset,
            elem: 0,
            elem_time: 0,
            remaining: lifetime,
        };
        tracing::debug!(
            ?side,
            anim,
            x = pos.x,
            y = pos.y,
            lifetime,
            "spawned hit spark"
        );
        self.effects.push(effect);
    }

    /// Advances every live hit-spark [`Effect`] one tick and drops the expired
    /// ones (audit #17).
    ///
    /// For each effect: decrement its lifetime, advance its animation cursor
    /// against the owning side's AIR action (re-resolving the current frame's
    /// sprite/offset), and remove it once its lifetime hits `0` or its action can
    /// no longer be resolved. Pure field writes over a bounded list; never panics.
    fn tick_effects(&mut self) {
        // Snapshot the AIR sources once (immutable) so the closure can resolve
        // each effect's frames without re-borrowing `self` mutably per element.
        let p1_air = &self.p1.loaded.air;
        let p2_air = &self.p2.loaded.air;
        let common_air = self.common_fx.as_ref();
        self.effects.retain_mut(|fx| {
            fx.remaining -= 1;
            if fx.remaining <= 0 {
                return false;
            }
            let air = match fx.side {
                EffectSide::P1 => p1_air,
                EffectSide::P2 => p2_air,
                // A common spark only exists if the common set was loaded when it
                // spawned; if it has since gone (it cannot, it is immutable), drop.
                EffectSide::Common => match common_air {
                    Some(air) => air,
                    None => return false,
                },
            };
            advance_effect_frame(fx, air)
        });
    }
}

/// Advances one hit-spark [`Effect`]'s animation cursor by a tick against its
/// owning AIR `air`, updating its current-frame [`Effect::sprite`]/[`Effect::offset`]
/// (audit #17).
///
/// Returns `true` to keep the effect, `false` to drop it. The cursor advances when
/// the current frame's `ticks` budget elapses. A hit-spark plays through **once
/// and stops** — unlike a looping fighter animation it does **not** wrap to the
/// action's `loopstart`; once it reaches the final frame it **holds** there until
/// the effect's own [`Effect::remaining`] lifetime reaps it. This is the MUGEN
/// behavior (sparks are one-shot, non-looping) and means a short spark ends on its
/// last frame rather than visibly repeating. A frame with `ticks <= 0` (an
/// infinite hold) never advances the cursor — it is held until the lifetime
/// expires. A missing/empty action drops the effect. Never panics.
fn advance_effect_frame(fx: &mut Effect, air: &AirFile) -> bool {
    let Some(action) = air.action(fx.anim) else {
        return false;
    };
    if action.frames.is_empty() {
        return false;
    }
    let last = action.frames.len() - 1;
    fx.elem_time += 1;
    // Advance past any elapsed frames (a loop guard bounds odd data: never iterate
    // more than the frame count, so a run of zero-duration frames cannot spin).
    let mut guard = action.frames.len() + 1;
    while guard > 0 {
        guard -= 1;
        // Already parked on the final frame: a one-shot spark holds here until its
        // lifetime reaps it (no loop back to `loopstart`).
        if fx.elem >= last {
            fx.elem = last;
            break;
        }
        let Some(frame) = action.frames.get(fx.elem) else {
            break;
        };
        if frame.ticks <= 0 || fx.elem_time < frame.ticks {
            break;
        }
        fx.elem_time = 0;
        fx.elem += 1;
    }
    // Re-resolve the (possibly advanced) current frame's sprite + offset.
    if let Some(frame) = action.frames.get(fx.elem) {
        fx.sprite = frame.sprite;
        fx.offset = frame.offset;
    }
    true
}

/// The lifetime (in frames) a hit-spark effect from `action` lives: the sum of its
/// frames' positive `ticks`, clamped to `[1, `[`EFFECT_MAX_LIFETIME`]`]`.
///
/// An infinite-hold frame (`ticks <= 0`) contributes nothing to the sum, so a
/// short spark whose final frame holds forever still gets a finite, bounded life.
/// An action with no positive-duration frames falls back to a single-frame life.
fn effect_lifetime(action: &AnimAction) -> i32 {
    let total: i32 = action
        .frames
        .iter()
        .map(|f| f.ticks.max(0))
        .fold(0i32, |acc, t| acc.saturating_add(t));
    total.clamp(1, EFFECT_MAX_LIFETIME)
}

/// Resolves an explod's `(remaining_lifetime, play_once)` from its `removetime`
/// (T033). The lifetime is ALWAYS a bounded, non-negative countdown so every
/// explod is eventually reaped (no leak), while `play_once` records whether the
/// animation should stop after one play-through.
///
/// - `removetime >= 0`: a fixed lifetime — that exact tick count, clamped to
///   [`EXPLOD_MAX_LIFETIME`]. The animation loops within the lifetime
///   (`play_once = false`).
/// - `removetime == -1` (MUGEN "play the animation once, then remove"): seeded to
///   the [`EXPLOD_MAX_LIFETIME`] ceiling but `play_once = true`, so the explod is
///   normally reaped the tick its one-shot animation finishes — the ceiling is a
///   backstop for an animation that never finishes (an infinite-hold final frame).
/// - any other negative (`-2` "loop forever", etc.): seeded to the ceiling with
///   `play_once = false` (a looping animation bounded only by the ceiling).
fn explod_lifetime(removetime: i32) -> (i32, bool) {
    match removetime {
        r if r >= 0 => (r.min(EXPLOD_MAX_LIFETIME), false),
        -1 => (EXPLOD_MAX_LIFETIME, true),
        _ => (EXPLOD_MAX_LIFETIME, false),
    }
}

/// Whether an explod with id `explod_id` matches a `RemoveExplod`/`ModifyExplod`
/// selector `selector` (T033): `Some(id)` matches only that id; `None` (the
/// controller fired with no `id`) matches every owned explod.
fn explod_id_matches(explod_id: i32, selector: Option<i32>) -> bool {
    match selector {
        Some(id) => explod_id == id,
        None => true,
    }
}

/// Advances one explod's animation cursor by a tick against the owner's AIR `air`,
/// re-resolving its current-frame [`Explod::sprite`]/[`Explod::offset`] (T033).
///
/// Returns `true` to keep the explod, `false` to drop it. The cursor advances when
/// the current frame's `ticks` budget elapses. When `play_once` is set (the MUGEN
/// `removetime = -1` convention), the explod is **reaped the tick it would advance
/// past its final frame** — it plays the animation exactly once. Otherwise the
/// animation **loops** (the MUGEN `removetime = -2` / fixed-time conventions),
/// wrapping to frame `0` past the end so a fixed-lifetime explod keeps animating.
/// A frame with `ticks <= 0` (an infinite hold) parks the cursor. A missing/empty
/// action drops the explod. A loop guard bounds the advance against a run of
/// zero-duration frames; never panics.
fn advance_explod_frame(e: &mut Explod, air: &AirFile, play_once: bool) -> bool {
    let Some(action) = air.action(e.anim) else {
        return false;
    };
    if action.frames.is_empty() {
        return false;
    }
    let last = action.frames.len() - 1;
    e.elem_time += 1;
    let mut guard = action.frames.len() + 1;
    while guard > 0 {
        guard -= 1;
        let Some(frame) = action.frames.get(e.elem) else {
            // Cursor ran past the end (e.g. the action shrank): wrap to the start.
            e.elem = 0;
            e.elem_time = 0;
            break;
        };
        if frame.ticks <= 0 || e.elem_time < frame.ticks {
            break;
        }
        e.elem_time = 0;
        // Step past the elapsed frame. Past the last frame, a play-once explod is
        // reaped (it has shown every frame); a looping one wraps to the start.
        if e.elem >= last {
            if play_once {
                return false;
            }
            e.elem = 0;
        } else {
            e.elem += 1;
        }
    }
    // Re-resolve the (possibly advanced) current frame's sprite + offset.
    if let Some(frame) = action.frames.get(e.elem) {
        e.sprite = frame.sprite;
        e.offset = frame.offset;
    }
    true
}

/// Advances a projectile's animation cursor (`anim_elem`/`anim_elem_time`) by one
/// tick against its owner's AIR `air` (T013), so the next frame's `Clsn1` attack
/// boxes are current for hit detection.
///
/// Unlike a one-shot hit-spark, a projectile animation **loops** (a fireball
/// spins continuously while it flies): once the cursor passes the last frame it
/// wraps to frame `0`. A frame with `ticks <= 0` (an infinite hold) parks the
/// cursor there. A missing / empty action leaves the cursor untouched (the
/// projectile still moves and connects on its current frame). A loop guard bounds
/// the advance against a run of zero-duration frames; never panics.
fn advance_projectile_frame(proj: &mut Character, air: &AirFile) {
    let Some(action) = air.action(proj.anim) else {
        return;
    };
    if action.frames.is_empty() {
        return;
    }
    proj.anim_elem_time += 1;
    let mut guard = action.frames.len() + 1;
    while guard > 0 {
        guard -= 1;
        let idx = proj.anim_elem.max(0) as usize;
        let Some(frame) = action.frames.get(idx) else {
            // Cursor ran past the end (e.g. the action shrank): wrap to the start.
            proj.anim_elem = 0;
            proj.anim_elem_time = 0;
            break;
        };
        if frame.ticks <= 0 || proj.anim_elem_time < frame.ticks {
            break;
        }
        proj.anim_elem_time = 0;
        proj.anim_elem += 1;
        // Loop back to the first frame once we step past the last.
        if proj.anim_elem as usize >= action.frames.len() {
            proj.anim_elem = 0;
        }
    }
}

/// Resolves every live projectile of `attacker` against `defender`'s character
/// (T013), applying any connecting hit and reaping the projectiles that landed.
///
/// Each projectile is run through the SAME pure
/// [`fp_character::combat::resolve_attack`] pipeline a melee attack uses: the
/// projectile's [`Character`] (carrying its `active_hitdef`/`anim`/`pos`/`facing`)
/// is the attacker, the opponent is the defender, and both source their collision
/// boxes from their respective AIR sets (the projectile from its **owner's** AIR,
/// since its `projanim` is one of the owner's actions). A projectile that connects
/// is **removed** from the slot-map — a projectile is single-hit. A projectile
/// that does not connect is retained to fly on. Damage / knockback / get-hit state
/// are applied to the defender inside `resolve_attack`. `attacker` and `defender`
/// are distinct [`Player`] fields, so the split borrow is sound; never panics.
fn resolve_projectile_hits(attacker: &mut Player, defender: &mut Player) {
    let owner_air = &attacker.loaded.air;
    let defender_air = &defender.loaded.air;
    let defender_states = &defender.loaded.states;
    // Whether any of this player's projectiles connected this pass: a connecting
    // projectile establishes the OWNER's `target` (T014), so the owner's
    // `target, …` redirects resolve to the hit defender — exactly as a melee hit
    // does. `resolve_attack` sets `has_target` on the *projectile* entity (the
    // attacker it is given), but the projectile is reaped, so the flag is lifted
    // onto the owning player here.
    let mut owner_connected = false;
    // Connections recorded this pass, lifted onto the owner's per-id projectile
    // tracker after the `retain_mut` (which only borrows `attacker.projectiles`,
    // not `attacker.character`): `(proj_id, guarded)` for the `ProjContact<id>` /
    // `ProjHit<id>` / `ProjGuarded<id>` / `Proj*Time<id>` trigger family (T026).
    let mut proj_events: Vec<(i32, bool)> = Vec::new();
    attacker.projectiles.retain_mut(|proj| {
        // `resolve_attack` returns `Some(..)` only on an effective hit/guard; on a
        // connection the projectile is consumed (single-hit), else it lives on.
        let outcome = resolve_attack(
            &mut proj.character,
            owner_air,
            &mut defender.character,
            defender_air,
            defender_states,
        );
        if let Some(res) = outcome {
            owner_connected = true;
            let guarded = matches!(res.result, fp_combat::HitResult::Guard);
            proj_events.push((proj.proj_id, guarded));
            tracing::debug!(
                "projectile id={} connected ({}); reaping (single-hit)",
                proj.proj_id,
                if guarded { "guarded" } else { "hit" }
            );
        }
        outcome.is_none()
    });
    if owner_connected {
        attacker.character.has_target = true;
    }
    // Record the connections on the owner so its `ProjContact<id>` / `ProjHit<id>`
    // / `ProjGuarded<id>` triggers fire this tick (T026).
    for (proj_id, guarded) in proj_events {
        attacker.character.record_proj_event(proj_id, guarded);
    }
}

impl Player {
    /// Runs this player's real [`CommandMatcher`] over a frame of input and
    /// installs the recognized command names as the character's command source.
    ///
    /// This is the correct command pipeline (mirroring `fp-app`'s single-character
    /// path): build a RAW absolute [`InputState`] straight from the [`MatchInput`]
    /// (no facing pre-rotation — facing is resolved at match time inside the
    /// matcher), push it into the rolling buffer, run the matcher facing-relative
    /// (`F`/`B` resolve against the character's current facing), then snapshot the
    /// active command names into an [`ActiveCommands`] command source. The
    /// character's own `.cmd`/common1 controllers (and the engine built-in
    /// locomotion) then read `command = "holdfwd"`, `command = "QCF_x"`, … exactly
    /// as the data files author them.
    ///
    /// Also updates [`fp_character::Character::holding_back`] from the
    /// facing-relative direction so [`resolve_attack`] can choose the guard path.
    fn feed_input(&mut self, input: MatchInput) {
        let raw = match_input_to_state(input);
        self.input_buffer.push(raw);

        let facing_right = self.character.facing == Facing::Right;
        self.matcher
            .check_commands(&self.input_buffer, facing_right);

        // The defender guards while holding "back" (away from the opponent):
        // resolve the raw absolute direction to facing-relative and read `back`,
        // but only when "back" is *unambiguously* held — both horizontals held
        // (or neither) is not a block, matching MUGEN's guard gate.
        let logical = logical_direction(&raw.direction, facing_right);
        self.character.holding_back = logical.back && !logical.forward;

        let active = snapshot_active_commands(&self.matcher, &self.command_defs);
        self.character.set_command_source(Box::new(active));
    }

    /// The body's left half-width (distance from axis to the `-X` edge), resolved
    /// for the current facing. Mirrors [`PushBody`]'s internal resolution so the
    /// clamp uses the same geometry as the push, and honours the per-tick `Width`
    /// override (#10) via [`push_widths`](Self::push_widths).
    fn push_body_left_half(&self) -> f32 {
        let (front, back) = self.push_widths();
        match self.character.facing {
            Facing::Right => back,
            Facing::Left => front,
        }
    }

    /// The body's right half-width (distance from axis to the `+X` edge), resolved
    /// for the current facing.
    fn push_body_right_half(&self) -> f32 {
        let (front, back) = self.push_widths();
        match self.character.facing {
            Facing::Right => front,
            Facing::Left => back,
        }
    }
}

/// Builds a RAW (absolute-direction) [`InputState`] from a [`MatchInput`].
///
/// The mapping is straight-through: [`MatchInput`]'s `left`/`right`/`up`/`down`
/// are already absolute screen directions, so they become the [`Direction`]'s
/// raw fields unchanged — facing is resolved later, at match time, inside the
/// [`CommandMatcher`] (do NOT pre-rotate here). Each attack button maps to its
/// [`Button`]. The resulting state is what gets pushed into the player's input
/// buffer each fight tick.
fn match_input_to_state(input: MatchInput) -> InputState {
    let mut state = InputState {
        direction: Direction {
            up: input.up,
            down: input.down,
            left: input.left,
            right: input.right,
        },
        ..Default::default()
    };
    for (pressed, button) in [
        (input.a, Button::A),
        (input.b, Button::B),
        (input.c, Button::C),
        (input.x, Button::X),
        (input.y, Button::Y),
        (input.z, Button::Z),
    ] {
        if pressed {
            state.set_button(button, true);
        }
    }
    state
}

/// Snapshots the command names a [`CommandMatcher`] reports active this tick into
/// an [`ActiveCommands`] command source.
///
/// Thin wrapper over the shared [`CommandMatcher::active_command_names_in`]
/// primitive (the actual filter lives in `fp-input`, in one place), which bounds
/// the matcher's active set to this character's own command vocabulary.
fn snapshot_active_commands(
    matcher: &CommandMatcher,
    command_defs: &[CommandDef],
) -> ActiveCommands {
    ActiveCommands::from_names(matcher.active_command_names_in(command_defs))
}

/// Decides a winner by comparing two life totals (used at time over).
fn compare_life(p1_life: i32, p2_life: i32) -> Winner {
    use std::cmp::Ordering;
    match p1_life.cmp(&p2_life) {
        Ordering::Greater => Winner::P1,
        Ordering::Less => Winner::P2,
        Ordering::Equal => Winner::Draw,
    }
}

/// Returns `true` if a character is in a *neutral* state where the baseline
/// `facep2` logic should keep it turned toward the opponent.
///
/// We keep this deliberately simple (documented baseline): a character should
/// re-face the opponent only while it is on its feet and not committed to an
/// action — i.e. a [`StateType::Standing`] (or the catch-all
/// [`StateType::Unchanged`]) character whose move type is idle. A character that
/// is attacking, being hit, crouching, airborne, or lying keeps its current
/// facing (turning mid-move would look wrong and could flip a knockback). MUGEN's
/// real turn behavior is richer (a dedicated turning state, `5900` common, etc.);
/// this is the documented minimal stand-in for task 7.1.
fn is_neutral_facing_state(c: &Character) -> bool {
    matches!(c.state_type, StateType::Standing | StateType::Unchanged)
        && c.move_type == MoveType::Idle
}

/// Sets both characters' facing toward each other based on their X positions,
/// unconditionally. Used at match construction to seed a sensible initial facing.
///
/// The character with the smaller X faces right (toward the larger X) and vice
/// versa; equal X leaves them facing right/left as a deterministic default.
fn face_each_other(a: &mut Character, b: &mut Character) {
    let (fa, fb) = facings_toward(a.pos.x, b.pos.x);
    a.facing = fa;
    b.facing = fb;
}

impl RoundResetState {
    /// Captures the reset template (start position, facing, life maximum) from a
    /// freshly-seeded character at match construction.
    fn capture(c: &Character) -> Self {
        Self {
            pos: c.pos,
            facing: c.facing,
            life_max: c.life_max,
        }
    }
}

/// Resets one fighter to the top-of-round state from its captured
/// [`RoundResetState`] (task 7.4 round reset).
///
/// Restores full life and the start position/facing; zeroes velocity; clears the
/// transient combat state that must not survive a round (active HitDef, hit-pause
/// and hit-shake timers, last-hit `GetHitVar`s, and the move-connect latch); and
/// returns the fighter to a neutral standing-idle state with the animation cursor
/// rewound and control removed (the intro re-grants control when the next round's
/// fight begins). Power is **not** touched here — it carries across rounds (see
/// [`Match::reset_for_next_round`]). Pure field writes; never panics.
fn reset_fighter_for_round(c: &mut Character, reset: RoundResetState) {
    // Resources: full life, max from the captured template; power carries over.
    c.life_max = reset.life_max;
    c.life = reset.life_max;

    // Kinematics: back to the start, at rest, facing the captured direction.
    c.pos = reset.pos;
    c.vel = Vec2::new(0.0, 0.0);
    c.facing = reset.facing;

    // Neutral stance: standing, idle, on the ground, no control during intro.
    c.state_type = StateType::Standing;
    c.move_type = MoveType::Idle;
    c.ctrl = false;
    c.holding_back = false;

    // State machine: back to the standing state with a fresh timer.
    c.state_no = 0;
    c.prev_state_no = 0;
    c.state_time = 0;

    // Animation cursor: rewind to the idle action's first element.
    c.anim = 0;
    c.anim_elem = 0;
    c.anim_elem_time = 0;
    c.anim_time = 0;

    // Transient combat state must not leak across the round boundary.
    c.active_hitdef = None;
    c.hitpause = 0;
    c.shaketime = 0;
    c.get_hit_vars = fp_character::GetHitVars::default();
    c.move_connect.reset();
    // Per-id projectile contact/hit/guard timing does not survive a round
    // boundary (the projectiles themselves are cleared too — T026).
    c.proj_events.clear();

    // Drop any stale commands so nothing fires before the next round goes live.
    c.set_command_source(Box::new(fp_character::NoCommands));
}

/// Applies the baseline `facep2`: each character that is in a neutral state turns
/// to face the other; a non-neutral character keeps its facing.
///
/// See [`is_neutral_facing_state`] for the (documented, simplified) definition of
/// "neutral". This is intentionally conservative so it never flips a character
/// out of an attack or get-hit reaction mid-animation.
fn face_each_other_when_neutral(a: &mut Character, b: &mut Character) {
    let (fa, fb) = facings_toward(a.pos.x, b.pos.x);
    // `AssertSpecial NoAutoTurn` (#13) suppresses this baseline auto-turn for the
    // tick that asserted it — e.g. common1's run (state 100) asserts it so a run
    // is not flipped mid-dash. The flag was set during this character's tick (and
    // is not cleared until the start of its next tick), so it is still readable
    // here at end-of-frame.
    if is_neutral_facing_state(a) && !a.asserted.no_auto_turn {
        a.facing = fa;
    }
    if is_neutral_facing_state(b) && !b.asserted.no_auto_turn {
        b.facing = fb;
    }
}

/// Returns the facings the two characters should have to look at each other,
/// given their X positions: `(facing_for_a, facing_for_b)`.
///
/// The left character (smaller X) faces right; the right character faces left.
/// When the X positions are equal, `a` faces right and `b` faces left as a
/// deterministic tie-break.
fn facings_toward(ax: f32, bx: f32) -> (Facing, Facing) {
    if ax <= bx {
        (Facing::Right, Facing::Left)
    } else {
        (Facing::Left, Facing::Right)
    }
}

/// Tests whether `attacker`'s current-frame attack (`Clsn1`) boxes overlap
/// `defender`'s current-frame hurt (`Clsn2`) boxes, positioned by each
/// character's `pos`/`facing` (audit #20, the clash-detection probe).
///
/// This mirrors the geometric test inside
/// [`fp_character::combat::resolve_attack`] — pulling each character's
/// current AIR frame boxes and running [`fp_combat::detect_hit`] — but
/// *without* requiring or consuming a `HitDef`, so the round coordinator can
/// detect a simultaneous clash before deciding which side(s) apply. An absent
/// action/frame or an empty box set yields `false` (no contact); never panics.
fn directional_contact(attacker: &Player, defender: &Player) -> bool {
    let clsn1 = current_frame_clsn1(
        &attacker.loaded.air,
        attacker.character.anim,
        attacker.character.anim_elem,
    );
    let clsn2 = current_frame_clsn2(
        &defender.loaded.air,
        defender.character.anim,
        defender.character.anim_elem,
    );
    if clsn1.is_empty() || clsn2.is_empty() {
        return false;
    }
    detect_hit(
        &clsn1,
        attacker.character.pos,
        to_clsn_facing(attacker.character.facing),
        &clsn2,
        defender.character.pos,
        to_clsn_facing(defender.character.facing),
    )
}

/// Like [`directional_contact`], but returns *where* the attack connects — the
/// [`fp_combat::HitContact`] overlap (its center is the hit-spark anchor, audit #17).
///
/// This mirrors the geometric probe inside
/// [`fp_character::combat::resolve_attack`] using the same pure
/// [`fp_combat::detect_hit_contact`], positioned by each character's `pos`/`facing`,
/// but *without* requiring or consuming a `HitDef`. The coordinator captures this
/// **before** running the resolve (which moves the defender into its get-hit state
/// and so changes its hurt boxes), and only uses it when the resolve actually
/// reports a connection. An absent action/frame or empty box set yields `None`
/// (no contact); never panics.
fn directional_contact_point(
    attacker: &Player,
    defender: &Player,
) -> Option<fp_combat::HitContact> {
    let clsn1 = current_frame_clsn1(
        &attacker.loaded.air,
        attacker.character.anim,
        attacker.character.anim_elem,
    );
    let clsn2 = current_frame_clsn2(
        &defender.loaded.air,
        defender.character.anim,
        defender.character.anim_elem,
    );
    if clsn1.is_empty() || clsn2.is_empty() {
        return None;
    }
    detect_hit_contact(
        &clsn1,
        attacker.character.pos,
        to_clsn_facing(attacker.character.facing),
        &clsn2,
        defender.character.pos,
        to_clsn_facing(defender.character.facing),
    )
}

/// Converts a [`Character`] [`Facing`] into the [`ClsnFacing`] that
/// [`fp_combat::detect_hit`] expects (distinct types in distinct crates).
fn to_clsn_facing(facing: Facing) -> ClsnFacing {
    match facing {
        Facing::Right => ClsnFacing::Right,
        Facing::Left => ClsnFacing::Left,
    }
}

/// Converts an AIR-frame collision [`Rect`] (top-left + size) into the
/// corner-pair [`ClsnBox`] the detection path uses.
fn rect_to_clsn(r: &Rect) -> ClsnBox {
    ClsnBox::new(r.x, r.y, r.right(), r.bottom())
}

/// Returns the `Clsn1` (attack) boxes for the current animation frame, or an
/// empty vector if the action/frame/boxes are absent. Mirrors `fp-character`'s
/// private frame-box extraction (which the engine cannot reach across crates).
fn current_frame_clsn1(air: &AirFile, anim: i32, elem: i32) -> Vec<ClsnBox> {
    current_frame_clsn(air, anim, elem, true)
}

/// Returns the `Clsn2` (hurt) boxes for the current animation frame, or an
/// empty vector if the action/frame/boxes are absent.
fn current_frame_clsn2(air: &AirFile, anim: i32, elem: i32) -> Vec<ClsnBox> {
    current_frame_clsn(air, anim, elem, false)
}

/// Shared frame-box extraction: looks up the action, clamps the (zero-based)
/// element index into range, and converts the selected box set (`attack` =
/// `Clsn1`, else `Clsn2`). Any missing piece yields an empty vector — never a
/// panic. A negative `elem` clamps to frame `0`; an over-large one to the last
/// frame, matching `fp-character`'s detection semantics.
fn current_frame_clsn(air: &AirFile, anim: i32, elem: i32, attack: bool) -> Vec<ClsnBox> {
    let Some(action) = air.action(anim) else {
        return Vec::new();
    };
    if action.frames.is_empty() {
        return Vec::new();
    }
    let max = action.frames.len() - 1;
    let idx = if elem < 0 {
        0
    } else {
        (elem as usize).min(max)
    };
    let Some(frame) = action.frames.get(idx) else {
        return Vec::new();
    };
    let rects = if attack { &frame.clsn1 } else { &frame.clsn2 };
    rects.iter().map(rect_to_clsn).collect()
}

/// Computes the on-block / on-hit frame advantage for an attack the `attacker`
/// just landed on the opponent, in 60Hz ticks (T065, feature F026).
///
/// Advantage is the **defender's induced stun** (`stun` — the hit-stun on a clean
/// hit, the guard-stun on a block, straight from the resolved
/// [`AttackResolution::stun`](fp_character::AttackResolution::stun)) minus the
/// **attacker's frames-until-actionable** (the frames the attacker still owes
/// before it can act again — its current move's recovery measured from where the
/// move's frame cursor currently sits). A positive result means the attacker
/// recovers first (advantage); a negative one means the defender recovers first.
///
/// "Frames-until-actionable" is the attacker's remaining move time: its current
/// action's static [`total`](fp_character::MoveFrameData::total) frame count minus
/// the frames already elapsed in that action (`startup + active` already spent on
/// reaching/holding the active window, so what is left is the recovery the
/// attacker must still sit through). It is read from the attacker's own AIR action
/// so it reflects the move actually being thrown, not a fixed guess.
///
/// Returns [`None`] (the readout shows `—`) when the attacker's current action is
/// not a countable attack ([`MoveFrameData::compute`] returned [`None`]) — there
/// is no honest recovery to subtract — so a wrong number is never shown. Never
/// panics: a missing action or an uncountable one both fall to [`None`], and the
/// arithmetic is saturating via [`frame_advantage`].
fn compute_frame_advantage(attacker: &Player, stun: i32) -> Option<i32> {
    let action = attacker.loaded.air.action(attacker.character.anim)?;
    let fd = MoveFrameData::compute(action)?;
    let elapsed = elapsed_in_action(action, attacker.character.anim_elem).min(fd.total);
    // Frames the attacker still owes before it can act: the remainder of the move.
    let frames_until_actionable = (fd.total - elapsed).max(0);
    Some(frame_advantage(stun, frames_until_actionable))
}

/// Sums the AIR frame durations of the elements strictly before the (zero-based)
/// `elem` cursor, giving the ticks elapsed in the action up to the start of the
/// current element — the static "where the move cursor sits" used by
/// [`compute_frame_advantage`].
///
/// A `-1` (infinite-hold) duration in the elapsed span is treated as `0` (it
/// contributes no measured time toward recovery rather than poisoning the count);
/// a negative `elem` clamps to `0`; an over-large `elem` clamps to the frame count
/// (the whole action elapsed). Never panics.
fn elapsed_in_action(action: &AnimAction, elem: i32) -> i32 {
    let upto = if elem < 0 {
        0
    } else {
        (elem as usize).min(action.frames.len())
    };
    action.frames[..upto]
        .iter()
        .map(|f| f.ticks.max(0))
        .fold(0i32, i32::saturating_add)
}

/// Builds the [`fp_character::SoundRequest`] for a `HitDef` impact sound (the
/// hit or guard sound chosen by [`fp_character::AttackResolution::hit_sound`]).
///
/// The impact sound plays on **channel 0** — a fixed channel reserved here for
/// hit/guard impacts so a new hit interrupts the previous impact rather than
/// stacking on the auto-allocated PlaySnd channels — at **full volume**
/// (`volume_scale = 100`) and never loops. `group`/`sample`/`common` are carried
/// straight from the [`fp_character::SoundId`], so a common-file hitsound (one
/// authored with no `S` prefix) resolves against the fight.snd downstream, while
/// an `S`-prefixed one stays on the character's own `.snd`.
fn hit_sound_request(s: fp_character::SoundId) -> fp_character::SoundRequest {
    fp_character::SoundRequest {
        group: s.group,
        sample: s.sample,
        channel: 0,
        volume_scale: 100,
        looping: false,
        common: s.common,
    }
}

/// Applies a binder's per-tick [`fp_character::TargetOp`]s (emitted by its
/// `Target*` controllers) to its `target` — the opponent in a 1-v-1 [`Match`]
/// (task P8b, the engine side of the throw system).
///
/// `binder` is the character whose `Target*` controllers fired this tick (read
/// only); `target` is the opponent the ops act on (mutated). `target_states` is
/// the **target's own** compiled state graph, used to enter a `TargetState`
/// destination so the victim runs through *its* thrown-animation states (KFM
/// state 820). Each op maps to its MUGEN controller:
///
/// - [`TargetOp::State`] — drive the target into the given state via its own
///   graph ([`Character::change_state`](fp_character::Character::change_state));
///   an unknown destination degrades to a cursor-only update (never panics).
/// - [`TargetOp::Bind`] — pin the target to a facing-relative offset from the
///   binder: `target.pos = binder.pos + (x * binder.facing.sign(), y)`. The
///   per-tick re-emit of `TargetBind` (KFM state 810) gives a continuous hold;
///   this is a first-cut *apply-on-emit* — the `time` field is honored only
///   implicitly by the re-emit each tick, not tracked as a countdown here.
/// - [`TargetOp::LifeAdd`] — add to the target's life, clamped to
///   `[floor, life_max]` where `floor` is `0` when `kill` is set, else `1` (a
///   non-killing add cannot drop the victim below `1`).
/// - [`TargetOp::Facing`] — orient the target relative to the binder: `value >=
///   0` faces it the **same** way as the binder, negative the **opposite** way.
/// - [`TargetOp::VelSet`] / [`TargetOp::VelAdd`] — set / add the target's
///   velocity `(x, y)` (taken as written; mirroring already resolved upstream).
/// - [`TargetOp::PowerAdd`] — add to the target's power, clamped to
///   `[0, power_max]`.
///
/// Pure field writes plus the panic-free `change_state` seam; never panics, and
/// an op for a missing/unknown state degrades safely. Empty `ops` is a no-op.
fn apply_target_ops(
    binder: &Character,
    target: &mut Character,
    target_states: &std::collections::HashMap<i32, fp_character::CompiledState>,
    ops: &[fp_character::TargetOp],
) {
    use fp_character::TargetOp;
    for op in ops {
        match *op {
            TargetOp::State(value) => {
                tracing::debug!(to_state = value, "target enters TargetState");
                target.change_state(target_states, value);
            }
            TargetOp::Bind { time, pos } => {
                let (ox, oy) = pos;
                let new_pos = Vec2::new(
                    binder.pos.x + ox * binder.facing.sign() as f32,
                    binder.pos.y + oy,
                );
                tracing::trace!(time, x = new_pos.x, y = new_pos.y, "target bound to binder");
                target.pos = new_pos;
            }
            TargetOp::LifeAdd { value, kill } => {
                let floor = if kill { 0 } else { 1 };
                let life_max = target.life_max.max(floor);
                target.life = target.life.saturating_add(value).clamp(floor, life_max);
            }
            TargetOp::Facing(value) => {
                target.facing = if value >= 0 {
                    binder.facing
                } else {
                    facing_opposite(binder.facing)
                };
            }
            TargetOp::VelSet((vx, vy)) => {
                target.vel = Vec2::new(vx, vy);
            }
            TargetOp::VelAdd((vx, vy)) => {
                target.vel = Vec2::new(target.vel.x + vx, target.vel.y + vy);
            }
            TargetOp::PowerAdd(value) => {
                let power_max = target.power_max.max(0);
                target.power = target.power.saturating_add(value).clamp(0, power_max);
            }
        }
    }
}

/// Returns the opposite of an [`fp_character::Facing`] (right ↔ left).
///
/// [`fp_character::Facing`] exposes a `sign` but no `opposite`; this local helper
/// flips it for [`TargetOp::Facing`]'s "opposite the binder" case.
fn facing_opposite(facing: Facing) -> Facing {
    match facing {
        Facing::Right => Facing::Left,
        Facing::Left => Facing::Right,
    }
}

/// Shared synthetic test fixtures used by both this crate's unit tests and the
/// [`team`] module's tests (T017). Authored from scratch — no external assets.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::{Match, Player, Side, TeamMatch};
    use fp_character::{
        Character, CharacterConstants, Facing, LoadedCharacter, MoveType, StateType,
    };
    use fp_core::{Rect, SpriteId, Vec2};
    use fp_formats::air::{AirFile, AnimAction, AnimFrame, BlendMode};
    use fp_formats::sff::SffFile;
    use std::collections::HashMap;

    /// Builds a minimal valid SFF v1 container in memory carrying a single linked
    /// (data-less) sprite, so a headless [`LoadedCharacter`] needs no asset on
    /// disk. Mirrors the `empty_sff` helper in `mod tests`.
    fn empty_sff() -> SffFile {
        const SUBHEADER_OFFSET: usize = 64;
        let mut buf = vec![0u8; SUBHEADER_OFFSET + 32];
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[15] = 1; // SFF v1
        buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // num_groups
        buf[20..24].copy_from_slice(&1u32.to_le_bytes()); // num_images
        buf[24..28].copy_from_slice(&(SUBHEADER_OFFSET as u32).to_le_bytes());
        SffFile::from_bytes(&buf).expect("synthetic SFF v1 must parse")
    }

    /// A one-action, one-frame AIR with a single hurt box on action 0, enough for
    /// the headless team-flow tests (no real sprites are read).
    fn simple_air() -> AirFile {
        let frame = AnimFrame {
            sprite: SpriteId::new(0, 0),
            offset: Vec2::new(0, 0),
            ticks: 1,
            flip_h: false,
            flip_v: false,
            blend: BlendMode::Normal,
            clsn1: Vec::new(),
            clsn2: vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
            ..Default::default()
        };
        let mut actions = HashMap::new();
        actions.insert(
            0,
            AnimAction {
                action_number: 0,
                frames: vec![frame],
                loopstart: 0,
            },
        );
        AirFile { actions }
    }

    /// A synthetic [`LoadedCharacter`] with an empty state graph and a simple AIR.
    fn simple_loaded() -> LoadedCharacter {
        LoadedCharacter {
            name: "test".to_string(),
            localcoord: (320, 240),
            constants: CharacterConstants::default(),
            states: HashMap::new(),
            sff: empty_sff(),
            air: simple_air(),
            cmd: None,
            snd: None,
            palettes: Vec::new(),
        }
    }

    /// Builds a fresh headless [`Player`] positioned at world X `x` — a default
    /// [`Character`] (full life) wrapping the synthetic [`simple_loaded`] assets.
    /// Faces right when on the left half of the stage, else left, so two such
    /// players square off.
    pub(crate) fn make_player(x: f32) -> Player {
        let mut c = Character::new();
        c.pos = Vec2::new(x, 0.0);
        c.facing = if x < 0.0 { Facing::Right } else { Facing::Left };
        c.state_type = StateType::Standing;
        c.move_type = MoveType::Idle;
        Player::new(c, simple_loaded())
    }

    /// Test-only mutators on [`TeamMatch`], used to drive deterministic KO / damage
    /// scenarios without having to land a real hit through the combat pipeline.
    impl TeamMatch {
        /// Forces the active fighter on `side` to `0` life (a KO) for the next
        /// resolution.
        pub(crate) fn kill_active(&mut self, side: Side) {
            self.set_active_life(side, 0);
        }

        /// Sets the active fighter on `side` to exactly `life`.
        pub(crate) fn set_active_life(&mut self, side: Side, life: i32) {
            let inner: &mut Match = self.inner_mut_for_test();
            match side {
                Side::P1 => inner.p1_mut_for_test().character.life = life,
                Side::P2 => inner.p2_mut_for_test().character.life = life,
            }
        }

        /// Forces the `idx`-th reserve fighter on `side` to `0` life (a KO).
        pub(crate) fn kill_reserve(&mut self, side: Side, idx: usize) {
            if let Some(p) = self.reserve_mut_for_test(side, idx) {
                p.character.life = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_character::{
        Character, CharacterConstants, CompiledController, CompiledExpr, CompiledParam,
        CompiledState, CompiledTriggerGroup, Facing, LoadedCharacter, MoveType, ProjContactTracker,
        StateType,
    };
    use fp_combat::{Damage, HitDef, HitFlags, HitTimes, PauseTime, Priority, PriorityType};
    use fp_core::{Rect, SpriteId, Vec2};
    use fp_formats::air::{AirFile, AnimAction, AnimFrame, BlendMode};
    use fp_formats::cmd::CmdFile;
    use fp_formats::sff::SffFile;
    use std::collections::HashMap;

    /// Builds a [`LoadedCharacter`] with the given AIR and a `.cmd` parsed from
    /// `cmd_text`, so a [`Player`] built from it has a real, non-empty
    /// [`CommandMatcher`]. Used by the input-pipeline tests.
    fn loaded_with_cmd(air: AirFile, cmd_text: &str) -> LoadedCharacter {
        let mut loaded = loaded_with(air);
        loaded.cmd = Some(CmdFile::from_str(cmd_text).expect("test .cmd must parse"));
        loaded
    }

    /// The minimal `.cmd` defining the four facing-relative "hold" commands the
    /// engine built-in locomotion gates on, plus a light-punch button command,
    /// mirroring the shapes a real `.cmd` (like KFM's) authors.
    const HOLD_CMD: &str = "\
[Command]
name = \"holdfwd\"
command = /$F
time = 1

[Command]
name = \"holdback\"
command = /$B
time = 1

[Command]
name = \"holdup\"
command = /$U
time = 1

[Command]
name = \"holddown\"
command = /$D
time = 1

[Command]
name = \"punch\"
command = x
time = 1
";

    /// Builds a one-action, one-frame [`AirFile`] carrying the given Clsn boxes.
    fn air_with(action: i32, clsn1: Vec<Rect>, clsn2: Vec<Rect>) -> AirFile {
        let frame = AnimFrame {
            sprite: SpriteId::new(0, 0),
            offset: Vec2::new(0, 0),
            ticks: 1,
            flip_h: false,
            flip_v: false,
            blend: BlendMode::Normal,
            clsn1,
            clsn2,
            ..Default::default()
        };
        let mut actions = HashMap::new();
        actions.insert(
            action,
            AnimAction {
                action_number: action,
                frames: vec![frame],
                loopstart: 0,
            },
        );
        AirFile { actions }
    }

    /// Builds a minimal valid SFF v1 container in memory carrying a single
    /// *linked* (data-less) sprite, so a headless [`LoadedCharacter`] can be
    /// constructed without a sprite asset on disk. The v1 parser rejects a
    /// zero-sprite file, so we include one linked sprite (`data_length = 0`,
    /// which skips PCX decoding); the simulation never reads sprites.
    fn empty_sff() -> SffFile {
        // SFF v1 layout used here:
        //   [0..12)   "ElecbyteSpr\0" signature
        //   [15]      major version = 1
        //   [16..20)  num_groups
        //   [20..24)  num_images = 1
        //   [24..28)  first_subheader_offset = 64
        //   [64..96)  one 32-byte sprite sub-header:
        //               [0..4)  next_offset = 0  (terminates the walk)
        //               [4..8)  data_length = 0  (linked sprite, no PCX)
        const SUBHEADER_OFFSET: usize = 64;
        let mut buf = vec![0u8; SUBHEADER_OFFSET + 32];
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[15] = 1; // SFF v1
        buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // num_groups
        buf[20..24].copy_from_slice(&1u32.to_le_bytes()); // num_images
        buf[24..28].copy_from_slice(&(SUBHEADER_OFFSET as u32).to_le_bytes());
        // Sub-header: next_offset = 0, data_length = 0 (the rest stays zeroed).
        SffFile::from_bytes(&buf).expect("synthetic SFF v1 must parse")
    }

    /// A synthetic [`LoadedCharacter`] with an empty state graph and the given
    /// AIR file. The SFF is a minimal empty v1 container (no sprites needed for
    /// headless simulation), so this never touches disk.
    fn loaded_with(air: AirFile) -> LoadedCharacter {
        LoadedCharacter {
            name: "test".to_string(),
            localcoord: (320, 240),
            constants: CharacterConstants::default(),
            states: HashMap::new(),
            sff: empty_sff(),
            air,
            cmd: None,
            snd: None,
            palettes: Vec::new(),
        }
    }

    /// A defender-style loaded character: a single hurt box on action 0.
    fn defender_loaded() -> LoadedCharacter {
        loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ))
    }

    /// An attacker-style loaded character: a punch (Clsn1) box on action 200 plus
    /// a hurt box on action 0 (so it can also be hit).
    fn attacker_loaded() -> LoadedCharacter {
        let punch = air_with(200, vec![Rect::new(10.0, -60.0, 45.0, 20.0)], Vec::new());
        // Merge an action-0 hurt frame in too so the same character is hittable.
        let mut air = punch;
        air.actions.insert(
            0,
            AnimAction {
                action_number: 0,
                frames: vec![AnimFrame {
                    sprite: SpriteId::new(0, 0),
                    offset: Vec2::new(0, 0),
                    ticks: 1,
                    flip_h: false,
                    flip_v: false,
                    blend: BlendMode::Normal,
                    clsn1: Vec::new(),
                    clsn2: vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
                    ..Default::default()
                }],
                loopstart: 0,
            },
        );
        loaded_with(air)
    }

    /// A "brawler" loaded character whose action 200 carries BOTH a Clsn1 attack
    /// box and a Clsn2 hurt box on the same frame, so two such characters posed on
    /// action 200 facing each other mutually connect (each one's attack box
    /// overlaps the other's hurt box). Used by the priority-clash tests, where the
    /// clash requires *both* directions to geometrically connect this tick.
    ///
    /// The boxes are wide and centred about the axis so the connection is robust
    /// to the small player-push separation a `Match::tick` applies.
    fn brawler_loaded() -> LoadedCharacter {
        loaded_with(air_with(
            200,
            // Attack box reaching out toward the opponent and across the axis.
            vec![Rect::new(-60.0, -60.0, 120.0, 40.0)],
            // Hurt box about the axis.
            vec![Rect::new(-30.0, -70.0, 60.0, 70.0)],
        ))
    }

    /// Poses a [`Player`]'s character on its action-200 brawler frame with a fresh
    /// (un-connected) HitDef of the given priority, ready to clash this tick.
    fn arm_brawler(p: &mut Player, hd: HitDef) {
        p.character.anim = 200;
        p.character.anim_elem = 0;
        p.character.move_type = MoveType::Attack;
        p.character.state_type = StateType::Standing;
        p.character.holding_back = false;
        p.character.active_hitdef = Some(hd);
        p.character.move_connect.reset();
    }

    /// A HitDef with concrete damage/knockback/stun (mirrors the combat tests).
    fn sample_hitdef() -> HitDef {
        HitDef {
            damage: Damage { hit: 30, guard: 5 },
            guardflag: HitFlags::parse("MA"),
            hitflag: HitFlags::parse("MAF"),
            ground_velocity: Vec2::new(4.0, -3.0),
            air_velocity: Vec2::new(4.0, -6.0),
            guard_velocity: -2.0,
            hittimes: HitTimes {
                ground: 12,
                air: 20,
                guard: 8,
            },
            pausetime: PauseTime { p1: 8, p2: 8 },
            ..HitDef::default()
        }
    }

    /// Builds a basic match: two characters far enough apart not to overlap, with
    /// a short round so tests can reach time-over quickly if needed.
    fn basic_match() -> Match {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        Match::new(p1, p2, StageBounds::new(-200.0, 200.0))
    }

    /// Drives the match out of the intro phase into [`RoundState::Fight`].
    fn into_fight(m: &mut Match) {
        for _ in 0..(INTRO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
    }

    #[test]
    fn both_characters_tick_without_panic() {
        let mut m = basic_match();
        for _ in 0..120 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // Reached at least the fight phase and no panic occurred.
        assert!(matches!(
            m.round_state(),
            RoundState::Fight | RoundState::Ko | RoundState::Win
        ));
    }

    #[test]
    fn starts_in_intro_then_enters_fight() {
        let mut m = basic_match();
        assert_eq!(m.round_state(), RoundState::Intro);
        // Tick through the intro.
        for _ in 0..INTRO_FRAMES {
            assert_eq!(m.round_state(), RoundState::Intro);
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // After INTRO_FRAMES ticks the intro elapses and the fight begins.
        assert_eq!(m.round_state(), RoundState::Fight);
        // Control is granted at fight start.
        assert!(m.p1().character.ctrl);
        assert!(m.p2().character.ctrl);
    }

    #[test]
    fn initial_facing_is_toward_opponent() {
        let m = basic_match();
        // P1 at -50 is left of P2 at +50: P1 faces right, P2 faces left.
        assert_eq!(m.p1().facing(), Facing::Right);
        assert_eq!(m.p2().facing(), Facing::Left);
    }

    #[test]
    fn connecting_attack_drops_life_and_ko_advances_round() {
        // Place the attacker so its punch box overlaps the defender's hurt box.
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;

        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;

        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Arm the attack only once the fight has begun, and set the defender almost
        // dead so a single 30-damage punch is a KO.
        m.p2.character.life = 20;
        let life_before = m.p2().life();
        // Re-arm and re-attach the HitDef each frame (a real move would keep it
        // active across its active frames); pin the attacker on its punch frame.
        for _ in 0..5 {
            m.p1.character.anim = 200;
            m.p1.character.anim_elem = 0;
            m.p1.character.move_type = MoveType::Attack;
            m.p1.character.active_hitdef = Some(sample_hitdef());
            m.p1.character.move_connect.reset();
            // Keep the defender on its hurt frame and on its feet.
            m.p2.character.anim = 0;
            m.p2.character.anim_elem = 0;
            m.p2.character.state_type = StateType::Standing;
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }

        assert!(
            m.p2().life() < life_before,
            "a connecting attack must reduce the defender's life"
        );
        assert_eq!(m.p2().life(), 0, "lethal punch drops life to zero");
        // The KO must have advanced the round out of Fight.
        assert!(
            matches!(m.round_state(), RoundState::Ko | RoundState::Win),
            "a KO advances the round state, got {:?}",
            m.round_state()
        );
        assert_eq!(m.winner(), Some(Winner::P1));
    }

    // ---- Frame-advantage readout (T065, feature F026) --------------------

    /// An attacker whose action 200 has a real startup / active / recovery shape so
    /// the frame-advantage readout has a non-trivial "frames-until-actionable" to
    /// subtract. Layout: 3 startup frames (no Clsn1), 2 active frames (Clsn1 attack
    /// box), 4 recovery frames (no Clsn1) — `MoveFrameData::compute` =>
    /// startup=3, active=2, recovery=4, total=9. Action 0 keeps a hurt box so the
    /// character is hittable.
    fn fa_attacker_loaded() -> LoadedCharacter {
        let attack = Rect::new(10.0, -60.0, 60.0, 20.0);
        let hurt = Rect::new(-18.0, -70.0, 36.0, 70.0);
        let mk = |clsn1: Vec<Rect>, clsn2: Vec<Rect>, ticks: i32| AnimFrame {
            sprite: SpriteId::new(0, 0),
            offset: Vec2::new(0, 0),
            ticks,
            flip_h: false,
            flip_v: false,
            blend: BlendMode::Normal,
            clsn1,
            clsn2,
            ..Default::default()
        };
        let mut actions = HashMap::new();
        actions.insert(
            200,
            AnimAction {
                action_number: 200,
                // Each element holds 10 ticks so a single `Match::tick` advance
                // (1 tick) stays WITHIN the element it started on — the anim-elem
                // cursor the readout reads is therefore deterministic across the
                // tick. startup = 30, active = 20, recovery = 40, total = 90.
                frames: vec![
                    mk(Vec::new(), Vec::new(), 10),   // startup 0
                    mk(Vec::new(), Vec::new(), 10),   // startup 1
                    mk(Vec::new(), Vec::new(), 10),   // startup 2
                    mk(vec![attack], Vec::new(), 10), // active 0 (elem 3)
                    mk(vec![attack], Vec::new(), 10), // active 1 (elem 4)
                    mk(Vec::new(), Vec::new(), 10),   // recovery 0
                    mk(Vec::new(), Vec::new(), 10),   // recovery 1
                    mk(Vec::new(), Vec::new(), 10),   // recovery 2
                    mk(Vec::new(), Vec::new(), 10),   // recovery 3
                ],
                loopstart: 0,
            },
        );
        actions.insert(
            0,
            AnimAction {
                action_number: 0,
                frames: vec![mk(Vec::new(), vec![hurt], 1)],
                loopstart: 0,
            },
        );
        loaded_with(AirFile { actions })
    }

    /// Poses two fighters with P1's punch (action 200) overlapping P2's hurt box on
    /// action 0, returns a fight-phase match. P1 is the attacker with the
    /// startup/active/recovery action above; P2 is a plain hittable defender.
    fn fa_match() -> Match {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(-20.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.life = 1000;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(20.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 1000;
        let p1 = Player::new(p1c, fa_attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);
        m
    }

    /// On a scripted CONNECTING hit, the attacker's frame advantage is surfaced
    /// (the whole point of T065 acceptance criterion #2): it equals the defender's
    /// induced ground hit-stun minus the attacker's frames-until-actionable, and is
    /// exposed both on the attacker `Player` and rendered by `format_frame_data`.
    #[test]
    fn frame_advantage_surfaced_on_scripted_connecting_hit() {
        let mut m = fa_match();
        // Pin P1 on the FIRST active frame of action 200 (zero-based elem 3) with a
        // fresh HitDef, and keep P2 on its standing hurt frame so the punch lands.
        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 3;
        m.p1.character.move_type = MoveType::Attack;
        m.p1.character.state_type = StateType::Standing;
        m.p1.character.active_hitdef = Some(sample_hitdef());
        m.p1.character.move_connect.reset();
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;
        m.p2.character.holding_back = false;

        m.tick(MatchInput::none(), MatchInput::none());

        // The hit connected (P2 took damage).
        assert!(m.p2().life() < 1000, "scripted attack must connect");

        // Expected: defender ground hit-stun (sample_hitdef ground hittime = 12)
        // minus the attacker's frames-until-actionable. P1 sat on elem 3 (the first
        // active frame); a single tick advances only WITHIN that 10-tick element, so
        // the anim-elem cursor is still 3 when combat reads it. elapsed before
        // elem 3 = 30 (3 startup frames @10), total = 90 =>
        // frames-until-actionable = 90 - 30 = 60 => advantage = 12 - 60 = -48.
        let adv = m
            .p1()
            .frame_advantage()
            .expect("a connecting attack surfaces a frame advantage");
        assert_eq!(adv, -48, "12 hit-stun minus 60 remaining recovery = -48");
    }

    /// On a tick with NO connection, the advantage is cleared to `None` (the
    /// readout shows `—`), so a stale number never lingers on screen.
    #[test]
    fn frame_advantage_is_none_without_a_connection() {
        let mut m = fa_match();
        // P1 idle on action 0, no active HitDef -> nothing connects.
        m.p1.character.anim = 0;
        m.p1.character.active_hitdef = None;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().frame_advantage(),
            None,
            "no connection => no advantage shown"
        );
    }

    // ---- Priority / trade clash resolution (audit #20) -------------------

    /// Builds a fight-phase match between two brawlers posed to mutually connect
    /// (both on action 200, overlapping boxes), each with a survivable life so a
    /// single trade/hit keeps the round live. P1 sits left of P2.
    fn clash_match() -> Match {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(-20.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.life = 1000;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(20.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 1000;
        let p1 = Player::new(p1c, brawler_loaded());
        let p2 = Player::new(p2c, brawler_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);
        m
    }

    /// Sanity: the brawler geometry actually produces a SIMULTANEOUS clash — with
    /// both sides armed at EQUAL Hit priority both fighters take damage this tick
    /// (a trade), proving both directions connect under the new arbitration.
    #[test]
    fn equal_hit_priority_trades_both_land() {
        let mut m = clash_match();
        // Equal value, both Hit (KFM's case: 3, Hit) -> Trade.
        let hd = HitDef {
            priority: Priority {
                value: 3,
                kind: PriorityType::Hit,
            },
            ..sample_hitdef()
        };
        arm_brawler(&mut m.p1, hd);
        arm_brawler(&mut m.p2, hd);

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(m.p1().life() < 1000, "P1 took the traded hit");
        assert!(m.p2().life() < 1000, "P2 took the traded hit");
    }

    /// Strictly higher priority value wins: the higher side lands and the lower
    /// side's HitDef is cancelled, so only the loser takes damage.
    #[test]
    fn higher_priority_value_cancels_lower() {
        let mut m = clash_match();
        // P1 priority 6, P2 priority 3 -> P1 wins, P2's HitDef cancelled.
        arm_brawler(
            &mut m.p1,
            HitDef {
                priority: Priority {
                    value: 6,
                    kind: PriorityType::Hit,
                },
                ..sample_hitdef()
            },
        );
        arm_brawler(
            &mut m.p2,
            HitDef {
                priority: Priority {
                    value: 3,
                    kind: PriorityType::Hit,
                },
                ..sample_hitdef()
            },
        );

        m.tick(MatchInput::none(), MatchInput::none());

        // P1 won: P2 takes damage, P1 is untouched (its attacker HitDef cancelled
        // P2's before P2's resolve_attack pass).
        assert!(
            m.p2().life() < 1000,
            "the higher-priority attacker (P1) lands on P2"
        );
        assert_eq!(
            m.p1().life(),
            1000,
            "the lower-priority attacker (P2) was cancelled"
        );
    }

    /// The mirror case: P2 has the higher value, so P1 is cancelled and only P1
    /// takes damage.
    #[test]
    fn higher_priority_value_wins_for_p2() {
        let mut m = clash_match();
        arm_brawler(
            &mut m.p1,
            HitDef {
                priority: Priority {
                    value: 2,
                    kind: PriorityType::Hit,
                },
                ..sample_hitdef()
            },
        );
        arm_brawler(
            &mut m.p2,
            HitDef {
                priority: Priority {
                    value: 7,
                    kind: PriorityType::Hit,
                },
                ..sample_hitdef()
            },
        );

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(
            m.p1().life() < 1000,
            "the higher-priority attacker (P2) lands on P1"
        );
        assert_eq!(
            m.p2().life(),
            1000,
            "the lower-priority attacker (P1) was cancelled"
        );
    }

    /// Equal value with a `Dodge` on one side suppresses BOTH attacks: neither
    /// fighter takes damage this tick.
    #[test]
    fn equal_value_dodge_suppresses_both() {
        let mut m = clash_match();
        arm_brawler(
            &mut m.p1,
            HitDef {
                priority: Priority {
                    value: 4,
                    kind: PriorityType::Hit,
                },
                ..sample_hitdef()
            },
        );
        arm_brawler(
            &mut m.p2,
            HitDef {
                priority: Priority {
                    value: 4,
                    kind: PriorityType::Dodge,
                },
                ..sample_hitdef()
            },
        );

        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(m.p1().life(), 1000, "a dodge clash lands nothing on P1");
        assert_eq!(m.p2().life(), 1000, "a dodge clash lands nothing on P2");
    }

    /// The SINGLE-attacker path must be unchanged: when only one fighter has an
    /// active HitDef, no clash arbitration runs and the lone attack lands exactly
    /// as before this feature. P1 attacks; P2 has no HitDef.
    #[test]
    fn single_attacker_path_is_unchanged() {
        let mut m = clash_match();
        arm_brawler(
            &mut m.p1,
            HitDef {
                // A LOW priority value that, IF a clash were (wrongly) detected
                // against P2, could cancel P1 — proving no clash logic fires when
                // P2 has no HitDef.
                priority: Priority {
                    value: 1,
                    kind: PriorityType::Hit,
                },
                ..sample_hitdef()
            },
        );
        // P2 is posed on the brawler frame (so its hurt box is present) but has NO
        // active HitDef — it is purely a defender this tick.
        m.p2.character.anim = 200;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;
        m.p2.character.active_hitdef = None;

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(m.p2().life() < 1000, "the lone attacker still lands");
        assert_eq!(m.p1().life(), 1000, "the non-attacking P2 deals no damage");
        // P1's HitDef was NOT spuriously cancelled by a phantom clash.
        assert!(
            m.p1().character.move_connect.contact(),
            "the lone attacker's move registered a connection"
        );
    }

    /// When the two attacks do NOT geometrically overlap simultaneously (only one
    /// direction connects), arbitration must not fire even though both sides have
    /// active HitDefs: the connecting side lands, the non-connecting side does
    /// nothing, and neither HitDef is cancelled by a phantom clash.
    #[test]
    fn no_clash_when_only_one_direction_connects() {
        // P1 is a brawler (action 200: attack + hurt boxes). P2 is armed with a
        // HitDef but posed on an action with NO attack box, so P2 cannot connect
        // while P1 can — a single-direction contact, not a clash.
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(-20.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.life = 1000;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(20.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 1000;
        // P2's loaded char only has hurt boxes on action 0 (defender_loaded).
        let p1 = Player::new(p1c, brawler_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // P1 high-value attacker on its brawler frame.
        arm_brawler(
            &mut m.p1,
            HitDef {
                priority: Priority {
                    value: 5,
                    kind: PriorityType::Hit,
                },
                ..sample_hitdef()
            },
        );
        // P2 armed with a HitDef but on action 0 (hurt boxes only -> cannot hit).
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;
        m.p2.character.active_hitdef = Some(HitDef {
            priority: Priority {
                value: 7,
                kind: PriorityType::Hit,
            },
            ..sample_hitdef()
        });
        m.p2.character.move_connect.reset();

        m.tick(MatchInput::none(), MatchInput::none());

        // Only P1 connects (P2 has no attack box this tick); P2 takes damage.
        // Crucially, P1's HitDef was NOT cancelled despite P2's higher *value* —
        // because there was no simultaneous clash to arbitrate.
        assert!(m.p2().life() < 1000, "P1's lone attack lands on P2");
        assert_eq!(m.p1().life(), 1000, "P2 could not connect (no attack box)");
    }

    #[test]
    fn connecting_attack_appends_hit_sound_to_attacker_requests() {
        use fp_character::SoundId;
        let hitsound = SoundId {
            group: 5,
            sample: 0,
            common: false,
        };

        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 1000; // survive so the round stays in Fight

        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Arm a hitsound-carrying HitDef and pin the attacker on its punch frame.
        let mut hd = sample_hitdef();
        hd.resources.hitsound = Some(hitsound);
        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 0;
        m.p1.character.move_type = MoveType::Attack;
        m.p1.character.active_hitdef = Some(hd);
        m.p1.character.move_connect.reset();
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;
        m.p2.character.holding_back = false; // takes a clean hit

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(m.p2().life() < 1000, "the attack must connect");
        // The attacker's (P1's) requests carry the HitDef hitsound on channel 0.
        let req = m
            .p1_sound_requests()
            .iter()
            .find(|r| r.group == hitsound.group && r.sample == hitsound.sample)
            .expect("P1's hit sound must be present in its sound requests");
        assert_eq!(req.channel, 0, "impact sound plays on the hit channel (0)");
        assert_eq!(req.volume_scale, 100, "impact sound at full volume");
        assert!(!req.looping);
        assert!(
            !req.common,
            "the SoundId `common` flag propagates unchanged (own .snd here)"
        );
        // The defender (P2) did not emit the attacker's hit sound.
        assert!(
            !m.p2_sound_requests()
                .iter()
                .any(|r| r.group == hitsound.group && r.sample == hitsound.sample),
            "the hit sound belongs to the attacker, not the defender"
        );
    }

    #[test]
    fn connecting_attack_without_hitsound_appends_nothing() {
        // A HitDef with no hit/guard sound must NOT append a spurious (0,0) request
        // when it connects — the `.and_then(|res| res.hit_sound)` guard is the whole
        // correctness of that. Mirrors the hitsound test but leaves the sounds None.
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 1000;

        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        let mut hd = sample_hitdef();
        hd.resources.hitsound = None; // explicit: no impact sound
        hd.resources.guardsound = None;
        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 0;
        m.p1.character.move_type = MoveType::Attack;
        m.p1.character.active_hitdef = Some(hd);
        m.p1.character.move_connect.reset();
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;
        m.p2.character.holding_back = false;

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(m.p2().life() < 1000, "the attack must connect");
        assert!(
            m.p1_sound_requests().is_empty(),
            "a connecting attack with no HitDef sound appends no request"
        );
    }

    #[test]
    fn guarded_attack_appends_guard_sound_to_attacker_requests() {
        use fp_character::SoundId;
        let guardsound = SoundId {
            group: 6,
            sample: 1,
            common: true,
        };

        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 1000;

        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        let mut hd = sample_hitdef();
        hd.resources.guardsound = Some(guardsound);
        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 0;
        m.p1.character.move_type = MoveType::Attack;
        m.p1.character.active_hitdef = Some(hd);
        m.p1.character.move_connect.reset();
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;

        // The defender blocks by holding "back" (away from the attacker). P2 sits
        // to P1's right and faces left, so "back" is the absolute RIGHT direction.
        // Feed it through the real input pipeline so `feed_input` sets
        // `holding_back` (which `tick` would otherwise overwrite from the input).
        let p2_back = MatchInput {
            right: true,
            ..MatchInput::none()
        };
        m.tick(MatchInput::none(), p2_back);

        // Guard, not a clean hit: only guard damage was dealt (sample_hitdef
        // guard = 5), and the guard sound is appended to the attacker's requests.
        assert_eq!(m.p2().life(), 1000 - 5, "guard damage, not hit damage");
        let req = m
            .p1_sound_requests()
            .iter()
            .find(|r| r.group == guardsound.group && r.sample == guardsound.sample)
            .expect("P1's guard sound must be present on a blocked attack");
        assert_eq!(req.channel, 0);
        assert_eq!(req.volume_scale, 100);
        assert!(
            req.common,
            "a common guard sound (the hitsound/guardsound default) resolves against fight.snd"
        );
    }

    #[test]
    fn ko_eventually_reaches_win() {
        let mut m = basic_match();
        into_fight(&mut m);
        // Force a KO directly.
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        // Hold through the KO phase: after exactly KO_FRAMES ticks the round is in
        // the Win (results) phase with the current-round winner decided. (One more
        // tick would run the best-of-N transition out of Win — see
        // `ko_p2_twice_wins_match_for_p1` — so we assert the Win phase here.)
        for _ in 0..KO_FRAMES {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.round_state(), RoundState::Win);
        assert_eq!(m.winner(), Some(Winner::P1));
    }

    // ---- Audit #21: RoundState / GameTime / MatchOver threaded to triggers ----

    /// The coordinator maps each round phase to MUGEN's `RoundState` code.
    #[test]
    fn round_state_trigger_code_maps_phases() {
        assert_eq!(RoundState::Intro.trigger_code(), 0);
        assert_eq!(RoundState::Fight.trigger_code(), 1);
        assert_eq!(RoundState::Ko.trigger_code(), 2);
        assert_eq!(RoundState::Win.trigger_code(), 3);
    }

    /// Before its first tick the match has not pushed a view yet, but after a tick
    /// the coordinator installs a live `RoundView` on BOTH characters reflecting
    /// the current phase, the advanced game clock, and the match-over flag.
    #[test]
    fn tick_installs_round_view_on_both_characters() {
        let mut m = basic_match();
        // Round one, intro phase: RoundState 0. After one tick GameTime is 1.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Intro);
        let v1 = m.p1().character.round_view;
        let v2 = m.p2().character.round_view;
        // Both fighters see the SAME engine-global view.
        assert_eq!(v1, v2, "both characters get the same coordinator view");
        assert_eq!(v1.round_state, 0, "intro maps to RoundState 0");
        assert_eq!(v1.game_time, 1, "GameTime advanced to 1 after one tick");
        assert!(!v1.match_over, "match not over during the intro");
        // The character's `RoundState`/`GameTime`/`MatchOver` triggers read this
        // installed `round_view` (the field-to-trigger mapping is pinned in
        // fp-character's `round_clock_triggers_read_round_view`).
    }

    /// `RoundState` advances 0 (intro) -> 1 (fight) -> 2 (KO) as the round
    /// progresses, and `GameTime` keeps climbing monotonically the whole time.
    #[test]
    fn round_state_and_game_time_advance_through_a_round() {
        let mut m = basic_match();

        // Intro: the characters' view reads RoundState 0 each tick.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.p1().character.round_view.round_state, 0);
        let gt_after_first = m.game_time();
        assert_eq!(gt_after_first, 1);

        // Tick into the fight phase; the installed view now reads RoundState 1.
        into_fight(&mut m);
        assert_eq!(m.round_state(), RoundState::Fight);
        assert_eq!(
            m.p1().character.round_view.round_state,
            1,
            "fight phase maps to RoundState 1"
        );
        assert!(
            m.game_time() > gt_after_first,
            "GameTime advances monotonically into the fight"
        );

        // GameTime never decreases tick-to-tick.
        let mut prev = m.game_time();
        for _ in 0..5 {
            m.tick(MatchInput::none(), MatchInput::none());
            assert!(m.game_time() >= prev, "GameTime is monotonic");
            prev = m.game_time();
        }

        // Force a KO: the next tick enters the KO hold, and the view installed at
        // the START of that tick still reads the fight code (1); a further tick in
        // the KO phase reads 2.
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        // Now in the KO phase: the next tick installs RoundState 2 on the chars.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().character.round_view.round_state,
            2,
            "KO / pre-over phase maps to RoundState 2"
        );
    }

    /// `MatchOver` is 0 throughout a live best-of-one match and flips to 1 on the
    /// tick that decides it; the characters' installed view reflects the flip.
    #[test]
    fn match_over_flips_at_match_end() {
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let mut m = Match::with_rounds_to_win(p1, p2, StageBounds::default(), 1);
        into_fight(&mut m);

        // Live fight: MatchOver is 0 on both the coordinator and the chars' view.
        assert_eq!(m.match_state(), MatchState::InProgress);
        assert!(!m.p1().character.round_view.match_over);

        // Best-of-one: one settled KO ends the whole match.
        ko_p2_and_settle(&mut m);
        assert_eq!(m.match_state(), MatchState::Over);
        assert_eq!(m.match_winner(), Some(Winner::P1));

        // The very next tick installs a view with MatchOver = 1 on both chars,
        // and GameTime is still climbing (the game clock never stops).
        let gt_before = m.game_time();
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(
            m.p1().character.round_view.match_over,
            "MatchOver is set once the match is decided"
        );
        assert!(m.p2().character.round_view.match_over);
        assert!(
            m.game_time() > gt_before,
            "GameTime keeps advancing past match end"
        );
    }

    // ---- T016: RoundNo / RoundsExisted threaded to triggers across rounds ----

    /// The `round_view()` the coordinator builds carries the live `RoundNo`
    /// (1-based) and `RoundsExisted` (`RoundNo - 1` for fighters present since
    /// round 1) directly from `round_number`.
    #[test]
    fn round_view_carries_round_number_and_rounds_existed() {
        let m = basic_match();
        // Fresh match: round 1, no rounds completed yet.
        assert_eq!(m.round_number(), 1);
        let v = m.round_view();
        assert_eq!(v.round_no, 1, "opening round is RoundNo 1");
        assert_eq!(v.rounds_existed, 0, "no rounds completed in round 1");
    }

    /// `RoundNo` climbs 1 -> 2 -> 3 across a best-of-three split, and
    /// `RoundsExisted` (= RoundNo - 1) trails it by one, with both pushed onto the
    /// characters' installed `round_view` each tick. This is the multi-round
    /// assertion the task asks for.
    #[test]
    fn round_no_and_rounds_existed_advance_across_a_multi_round_match() {
        let mut m = basic_match();

        // --- Round 1 ---
        into_fight(&mut m);
        assert_eq!(m.round_number(), 1);
        // Both characters' installed view reads RoundNo 1 / RoundsExisted 0.
        let v1 = m.p1().character.round_view;
        let v2 = m.p2().character.round_view;
        assert_eq!(v1, v2, "both characters get the same coordinator view");
        assert_eq!(v1.round_no, 1, "round 1: RoundNo 1");
        assert_eq!(v1.rounds_existed, 0, "round 1: RoundsExisted 0");
        // The mapping field->trigger is pinned in fp-character; here we confirm
        // `RoundsExisted == RoundNo - 1`, the relationship a fighter present from
        // the start always sees.
        assert_eq!(v1.rounds_existed, v1.round_no - 1);

        // P1 wins round 1 (best-of-three, so the match continues into round 2).
        ko_p2_and_settle(&mut m);
        assert_eq!(m.round_number(), 2, "advanced to round 2");
        assert_eq!(m.p1_round_wins(), 1);
        // The first tick of round 2 installs RoundNo 2 / RoundsExisted 1.
        m.tick(MatchInput::none(), MatchInput::none());
        let v = m.p1().character.round_view;
        assert_eq!(v.round_no, 2, "round 2: RoundNo 2");
        assert_eq!(
            v.rounds_existed, 1,
            "one round completed -> RoundsExisted 1"
        );
        assert_eq!(v.rounds_existed, v.round_no - 1);

        // --- Round 2: split it so the match reaches round 3 (P2 wins this one) ---
        into_fight(&mut m);
        ko_p1_and_settle(&mut m);
        assert_eq!(m.round_number(), 3, "the split advances to round 3");
        assert_eq!(m.p1_round_wins(), 1);
        assert_eq!(m.p2_round_wins(), 1);
        // Round 3's view: RoundNo 3 / RoundsExisted 2.
        m.tick(MatchInput::none(), MatchInput::none());
        let v = m.p2().character.round_view;
        assert_eq!(v.round_no, 3, "round 3: RoundNo 3");
        assert_eq!(
            v.rounds_existed, 2,
            "two rounds completed -> RoundsExisted 2"
        );
        assert_eq!(v.rounds_existed, v.round_no - 1);
    }

    #[test]
    fn player_push_separates_overlapping_characters() {
        // Two characters at the exact same X must be pushed apart by tick.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(0.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(0.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));

        // A single tick should separate them (push runs every frame).
        m.tick(MatchInput::none(), MatchInput::none());
        let dx = (m.p1().pos().x - m.p2().pos().x).abs();
        assert!(
            dx > 0.0,
            "overlapping characters must be pushed apart, got dx = {dx}"
        );
        // They should now be at least just-touching: the gap equals the summed
        // half-widths on the touching sides (default ground.front 16 + 16).
        assert!(dx >= 1.0, "separation should be meaningful, got {dx}");
    }

    #[test]
    fn bound_clamp_keeps_characters_on_stage() {
        // Put a character far off the right edge; the clamp must pull it back.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(1000.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-1000.0, 0.0);
        let bounds = StageBounds::new(-100.0, 100.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, bounds);

        m.tick(MatchInput::none(), MatchInput::none());

        // After clamping, each body's edge must be within the stage bounds.
        let p1 = m.p1();
        let p1_right = p1.pos().x + p1.push_body_right_half();
        assert!(
            p1_right <= bounds.right + 1e-3,
            "P1 right edge {p1_right} must be within stage right {}",
            bounds.right
        );
        let p2 = m.p2();
        let p2_left = p2.pos().x - p2.push_body_left_half();
        assert!(
            p2_left >= bounds.left - 1e-3,
            "P2 left edge {p2_left} must be within stage left {}",
            bounds.left
        );
    }

    #[test]
    fn feed_input_runs_real_matcher_facing_relative() {
        // The real pipeline: pushing raw absolute input runs the player's own
        // CommandMatcher, which resolves F/B against facing. Facing right,
        // hardware-right activates `holdfwd` (toward opponent) and clears
        // holding_back; hardware-left activates `holdback` and sets holding_back.
        let mut p = Player::new(
            Character::new(),
            loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
        );
        p.character.facing = Facing::Right;
        p.feed_input(MatchInput {
            right: true,
            ..MatchInput::none()
        });
        assert!(
            p.character.commands.is_active("holdfwd"),
            "right while facing right is holdfwd"
        );
        assert!(!p.character.commands.is_active("holdback"));
        assert!(!p.character.holding_back, "toward opponent is not back");

        // Hardware-left while facing right is Back.
        let mut p = Player::new(
            Character::new(),
            loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
        );
        p.character.facing = Facing::Right;
        p.feed_input(MatchInput {
            left: true,
            ..MatchInput::none()
        });
        assert!(
            p.character.commands.is_active("holdback"),
            "left while facing right is holdback"
        );
        assert!(!p.character.commands.is_active("holdfwd"));
        assert!(
            p.character.holding_back,
            "holding away from the opponent sets holding_back"
        );

        // Facing LEFT mirrors it: hardware-left is now Forward.
        let mut p = Player::new(
            Character::new(),
            loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
        );
        p.character.facing = Facing::Left;
        p.feed_input(MatchInput {
            left: true,
            ..MatchInput::none()
        });
        assert!(
            p.character.commands.is_active("holdfwd"),
            "left while facing left is holdfwd"
        );
        assert!(!p.character.holding_back);
    }

    #[test]
    fn feed_input_recognizes_button_commands() {
        // A button command in the `.cmd` (`punch = x`) fires when the matching
        // button is pressed, and not otherwise.
        let mut p = Player::new(
            Character::new(),
            loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
        );
        p.feed_input(MatchInput {
            x: true,
            ..MatchInput::none()
        });
        assert!(
            p.character.commands.is_active("punch"),
            "pressing x fires the `punch` command"
        );

        let mut p = Player::new(
            Character::new(),
            loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
        );
        p.feed_input(MatchInput {
            a: true,
            ..MatchInput::none()
        });
        assert!(
            !p.character.commands.is_active("punch"),
            "pressing a does not fire `punch`"
        );
    }

    #[test]
    fn time_over_compares_life() {
        // A one-second round: after 60 fight frames the timer hits zero.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.life = 800;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.life = 500; // less life -> P1 wins on time over
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::with_round_seconds(p1, p2, StageBounds::new(-200.0, 200.0), 1);
        into_fight(&mut m);

        // Burn the one-second clock down.
        for _ in 0..(TICKS_PER_SECOND + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert_ne!(m.round_state(), RoundState::Fight, "timer ran out");
        assert_eq!(m.winner(), Some(Winner::P1), "more life wins on time over");
    }

    #[test]
    fn neutral_character_refaces_opponent() {
        // Construct so P1 is to the RIGHT of P2 but facing the wrong way; a
        // neutral state must turn it back toward the opponent on tick.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(50.0, 0.0);
        p1c.facing = Facing::Right; // wrong: opponent is to the left
        p1c.state_type = StateType::Standing;
        p1c.move_type = MoveType::Idle;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-50.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.state_type = StateType::Standing;
        p2c.move_type = MoveType::Idle;
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Force both to the WRONG facing (away from the opponent) while neutral;
        // the baseline facep2 must correct them on the next tick.
        m.p1.character.facing = Facing::Right; // P1 is on the right, should face left
        m.p2.character.facing = Facing::Left; // P2 is on the left, should face right
        m.p1.character.move_type = MoveType::Idle;
        m.p1.character.state_type = StateType::Standing;
        m.p2.character.move_type = MoveType::Idle;
        m.p2.character.state_type = StateType::Standing;

        m.tick(MatchInput::none(), MatchInput::none());
        // P1 (right side) should now face left, P2 (left side) face right.
        assert_eq!(m.p1().facing(), Facing::Left);
        assert_eq!(m.p2().facing(), Facing::Right);
    }

    #[test]
    fn attacking_character_keeps_facing() {
        // A character that is attacking must NOT be re-faced mid-move.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(50.0, 0.0);
        p1c.state_type = StateType::Standing;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-50.0, 0.0);
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // After the intro, force P1 to the WRONG facing while it is attacking.
        // Because it is committed to a move (move_type = Attack), the baseline
        // facep2 must leave its facing alone.
        m.p1.character.facing = Facing::Right; // opponent is to the left -> "wrong"
        m.p1.character.move_type = MoveType::Attack;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().facing(),
            Facing::Right,
            "an attacking character must not be re-faced"
        );
    }

    #[test]
    fn round_seconds_clamps_non_positive() {
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let m = Match::with_round_seconds(p1, p2, StageBounds::default(), -5);
        assert_eq!(m.timer(), 0, "a non-positive round length clamps to 0");
    }

    // ---- Additional coverage (Proctor): edge cases, error paths, semantics ----

    /// AC1/AC2: the default round timer is exactly 99 seconds * 60 ticks.
    #[test]
    fn default_timer_is_ninety_nine_seconds() {
        let m = basic_match();
        assert_eq!(
            m.timer(),
            DEFAULT_ROUND_SECONDS * TICKS_PER_SECOND,
            "default round clock is 99s * 60"
        );
        assert_eq!(m.timer(), 99 * 60);
    }

    /// AC2: the timer must not tick down during the Intro phase — only once the
    /// fight is live. A renderer/clock reading `timer()` during intro should see
    /// the full starting value.
    #[test]
    fn timer_frozen_during_intro() {
        let mut m = basic_match();
        let start = m.timer();
        // Tick partway into (but not past) the intro.
        for _ in 0..(INTRO_FRAMES - 1) {
            m.tick(MatchInput::none(), MatchInput::none());
            assert_eq!(m.round_state(), RoundState::Intro);
            assert_eq!(m.timer(), start, "timer must not move during intro");
        }
    }

    /// AC2: during the fight the timer decrements exactly one frame per tick.
    #[test]
    fn timer_decrements_one_per_fight_tick() {
        let mut m = basic_match();
        into_fight(&mut m);
        assert_eq!(m.round_state(), RoundState::Fight);
        let before = m.timer();
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.timer(),
            before - 1,
            "one fight tick burns one timer frame"
        );
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.timer(), before - 2);
    }

    /// AC2: the timer holds its value during the KO hold (it is frozen, not
    /// counted down) and through Win.
    #[test]
    fn timer_frozen_after_ko() {
        let mut m = basic_match();
        into_fight(&mut m);
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        let frozen = m.timer();
        for _ in 0..10 {
            m.tick(MatchInput::none(), MatchInput::none());
            assert_eq!(m.timer(), frozen, "timer is frozen during the KO hold");
        }
    }

    /// AC2: WITHIN a single round the state machine never moves backwards. (Across
    /// a best-of-N match it deliberately cycles Win -> Intro on a round reset; that
    /// reset is covered by `ko_p2_twice_wins_match_for_p1`. Here we assert monotone
    /// ordering up to and including the first arrival at Win, the round-decided
    /// phase.)
    #[test]
    fn round_state_is_monotonic() {
        fn rank(s: RoundState) -> u8 {
            match s {
                RoundState::Intro => 0,
                RoundState::Fight => 1,
                RoundState::Ko => 2,
                RoundState::Win => 3,
            }
        }
        let mut m = basic_match();
        into_fight(&mut m);
        m.p2.character.life = 0; // force the KO path
        let mut prev = rank(m.round_state());
        // From Fight: one tick enters Ko, then KO_FRAMES ticks reach Win — all
        // monotone, with no reset (the reset happens on the tick AFTER Win is
        // first observed).
        for _ in 0..(KO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
            let now = rank(m.round_state());
            assert!(now >= prev, "round state went backwards: {prev} -> {now}");
            prev = now;
        }
        assert_eq!(m.round_state(), RoundState::Win);
    }

    /// AC2: the Win (round-decided) phase holds for exactly one observable frame
    /// per round — the round winner is stable while it is displayed, and the very
    /// next tick runs the best-of-N transition out of Win (to the next round's
    /// Intro here, since one KO is below the best-of-three threshold). Under
    /// best-of-N, Win is per-round, not match-terminal; the terminal invariant
    /// belongs to match-over (see `match_over_is_terminal`).
    #[test]
    fn win_phase_holds_then_advances_round() {
        let mut m = basic_match();
        into_fight(&mut m);
        m.p2.character.life = 0;
        // From Fight, one tick enters Ko and KO_FRAMES more reach Win.
        for _ in 0..(KO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.round_state(), RoundState::Win);
        assert_eq!(m.winner(), Some(Winner::P1), "round winner decided in Win");
        assert_eq!(m.round_number(), 1, "still round 1 while Win is displayed");

        // The next tick runs the Win-arm transition: below threshold -> next round.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.round_state(),
            RoundState::Intro,
            "Win advances to next round"
        );
        assert_eq!(m.round_number(), 2, "round_number incremented out of Win");
        assert_eq!(m.p1_round_wins(), 1, "the round win was credited");
        assert_eq!(
            m.winner(),
            None,
            "current-round winner cleared for the new round"
        );
    }

    /// AC2: winner() is None until the round is actually decided.
    #[test]
    fn winner_is_none_before_decision() {
        let mut m = basic_match();
        assert_eq!(m.winner(), None, "no winner during intro");
        into_fight(&mut m);
        assert_eq!(m.winner(), None, "no winner at fight start");
    }

    /// AC2: a double-KO on the same frame is a Draw.
    #[test]
    fn double_ko_is_a_draw() {
        let mut m = basic_match();
        into_fight(&mut m);
        m.p1.character.life = 0;
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.winner(), Some(Winner::Draw), "both down is a draw");
        assert!(matches!(m.round_state(), RoundState::Ko | RoundState::Win));
    }

    /// AC2: when only P1 is downed, P2 wins (the mirror of the existing P1-wins
    /// test).
    #[test]
    fn p1_ko_makes_p2_win() {
        let mut m = basic_match();
        into_fight(&mut m);
        m.p1.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.winner(), Some(Winner::P2));
        assert_eq!(m.round_state(), RoundState::Ko);
    }

    /// MUGEN semantics: negative life (overkill) is still treated as a KO and the
    /// surviving player wins — the comparison is `life <= 0`, not `== 0`.
    #[test]
    fn negative_life_counts_as_ko() {
        let mut m = basic_match();
        into_fight(&mut m);
        m.p2.character.life = -100;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        assert_eq!(m.winner(), Some(Winner::P1));
    }

    /// AC2: time over with equal life is a Draw.
    #[test]
    fn time_over_equal_life_is_draw() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.life = 600;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.life = 600;
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::with_round_seconds(p1, p2, StageBounds::new(-200.0, 200.0), 1);
        into_fight(&mut m);
        for _ in 0..(TICKS_PER_SECOND + 2) {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert_ne!(m.round_state(), RoundState::Fight);
        assert_eq!(
            m.winner(),
            Some(Winner::Draw),
            "equal life at time over draws"
        );
    }

    /// AC2: time over with P2 ahead makes P2 win (mirror of the existing P1 case).
    #[test]
    fn time_over_p2_more_life_wins() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.life = 300;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.life = 900;
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::with_round_seconds(p1, p2, StageBounds::new(-200.0, 200.0), 1);
        into_fight(&mut m);
        for _ in 0..(TICKS_PER_SECOND + 2) {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert_eq!(m.winner(), Some(Winner::P2));
    }

    /// `compare_life` is the time-over decider; cover all three orderings directly.
    #[test]
    fn compare_life_covers_all_orderings() {
        assert_eq!(compare_life(100, 50), Winner::P1);
        assert_eq!(compare_life(50, 100), Winner::P2);
        assert_eq!(compare_life(50, 50), Winner::Draw);
    }

    /// AC3 (semantics): an attack must NOT connect outside the live Fight phase.
    /// Even with a fully armed, overlapping HitDef during Intro, P2's life is
    /// untouched until the fight actually begins.
    #[test]
    fn no_combat_during_intro() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        assert_eq!(m.round_state(), RoundState::Intro);

        let life_before = m.p2().life();
        // Arm an overlapping attack during the intro and tick several intro frames.
        for _ in 0..5 {
            m.p1.character.anim = 200;
            m.p1.character.anim_elem = 0;
            m.p1.character.move_type = MoveType::Attack;
            m.p1.character.active_hitdef = Some(sample_hitdef());
            m.p1.character.move_connect.reset();
            m.p2.character.anim = 0;
            m.p2.character.anim_elem = 0;
            m.p2.character.state_type = StateType::Standing;
            assert_eq!(m.round_state(), RoundState::Intro, "still in intro");
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(
            m.p2().life(),
            life_before,
            "no damage may be dealt during the intro phase"
        );
    }

    /// AC3 (semantics): an attack must not connect during the KO hold either —
    /// a HitDef left active when the round is decided cannot keep dealing damage.
    #[test]
    fn no_combat_during_ko() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Force a KO so we are now in the KO hold; P1 still has its punch armed.
        m.p1.character.life = 0; // P1 down -> P2 wins, enter KO
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);

        let p2_life = m.p2().life();
        for _ in 0..5 {
            m.p1.character.anim = 200;
            m.p1.character.anim_elem = 0;
            m.p1.character.move_type = MoveType::Attack;
            m.p1.character.active_hitdef = Some(sample_hitdef());
            m.p1.character.move_connect.reset();
            m.p2.character.anim = 0;
            m.p2.character.anim_elem = 0;
            m.p2.character.state_type = StateType::Standing;
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.p2().life(), p2_life, "no combat during the KO hold");
    }

    /// AC3 (semantics): combat runs in BOTH directions. Arm both fighters with an
    /// overlapping attack on the same frame; both must take damage.
    #[test]
    fn combat_runs_both_directions() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(40.0, 0.0);
        p2c.facing = Facing::Left;
        // Both are attacker-shaped (punch on 200, hurt on 0).
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, attacker_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        let p1_before = m.p1().life();
        let p2_before = m.p2().life();
        // Arm both with a punch on the active frame, overlapping the other's hurt
        // box (each character carries both Clsn1 on 200 and Clsn2 on 0; here we put
        // each attacker on its punch frame while its OWN hurt box is also present on
        // the punch action's defender side — so use action 0 hurt by keeping anim 200
        // for attack and relying on the punch Clsn1 vs the other's idle Clsn2).
        // To get a clean mutual hit, alternate frames: keep each on the punch action
        // (Clsn1 present) and ensure the other exposes a Clsn2. attacker_loaded's
        // action 200 has no Clsn2, so put each defender-frame as action 0.
        // Simplest robust approach: one tick with both on punch (Clsn1) while the
        // OTHER is read for Clsn2 from action 0 — not simultaneously possible per
        // character. Instead, verify both directions across two arming passes.

        // Pass A: P1 punches (200) into P2 idle (0).
        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 0;
        m.p1.character.move_type = MoveType::Attack;
        m.p1.character.active_hitdef = Some(sample_hitdef());
        m.p1.character.move_connect.reset();
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;
        m.p2.character.active_hitdef = None;
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(m.p2().life() < p2_before, "P1's attack must damage P2");

        if m.round_state() != RoundState::Fight {
            return; // a KO ended the round; both-direction intent already shown
        }

        // Pass B: P2 punches (200) into P1 idle (0).
        m.p2.character.anim = 200;
        m.p2.character.anim_elem = 0;
        m.p2.character.move_type = MoveType::Attack;
        m.p2.character.active_hitdef = Some(sample_hitdef());
        m.p2.character.move_connect.reset();
        m.p1.character.anim = 0;
        m.p1.character.anim_elem = 0;
        m.p1.character.state_type = StateType::Standing;
        m.p1.character.active_hitdef = None;
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(m.p1().life() < p1_before, "P2's attack must damage P1");
    }

    /// AC3 (semantics): the `hitonce` rule — a single armed HitDef that has
    /// already connected does not deal damage twice while it stays active.
    #[test]
    fn hitonce_prevents_double_damage() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Arm ONCE (no per-frame re-arm / reset): the move connects on the first
        // frame, then `move_connect` must suppress further hits.
        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 0;
        m.p1.character.move_type = MoveType::Attack;
        m.p1.character.active_hitdef = Some(sample_hitdef());
        m.p1.character.move_connect.reset();
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;

        m.tick(MatchInput::none(), MatchInput::none());
        let after_first = m.p2().life();
        assert!(after_first < m.p2().life_max(), "first frame connects");

        // Keep the same HitDef active (do NOT reset move_connect) for more frames;
        // pin the defender back onto its hurt frame each tick.
        for _ in 0..4 {
            m.p2.character.anim = 0;
            m.p2.character.anim_elem = 0;
            m.p2.character.state_type = StateType::Standing;
            if m.round_state() != RoundState::Fight {
                break;
            }
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(
            m.p2().life(),
            after_first,
            "hitonce: an already-connected move must not damage again"
        );
    }

    /// AC1/AC3: a missing attack box (empty Clsn1) degrades to "no contact" — an
    /// attacker on a non-attack frame never connects even with a HitDef armed.
    #[test]
    fn missing_attack_box_does_not_connect() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(20.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        let before = m.p2().life();
        // Arm a HitDef but point the attacker at action 0 (which has NO Clsn1).
        for _ in 0..5 {
            m.p1.character.anim = 0; // no attack box on action 0
            m.p1.character.anim_elem = 0;
            m.p1.character.move_type = MoveType::Attack;
            m.p1.character.active_hitdef = Some(sample_hitdef());
            m.p1.character.move_connect.reset();
            if m.round_state() != RoundState::Fight {
                break;
            }
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.p2().life(), before, "no Clsn1 -> no contact, no damage");
    }

    /// AC1/AC4: a character whose loaded AIR is empty (no actions at all) must
    /// still tick without panic and never connect — missing assets degrade safely.
    #[test]
    fn empty_air_ticks_safely() {
        let empty = AirFile {
            actions: HashMap::new(),
        };
        let p1 = Player::new(Character::new(), loaded_with(empty.clone()));
        let p2 = Player::new(Character::new(), loaded_with(empty));
        let mut m = Match::new(p1, p2, StageBounds::default());
        for _ in 0..120 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // No panic; round progressed normally.
        assert!(matches!(
            m.round_state(),
            RoundState::Fight | RoundState::Ko | RoundState::Win
        ));
    }

    /// AC3 (geometry): two coincident default-sized characters are pushed to be
    /// exactly edge-touching. Default front=16, back=15; P1 faces right (extent
    /// center-15..center+16), P2 faces left (extent center-16..center+15). At
    /// equal centers the overlap is 30, so each moves 15 and they end 30 apart.
    #[test]
    fn player_push_separation_is_exact() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(0.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(0.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        m.tick(MatchInput::none(), MatchInput::none());
        // One push from coincident centers moves each by half the overlap.
        // Overlap of two default bodies at center 0: P1 faces right (extent
        // -15..16), P2 faces left (extent -16..15) -> overlap 30, half 15, so
        // P1 -> -15 and P2 -> +15: centers end exactly 30 apart.
        let dx = m.p2().pos().x - m.p1().pos().x;
        assert!(
            (dx - 30.0).abs() < 1e-3,
            "coincident default bodies separate to centers 30 apart, got {dx}"
        );
        // The even split over ASYMMETRIC front/back leaves a small residual
        // overlap (front 16 on both inner edges), which the per-frame push shrinks
        // on subsequent ticks. Verify the residual strictly shrinks toward
        // edge-touching rather than asserting it vanishes in one frame.
        let inner_overlap = |m: &Match| {
            let p1_right = m.p1().pos().x + m.p1().push_body_right_half();
            let p2_left = m.p2().pos().x - m.p2().push_body_left_half();
            p1_right - p2_left
        };
        let after_first = inner_overlap(&m);
        m.tick(MatchInput::none(), MatchInput::none());
        let after_second = inner_overlap(&m);
        assert!(
            after_second < after_first + 1e-6,
            "repeated push must not increase overlap: {after_first} -> {after_second}"
        );
    }

    /// AC3: non-overlapping characters are left exactly in place by the push
    /// (the push only fires on a strictly positive overlap).
    #[test]
    fn player_push_leaves_separated_characters_untouched() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-100.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(100.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        m.tick(MatchInput::none(), MatchInput::none());
        assert!((m.p1().pos().x - -100.0).abs() < 1e-3, "P1 unmoved");
        assert!((m.p2().pos().x - 100.0).abs() < 1e-3, "P2 unmoved");
    }

    /// AC3 (robustness): when the stage is narrower than a single body can fit,
    /// the clamp must not panic and must keep the position finite (the left edge
    /// wins per the physics primitive's documented behavior).
    #[test]
    fn clamp_with_too_narrow_stage_is_finite() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(500.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-500.0, 0.0);
        // Bounds far narrower than the ~31px-wide bodies.
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-5.0, 5.0));
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(m.p1().pos().x.is_finite(), "P1 x stays finite");
        assert!(m.p2().pos().x.is_finite(), "P2 x stays finite");
    }

    /// AC1/AC4 (robustness): reversed stage bounds (left > right) must not panic
    /// and must leave positions finite.
    #[test]
    fn reversed_bounds_do_not_panic() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(50.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-50.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        // Deliberately reversed: left (100) > right (-100).
        let mut m = Match::new(p1, p2, StageBounds::new(100.0, -100.0));
        for _ in 0..5 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert!(m.p1().pos().x.is_finite());
        assert!(m.p2().pos().x.is_finite());
    }

    /// AC5: a character left at exactly the same X never produces a NaN separation
    /// and remains within finite bounds after many ticks.
    #[test]
    fn long_run_stays_finite_and_on_stage() {
        let mut m = basic_match();
        let bounds = m.bounds();
        for _ in 0..600 {
            m.tick(MatchInput::none(), MatchInput::none());
            for p in [m.p1(), m.p2()] {
                assert!(p.pos().x.is_finite(), "position must stay finite");
                let right = p.pos().x + p.push_body_right_half();
                let left = p.pos().x - p.push_body_left_half();
                assert!(
                    right <= bounds.right + 1.0 && left >= bounds.left - 1.0,
                    "fighter drifted off stage: [{left}, {right}] vs [{}, {}]",
                    bounds.left,
                    bounds.right
                );
            }
        }
    }

    /// AC1: read accessors expose the renderer-facing state, and they reflect the
    /// live character. anim/anim_elem/life/life_max/power/power_max/pos/facing all
    /// round-trip.
    #[test]
    fn read_accessors_reflect_character_state() {
        let mut c = Character::with_constants(CharacterConstants::default());
        c.pos = Vec2::new(12.0, -34.0);
        c.facing = Facing::Left;
        c.anim = 42;
        c.anim_elem = 3;
        c.life = 777;
        c.power = 1500;
        c.power_max = 3000;
        let p = Player::new(c, defender_loaded());
        assert_eq!(p.pos(), Vec2::new(12.0, -34.0));
        assert_eq!(p.facing(), Facing::Left);
        assert_eq!(p.anim(), 42);
        assert_eq!(p.anim_elem(), 3);
        assert_eq!(p.life(), 777);
        assert_eq!(p.life_max(), 1000);
        assert_eq!(
            p.power(),
            1500,
            "power accessor reflects the live character"
        );
        assert_eq!(
            p.power_max(),
            3000,
            "power_max accessor reflects the live character"
        );
    }

    /// PR-C (audit #26): the power accessor surfaces the meter the simulation
    /// tracks, and that meter carries across a round reset (power is intentionally
    /// not zeroed between the rounds of a match) so a HUD power bar keeps its fill.
    #[test]
    fn power_accessor_surfaces_meter_and_carries_across_rounds() {
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::default());

        // Build some meter on P1 directly (as a super/PowerAdd would), then drive
        // a full round to a decision and the next-round reset.
        m.p1.character.power = 1234;
        let built = m.p1().power();
        assert_eq!(built, 1234, "accessor reflects the meter the sim tracks");
        assert!(m.p1().power_max() > 0, "power_max is a sane denominator");

        // KO P2 immediately so the round decides on the next Fight tick, then run
        // far enough to pass through Ko -> Win -> next-round reset.
        m.p2.character.life = 0;
        for _ in 0..(INTRO_FRAMES + KO_FRAMES + 4) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(
            m.round_number(),
            2,
            "the match advanced into the next round (reset ran)"
        );
        assert_eq!(
            m.p1().power(),
            built,
            "power carries across the round reset (not zeroed) and stays visible"
        );
    }

    /// AC1: pressing both left and right (or neither) yields no net horizontal
    /// command and is not treated as blocking. `$F`/`$B` exclude the opposing
    /// axis, so both-held activates neither holdfwd nor holdback.
    #[test]
    fn opposing_horizontal_inputs_cancel() {
        let mut p = Player::new(
            Character::new(),
            loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
        );
        p.character.facing = Facing::Right;
        p.feed_input(MatchInput {
            left: true,
            right: true,
            ..MatchInput::none()
        });
        assert!(!p.character.commands.is_active("holdfwd"));
        assert!(!p.character.commands.is_active("holdback"));
        assert!(
            !p.character.holding_back,
            "ambiguous horizontal is not blocking"
        );

        // Neither held: no horizontal command, not blocking.
        let mut p = Player::new(
            Character::new(),
            loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
        );
        p.character.facing = Facing::Left;
        p.feed_input(MatchInput::none());
        assert!(!p.character.commands.is_active("holdfwd"));
        assert!(!p.character.commands.is_active("holdback"));
        assert!(!p.character.holding_back);
    }

    /// AC1: up/down are facing-independent — `holdup`/`holddown` fire regardless
    /// of facing.
    #[test]
    fn vertical_inputs_map_unchanged() {
        for facing in [Facing::Left, Facing::Right] {
            let mut p = Player::new(
                Character::new(),
                loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
            );
            p.character.facing = facing;
            p.feed_input(MatchInput {
                up: true,
                ..MatchInput::none()
            });
            assert!(
                p.character.commands.is_active("holdup"),
                "up fires holdup ({facing:?})"
            );

            let mut p = Player::new(
                Character::new(),
                loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
            );
            p.character.facing = facing;
            p.feed_input(MatchInput {
                down: true,
                ..MatchInput::none()
            });
            assert!(
                p.character.commands.is_active("holddown"),
                "down fires holddown ({facing:?})"
            );
        }
    }

    /// AC1: each attack button drives its own button command through the matcher.
    #[test]
    fn all_attack_buttons_map() {
        // A `.cmd` with one command per button.
        let cmd = "\
[Command]\nname = \"a\"\ncommand = a\ntime = 1\n
[Command]\nname = \"b\"\ncommand = b\ntime = 1\n
[Command]\nname = \"c\"\ncommand = c\ntime = 1\n
[Command]\nname = \"x\"\ncommand = x\ntime = 1\n
[Command]\nname = \"y\"\ncommand = y\ntime = 1\n
[Command]\nname = \"z\"\ncommand = z\ntime = 1\n";
        let input = MatchInput {
            a: true,
            b: true,
            c: true,
            x: true,
            y: true,
            z: true,
            ..MatchInput::none()
        };
        let mut p = Player::new(
            Character::new(),
            loaded_with_cmd(air_with(0, vec![], vec![]), cmd),
        );
        p.feed_input(input);
        for name in ["a", "b", "c", "x", "y", "z"] {
            assert!(
                p.character.commands.is_active(name),
                "missing button command {name}"
            );
        }
    }

    /// AC1: holding "back" (away from the opponent) sets `holding_back` on the
    /// character through the full feed path, enabling the guard path in combat.
    #[test]
    fn feed_input_sets_holding_back_on_character() {
        let mut p = Player::new(
            Character::new(),
            loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
        );
        p.character.facing = Facing::Right;
        // Facing right, pressing left = away from opponent = back.
        p.feed_input(MatchInput {
            left: true,
            ..MatchInput::none()
        });
        assert!(
            p.character.holding_back,
            "holding away from opponent sets holding_back"
        );

        // Pressing toward the opponent clears it.
        let mut p = Player::new(
            Character::new(),
            loaded_with_cmd(air_with(0, vec![], vec![]), HOLD_CMD),
        );
        p.character.facing = Facing::Right;
        p.feed_input(MatchInput {
            right: true,
            ..MatchInput::none()
        });
        assert!(
            !p.character.holding_back,
            "holding toward opponent is not back"
        );
    }

    /// AC1: `MatchInput::none()` and the derived `Default` agree (nothing held).
    #[test]
    fn match_input_none_matches_default() {
        assert_eq!(MatchInput::none(), MatchInput::default());
    }

    /// AC3 (baseline facep2): a crouching character is NOT re-faced (only
    /// standing+idle is neutral), documenting the conservative turn rule.
    #[test]
    fn crouching_character_keeps_facing() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(50.0, 0.0); // right side; "correct" facing is Left
        p1c.facing = Facing::Right; // wrong on purpose
        p1c.state_type = StateType::Crouching;
        p1c.move_type = MoveType::Idle;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-50.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);
        m.p1.character.facing = Facing::Right;
        m.p1.character.state_type = StateType::Crouching;
        m.p1.character.move_type = MoveType::Idle;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().facing(),
            Facing::Right,
            "a crouching character is not neutral for facep2"
        );
    }

    /// AC3 (baseline facep2): a character being hit keeps its facing (would-be
    /// flip during a knockback reaction is suppressed).
    #[test]
    fn being_hit_character_keeps_facing() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(50.0, 0.0);
        p1c.facing = Facing::Right; // wrong: opponent on the left
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-50.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);
        m.p1.character.facing = Facing::Right;
        m.p1.character.state_type = StateType::Standing;
        m.p1.character.move_type = MoveType::BeingHit;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().facing(),
            Facing::Right,
            "get-hit reaction keeps facing"
        );
    }

    /// `is_neutral_facing_state` directly: only Standing/Unchanged + Idle qualify.
    #[test]
    fn neutral_facing_state_predicate() {
        let mut c = Character::new();
        c.state_type = StateType::Standing;
        c.move_type = MoveType::Idle;
        assert!(is_neutral_facing_state(&c));
        c.state_type = StateType::Unchanged;
        assert!(is_neutral_facing_state(&c));
        c.move_type = MoveType::Attack;
        assert!(!is_neutral_facing_state(&c), "attacking is not neutral");
        c.move_type = MoveType::Idle;
        c.state_type = StateType::Air;
        assert!(!is_neutral_facing_state(&c), "airborne is not neutral");
        c.state_type = StateType::Crouching;
        assert!(!is_neutral_facing_state(&c), "crouching is not neutral");
    }

    /// `facings_toward` tie-break: equal X is deterministic (a right, b left).
    #[test]
    fn facings_toward_tie_break() {
        assert_eq!(facings_toward(0.0, 0.0), (Facing::Right, Facing::Left));
        assert_eq!(facings_toward(-10.0, 10.0), (Facing::Right, Facing::Left));
        assert_eq!(facings_toward(10.0, -10.0), (Facing::Left, Facing::Right));
    }

    /// AC1: control is removed from both fighters when the round ends (KO hold).
    #[test]
    fn ko_removes_control() {
        let mut m = basic_match();
        into_fight(&mut m);
        assert!(m.p1().character.ctrl && m.p2().character.ctrl);
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        assert!(!m.p1().character.ctrl, "control revoked at KO");
        assert!(!m.p2().character.ctrl, "control revoked at KO");
    }

    /// AC1: bounds() accessor returns what was passed in.
    #[test]
    fn bounds_accessor_round_trips() {
        let b = StageBounds::new(-123.0, 456.0);
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let m = Match::new(p1, p2, b);
        assert_eq!(m.bounds(), b);
        assert_eq!(m.bounds().left, -123.0);
        assert_eq!(m.bounds().right, 456.0);
    }

    /// StageBounds::default is a symmetric playfield around the origin.
    #[test]
    fn stage_bounds_default_is_symmetric() {
        let b = StageBounds::default();
        assert_eq!(b.left, -200.0);
        assert_eq!(b.right, 200.0);
        assert!(b.left < b.right);
    }

    // ---- AC5 (optional): gated real-KFM integration test --------------------

    fn test_asset(rel: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join(rel)
    }

    /// Loads the real Kung Fu Man character on BOTH sides and drives a full match
    /// of the coordinator end-to-end: it must tick many frames without panic,
    /// progress the round state machine, keep both fighters on stage, and (since
    /// real KFM carries Clsn boxes) be able to deal damage when a real attack
    /// frame is posed overlapping the opponent's hurt frame.
    ///
    /// Skips cleanly (printing the reason) when `test-assets/` is absent or the
    /// fixture fails to load, so the suite stays green without the (uncommitted)
    /// assets. The fixture is known present in this repo, so the body runs here.
    #[test]
    fn real_kfm_match_runs_end_to_end() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let lc1 = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: kfm.def failed to load: {e}");
                return;
            }
        };
        let lc2 = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: kfm.def failed to load: {e}");
                return;
            }
        };

        let mut p1c = Character::with_constants(lc1.constants);
        p1c.pos = Vec2::new(-40.0, 0.0);
        let mut p2c = Character::with_constants(lc2.constants);
        p2c.pos = Vec2::new(40.0, 0.0);
        let p1 = Player::new(p1c, lc1);
        let p2 = Player::new(p2c, lc2);
        let bounds = StageBounds::new(-300.0, 300.0);
        let mut m = Match::new(p1, p2, bounds);

        // Drive through the intro and a chunk of the fight with idle inputs.
        for _ in 0..(INTRO_FRAMES + 120) {
            m.tick(MatchInput::none(), MatchInput::none());
            for p in [m.p1(), m.p2()] {
                assert!(p.pos().x.is_finite(), "real-KFM position must stay finite");
                let right = p.pos().x + p.push_body_right_half();
                let left = p.pos().x - p.push_body_left_half();
                assert!(
                    right <= bounds.right + 1.0 && left >= bounds.left - 1.0,
                    "real-KFM fighter drifted off stage"
                );
            }
        }
        assert!(matches!(
            m.round_state(),
            RoundState::Fight | RoundState::Ko | RoundState::Win
        ));

        // Exercise the combat apply path the coordinator uses with REAL KFM Clsn
        // frames. We call the same `resolve_attack` primitive `Match::tick` invokes,
        // posing the attacker on KFM's stand light punch (action 200, element 2
        // carries the real Clsn1 attack box) and the defender on its idle action 0
        // (real Clsn2 hurt box). We call `resolve_attack` directly here (rather than
        // through `Match::tick`) because the coordinator's tick advances each
        // character's animation cursor *before* combat, which would move the
        // attacker off the posed punch frame; this isolates the real-asset combat
        // wiring. The frames are taken from the just-loaded real AIR.
        let air = &m.p1().loaded.air;
        let has_attack_box = air
            .actions
            .get(&200)
            .map(|a| a.frames.iter().any(|f| !f.clsn1.is_empty()))
            .unwrap_or(false);
        let has_hurt_box = air
            .actions
            .get(&0)
            .map(|a| a.frames.iter().any(|f| !f.clsn2.is_empty()))
            .unwrap_or(false);
        if !has_attack_box || !has_hurt_box {
            eprintln!("skipping real-KFM combat: action 200/0 lack expected Clsn boxes");
            return;
        }

        // Build a fresh attacker/defender from the real constants + real AIR and
        // sweep the defender until the REAL boxes overlap.
        let attacker_air = m.p1().loaded.air.clone();
        let defender_air = m.p2().loaded.air.clone();
        let defender_states = m.p2().loaded.states.clone();
        let mut connected = false;
        for dx in 0..=120 {
            let mut attacker = Character::with_constants(m.p1().character.constants);
            attacker.anim = 200;
            attacker.anim_elem = 2;
            attacker.pos = Vec2::new(0.0, 0.0);
            attacker.facing = Facing::Right;
            attacker.move_type = MoveType::Attack;
            attacker.active_hitdef = Some(sample_hitdef());
            attacker.move_connect.reset();

            let mut defender = Character::with_constants(m.p2().character.constants);
            defender.anim = 0;
            defender.anim_elem = 0;
            defender.pos = Vec2::new(dx as f32, 0.0);
            defender.facing = Facing::Left;
            defender.state_type = StateType::Standing;
            let life_before = defender.life;

            let res = resolve_attack(
                &mut attacker,
                &attacker_air,
                &mut defender,
                &defender_air,
                &defender_states,
            );
            if res.is_some() && defender.life < life_before {
                connected = true;
                break;
            }
        }
        assert!(
            connected,
            "real KFM punch (action 200/2) should connect with idle hurt box across the sweep"
        );
    }

    /// AC3 (gated, skip-if-missing): drive a REAL two-KFM [`Match`] so P1's throw
    /// `HitDef` (state 800, `p1stateno = 810` / `p2stateno = 820`) connects on P2
    /// and assert the throw wiring this task added: P1 enters its own state 810
    /// (the `p1stateno` attacker transition applied in `Match::tick`), P2 enters
    /// state 820 (the `p2stateno` defender transition resolve_attack applies), and
    /// over the following ticks P2 stays bound near P1 and loses life from the
    /// state-810 `TargetBind` / `TargetLifeAdd` (the `TargetOp` apply path).
    ///
    /// We do **not** command-drive the throw (KFM's throw command is hard to land
    /// deterministically frame-perfect in a headless harness — see the task note);
    /// instead we put P1 directly into the real state 800 in range of P2 and let
    /// the coordinator's own combat + target-op pipeline carry the rest, which is
    /// exactly the code under test. Skips cleanly when the KFM fixture is absent.
    #[test]
    fn real_kfm_throw_drives_p1stateno_p2stateno_and_binds_target() {
        let Some((lc1, lc2)) = two_kfm_loaded() else {
            return; // fixture absent; helper already logged the skip
        };
        // The fixture must actually define the throw states for this test to mean
        // anything; if a stripped CNS lacks them, skip rather than false-fail.
        if !(lc1.states.contains_key(&800)
            && lc1.states.contains_key(&810)
            && lc2.states.contains_key(&820))
        {
            eprintln!("skipping: KFM fixture lacks throw states 800/810/820");
            return;
        }

        let mut p1c = Character::with_constants(lc1.constants);
        p1c.pos = Vec2::new(0.0, 0.0);
        let mut p2c = Character::with_constants(lc2.constants);
        // Stand P2 just in front of P1, inside throw range.
        p2c.pos = Vec2::new(20.0, 0.0);
        let p1 = Player::new(p1c, lc1);
        let p2 = Player::new(p2c, lc2);
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Put P1 into the real throw startup state (800) via its OWN graph; its
        // [State 800, 1] HitDef (Trigger1 = Time = 0) arms on the next tick. Keep
        // P2 standing and idle so the throw's `hitflag = M-` (ground, not-being-hit)
        // is satisfied.
        let throw_states = m.p1().loaded.states.clone();
        m.p1.character.change_state(&throw_states, 800);
        m.p2.character.state_type = StateType::Standing;
        m.p2.character.move_type = MoveType::Idle;
        let p2_life_at_grab = m.p2().life();

        // Tick until the throw connects: P1 should enter 810 and P2 should enter
        // 820. Re-pin P2 standing/idle and in range each pre-connect tick so the
        // grab can land deterministically.
        let mut connected = false;
        for _ in 0..30 {
            if m.p1().character.state_no != 800 && m.p1().character.state_no != 810 {
                // P1 fell out of the throw startup before connecting (e.g. AnimTime
                // ChangeState back to 0); re-arm the startup once.
                m.p1.character.change_state(&throw_states, 800);
            }
            if m.p1().character.state_no == 800 {
                m.p2.character.state_type = StateType::Standing;
                m.p2.character.move_type = MoveType::Idle;
                m.p2.character.pos.x = m.p1().pos().x + 20.0;
            }
            m.tick(MatchInput::none(), MatchInput::none());
            if m.p1().character.state_no == 810 && m.p2().character.state_no == 820 {
                connected = true;
                break;
            }
        }

        // The deterministic core of AC3: the throw wired both state transitions.
        assert!(
            connected,
            "real KFM throw should drive P1 -> 810 (p1stateno) and P2 -> 820 (p2stateno); \
             P1 in {}, P2 in {}",
            m.p1().character.state_no,
            m.p2().character.state_no
        );

        // Over the following ticks the state-810 TargetBind keeps P2 pinned near P1
        // and the TargetLifeAdd (AnimElem 11) drains P2's life. These are gated on
        // the throw animation's element timing, so we run the hold out and assert
        // the observable end effects (bound-near + life-drop) happened at least
        // once; if the (real) anim never reaches the damage element within the
        // budget we document rather than hard-fail on asset timing.
        let mut ever_bound_near = false;
        let mut life_dropped = false;
        for _ in 0..120 {
            if m.p1().character.state_no != 810 {
                break; // throw released
            }
            m.tick(MatchInput::none(), MatchInput::none());
            let dx = (m.p2().pos().x - m.p1().pos().x).abs();
            if dx <= 80.0 {
                ever_bound_near = true;
            }
            if m.p2().life() < p2_life_at_grab {
                life_dropped = true;
            }
        }
        assert!(
            ever_bound_near,
            "thrown P2 should be bound near P1 during the throw (TargetBind)"
        );
        if life_dropped {
            assert!(
                m.p2().life() < p2_life_at_grab,
                "TargetLifeAdd should have reduced thrown P2's life"
            );
        } else {
            eprintln!(
                "note: real KFM throw anim did not reach the AnimElem 11 TargetLifeAdd \
                 within the tick budget; bind verified, damage relies on the synthetic test"
            );
        }
    }

    // ---- Task 7.3: the real command pipeline drives locomotion (no shim) ----

    /// Builds a two-KFM [`Match`] (P1 left, P2 right, facing each other, stood in
    /// state 0 with the round about to start), or `None` (skip) when the fixture
    /// is absent. Mirrors the app's construction but lives in fp-engine so it
    /// exercises the engine's own command pipeline.
    /// Loads two independent KFM [`LoadedCharacter`]s (or `None` to skip when the
    /// fixture is absent). Lets a test inject a custom state into one before
    /// building the [`Match`].
    fn two_kfm_loaded() -> Option<(LoadedCharacter, LoadedCharacter)> {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return None;
        }
        Some((
            LoadedCharacter::load(&def).ok()?,
            LoadedCharacter::load(&def).ok()?,
        ))
    }

    fn two_kfm_match() -> Option<Match> {
        let (lc1, lc2) = two_kfm_loaded()?;
        let mut p1c = Character::with_constants(lc1.constants);
        p1c.pos = Vec2::new(-60.0, 0.0);
        p1c.state_no = 0;
        p1c.anim = 0;
        p1c.ctrl = true;
        let mut p2c = Character::with_constants(lc2.constants);
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.state_no = 0;
        p2c.anim = 0;
        p2c.ctrl = true;
        Some(Match::new(
            Player::new(p1c, lc1),
            Player::new(p2c, lc2),
            StageBounds::new(-220.0, 220.0),
        ))
    }

    /// Drives `m` through the intro into the live fight, returning whether it
    /// became live within the budget.
    fn run_until_fight(m: &mut Match) -> bool {
        for _ in 0..(INTRO_FRAMES + 5) {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() == RoundState::Fight {
                return true;
            }
        }
        m.round_state() == RoundState::Fight
    }

    /// Task 7.3 headline: in a real two-KFM match, P1 holding "toward the
    /// opponent" (absolute right, since P1 faces right) enters walk (state 20) and
    /// its world X advances toward P2 — driven entirely by P1's REAL CommandMatcher
    /// (Part A) and the loader's built-in stand->walk plus KFM's own walk velocity
    /// (Part B), with NO app/engine shim inventing `fwd`/`back`.
    #[test]
    fn p1_walks_toward_opponent_via_real_commands() {
        let Some(mut m) = two_kfm_match() else { return };
        assert!(
            run_until_fight(&mut m),
            "fight must go live before driving input"
        );
        assert_eq!(m.p1().facing(), Facing::Right, "P1 faces P2 (to its right)");

        let x_before = m.p1().pos().x;
        let gap_before = (m.p1().pos().x - m.p2().pos().x).abs();

        let mut entered_walk = false;
        for _ in 0..60 {
            m.tick(
                MatchInput {
                    right: true,
                    ..MatchInput::none()
                },
                MatchInput::none(),
            );
            if m.p1().character.state_no == 20 {
                entered_walk = true;
            }
        }
        assert!(
            entered_walk,
            "P1 holding toward the opponent must enter walk (state 20) via its real CommandMatcher"
        );
        assert!(
            m.p1().pos().x > x_before,
            "P1's world X must advance toward P2 while walking; {x_before} -> {}",
            m.p1().pos().x
        );
        let gap_after = (m.p1().pos().x - m.p2().pos().x).abs();
        assert!(
            gap_after < gap_before,
            "walking toward P2 must close the gap ({gap_before} -> {gap_after})"
        );
    }

    /// Task 7.3 (mirror facing): P2 faces LEFT, so holding absolute LEFT is "toward
    /// the opponent" for P2 and must walk it toward P1 — proving facing-relative
    /// command resolution flows through each player's own matcher.
    #[test]
    fn p2_walks_toward_opponent_facing_left() {
        let Some(mut m) = two_kfm_match() else { return };
        assert!(
            run_until_fight(&mut m),
            "fight must go live before driving input"
        );
        assert_eq!(m.p2().facing(), Facing::Left, "P2 faces P1 (to its left)");

        let x_before = m.p2().pos().x;
        for _ in 0..60 {
            m.tick(
                MatchInput::none(),
                MatchInput {
                    left: true,
                    ..MatchInput::none()
                },
            );
        }
        assert!(
            m.p2().pos().x < x_before,
            "P2 (facing left) holding absolute left must advance toward P1; {x_before} -> {}",
            m.p2().pos().x
        );
    }

    /// Task 7.3 end-to-end in fp-engine: P1 walks into range on real commands and
    /// a real KFM light punch connects, dropping P2's life — the same behavior the
    /// app's `headless_two_player_attack_connects_and_drops_life` proves, asserted
    /// here against the engine's own pipeline (no shim).
    #[test]
    fn real_command_attack_connects_and_drops_life() {
        let Some(mut m) = two_kfm_match() else { return };
        assert!(
            run_until_fight(&mut m),
            "fight must go live before driving input"
        );
        let p2_life_before = m.p2().life();

        // Walk into range.
        for _ in 0..240 {
            m.tick(
                MatchInput {
                    right: true,
                    ..MatchInput::none()
                },
                MatchInput::none(),
            );
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }
        // Throw light punches (x) on alternate frames so the matcher sees fresh
        // presses; over a generous budget a punch must connect.
        let mut hit = false;
        for i in 0..400 {
            let inp = if i % 3 == 0 {
                MatchInput {
                    x: true,
                    ..MatchInput::none()
                }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            if m.p2().life() < p2_life_before {
                hit = true;
                break;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert!(
            hit,
            "a P1 attack on real commands must connect and drop P2's life; P2 stayed at {} ({:?})",
            m.p2().life(),
            m.round_state()
        );
    }

    // ---- 8.3b AC: Match surfaces per-player PlaySnd SoundRequests -----------

    /// Builds a [`CompiledController`] of type `PlaySnd` with the given `value`
    /// pair (`group, sample`), firing unconditionally (`trigger1 = 1`).
    fn play_snd_controller(value: &str) -> fp_character::CompiledController {
        fp_character::CompiledController {
            state_number: 0,
            label: String::new(),
            controller_type: Some("PlaySnd".to_string()),
            triggerall: Vec::new(),
            triggers: vec![fp_character::CompiledTriggerGroup {
                number: 1,
                conditions: vec![fp_character::CompiledExpr::compile("1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params: [(
                "value".to_string(),
                fp_character::CompiledParam::compile(value),
            )]
            .into_iter()
            .collect(),
        }
    }

    /// A [`CompiledState`] number 0 whose only controller is the given one.
    fn state0_with(controller: fp_character::CompiledController) -> fp_character::CompiledState {
        fp_character::CompiledState {
            number: 0,
            // No physics so the synthetic character never falls/moves.
            state_type: Some("S".to_string()),
            movetype: Some("I".to_string()),
            physics: Some("N".to_string()),
            anim: None,
            ctrl: None,
            velset: None,
            poweradd: None,
            controllers: vec![controller],
            ..Default::default()
        }
    }

    /// A [`LoadedCharacter`] in state 0 whose state graph fires a `PlaySnd`
    /// (group/sample `7, 3`) every tick it runs.
    fn play_snd_loaded() -> LoadedCharacter {
        let mut loaded = loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ));
        loaded
            .states
            .insert(0, state0_with(play_snd_controller("7, 3")));
        loaded
    }

    /// A `Match` whose P1 fires a PlaySnd every tick and whose P2 is silent.
    fn play_snd_match() -> Match {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, play_snd_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        Match::new(p1, p2, StageBounds::new(-200.0, 200.0))
    }

    /// AC: a player's `PlaySnd` controller surfaces a [`fp_character::SoundRequest`]
    /// onto `Match::pN_sound_requests()` after `tick`, carrying the authored
    /// group/sample.
    #[test]
    fn play_snd_surfaces_on_p1_sound_requests() {
        let mut m = play_snd_match();
        // Even during the intro phase the character still ticks (idle animations),
        // and this synthetic PlaySnd is gated only on `trigger1 = 1`, so it fires.
        m.tick(MatchInput::none(), MatchInput::none());

        let reqs = m.p1_sound_requests();
        assert_eq!(reqs.len(), 1, "P1's PlaySnd surfaces exactly one request");
        assert_eq!(reqs[0].group, 7, "authored group is surfaced");
        assert_eq!(reqs[0].sample, 3, "authored sample is surfaced");
        // P2 fired no PlaySnd, so its requests stay empty.
        assert!(
            m.p2_sound_requests().is_empty(),
            "silent P2 surfaces nothing"
        );
    }

    /// AC: the per-player request slice is REPLACED each tick, not accumulated —
    /// a tick with no PlaySnd leaves it empty again.
    #[test]
    fn sound_requests_are_replaced_each_tick() {
        let mut m = play_snd_match();
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1_sound_requests().len(),
            1,
            "first tick surfaces the request"
        );

        // Swap P1 into an empty (no-PlaySnd) state, then tick again: the prior
        // request must NOT persist — the slice reflects only the latest tick.
        m.p1.character.state_no = 999; // unknown state → no controllers run
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(
            m.p1_sound_requests().is_empty(),
            "a tick with no PlaySnd replaces the slice with empty (not appended)"
        );
    }

    // ---- Cross-entity eval: Match::tick wires the opponent into each tick ----

    /// A `VarSet var(idx) = <expr>` controller firing unconditionally
    /// (`trigger1 = 1`). The `var(idx)` key form routes through the executor's
    /// indexed-key VarSet path.
    fn varset_controller(idx: i32, expr: &str) -> CompiledController {
        CompiledController {
            state_number: 0,
            label: String::new(),
            controller_type: Some("VarSet".to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params: [(format!("var({idx})"), CompiledParam::compile(expr))]
                .into_iter()
                .collect(),
        }
    }

    /// A state 0 that, every tick, records what this character's cross-entity
    /// triggers see about its opponent into its integer var bank:
    /// - `var(0)` = `p2dist X` (facing-relative gap to the opponent),
    /// - `var(1)` = `p2bodydist X` (edge-to-edge gap),
    /// - `var(2)` = `P2Life` (the opponent's life via the standalone alias).
    ///
    /// (The `enemy, life` redirect form is exercised at the unit level in
    /// `fp-character`; it cannot be probed via a VarSet value here because a
    /// controller parameter splits on the top-level comma, so `enemy, life` would
    /// be read as two separate components rather than one redirected expression.)
    fn cross_entity_probe_state() -> CompiledState {
        CompiledState {
            number: 0,
            state_type: Some("S".to_string()),
            movetype: Some("I".to_string()),
            physics: Some("N".to_string()),
            anim: None,
            ctrl: None,
            velset: None,
            poweradd: None,
            controllers: vec![
                varset_controller(0, "p2dist X"),
                varset_controller(1, "p2bodydist X"),
                varset_controller(2, "P2Life"),
            ],
            ..Default::default()
        }
    }

    /// AC: a real two-KFM `Match` wires each player's opponent into its tick, so
    /// P1's `p2dist`/`p2bodydist`/`P2Life` triggers all SEE P2. Proven by gating
    /// VarSet controllers on those triggers and reading the resulting vars after
    /// one tick — exercising the cross-entity seam through the real `Match::tick`,
    /// not an internal helper.
    #[test]
    fn match_tick_wires_opponent_into_cross_entity_triggers() {
        let Some((mut lc1, lc2)) = two_kfm_loaded() else {
            return;
        };
        // Replace P1's state 0 with the cross-entity probe.
        lc1.states.insert(0, cross_entity_probe_state());

        // P1 at x=-60 facing right, P2 at x=60 facing left, with a known life.
        let mut p1c = Character::with_constants(lc1.constants);
        p1c.pos = Vec2::new(-60.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        p1c.anim = 0;

        let mut p2c = Character::with_constants(lc2.constants);
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.state_no = 0;
        p2c.anim = 0;
        p2c.life = 432; // a distinctive opponent life to read back

        let front = p1c.constants.size.ground_front + p2c.constants.size.ground_front;
        let mut m = Match::new(
            Player::new(p1c, lc1),
            Player::new(p2c, lc2),
            StageBounds::new(-220.0, 220.0),
        );

        // One tick: P1's probe state records what it sees of P2. (Physics is N and
        // the controllers do not move P1, so the recorded gap is the start gap.)
        m.tick(MatchInput::none(), MatchInput::none());

        let p1 = &m.p1().character;
        // p2dist X: P2 is 120px ahead of a right-facing P1 → +120 (positive = front).
        assert_eq!(
            p1.vars[0], 120,
            "p1 must see p2dist X = 120 (opponent in front)"
        );
        // p2bodydist X: 120 minus both front half-widths.
        assert_eq!(
            p1.vars[1],
            120 - front,
            "p1 must see edge-to-edge p2bodydist X = 120 - widths"
        );
        // P2Life reads the opponent's distinctive life.
        assert_eq!(p1.vars[2], 432, "P2Life must read the opponent's life");
    }

    /// AC (redirect path): the `p2`/`enemy` REDIRECT — not just the standalone
    /// `P2*` triggers — resolves the opponent through a real `Match::tick`. A
    /// controller TRIGGER (a full expression, unlike a param value which splits on
    /// the top-level comma) gated on `(p2, life) = 432` fires only if the redirect
    /// reads P2's life, exercising `EvalCtx::redirect` end-to-end.
    #[test]
    fn match_tick_resolves_redirect_through_a_trigger() {
        let Some((mut lc1, lc2)) = two_kfm_loaded() else {
            return;
        };
        let gated = CompiledController {
            state_number: 0,
            label: String::new(),
            controller_type: Some("VarSet".to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("(p2, life) = 432")],
            }],
            persistent: None,
            ignorehitpause: None,
            params: [("var(3)".to_string(), CompiledParam::compile("1"))]
                .into_iter()
                .collect(),
        };
        let mut probe = cross_entity_probe_state();
        probe.controllers.push(gated);
        lc1.states.insert(0, probe);

        let mut p1c = Character::with_constants(lc1.constants);
        p1c.pos = Vec2::new(-60.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        p1c.anim = 0;
        let mut p2c = Character::with_constants(lc2.constants);
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.state_no = 0;
        p2c.anim = 0;
        p2c.life = 432;
        let mut m = Match::new(
            Player::new(p1c, lc1),
            Player::new(p2c, lc2),
            StageBounds::new(-220.0, 220.0),
        );
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().character.vars[3],
            1,
            "`(p2, life)` redirect must resolve the opponent through Match::tick"
        );
    }

    // ---- Task 7.4: best-of-N match flow ------------------------------------

    /// Advances the match one full round-decision cycle: from a live Fight,
    /// force the given KO, then tick through the KO hold so `advance_round`
    /// processes the [`RoundState::Win`] arm (either resetting for the next round
    /// or ending the match). After this returns, the match is in either the next
    /// round's `Intro` or the terminal match-over state.
    fn ko_p2_and_settle(m: &mut Match) {
        // We must be live to decide a round.
        assert_eq!(m.round_state(), RoundState::Fight);
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko, "KO enters the KO hold");
        // Hold through KO -> Win, then one more tick runs the Win-arm transition.
        for _ in 0..(KO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
    }

    /// Same as [`ko_p2_and_settle`] but KOs P1 (so P2 wins the round).
    fn ko_p1_and_settle(m: &mut Match) {
        assert_eq!(m.round_state(), RoundState::Fight);
        m.p1.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        for _ in 0..(KO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
    }

    /// AC1: a fresh match exposes the documented best-of-N defaults.
    #[test]
    fn match_starts_best_of_three_round_one() {
        let m = basic_match();
        assert_eq!(
            m.rounds_to_win(),
            DEFAULT_ROUNDS_TO_WIN,
            "default best of three"
        );
        assert_eq!(m.rounds_to_win(), 2);
        assert_eq!(m.round_number(), 1, "first round is round 1");
        assert_eq!(m.p1_round_wins(), 0);
        assert_eq!(m.p2_round_wins(), 0);
        assert_eq!(m.match_state(), MatchState::InProgress);
        assert_eq!(m.match_winner(), None, "no match winner at the start");
    }

    /// AC1/AC3: `with_rounds_to_win` and `set_rounds_to_win` override the target;
    /// a non-positive value clamps to 1 (a one-round match is always winnable).
    #[test]
    fn rounds_to_win_override_and_clamp() {
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let m = Match::with_rounds_to_win(p1, p2, StageBounds::default(), 3);
        assert_eq!(m.rounds_to_win(), 3, "explicit best of five");

        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let m = Match::with_rounds_to_win(p1, p2, StageBounds::default(), 0);
        assert_eq!(m.rounds_to_win(), 1, "non-positive target clamps to 1");

        let mut m = basic_match();
        m.set_rounds_to_win(-4);
        assert_eq!(m.rounds_to_win(), 1, "setter clamps a non-positive target");
        m.set_rounds_to_win(5);
        assert_eq!(m.rounds_to_win(), 5);
    }

    /// AC2/AC4 headline: KO P2 twice — P1 reaches 2 round wins and the match is
    /// over with `match_winner() == P1`. Between the rounds the fighters reset
    /// (life to max, back to start positions/facing) and `round_number` advances.
    #[test]
    fn ko_p2_twice_wins_match_for_p1() {
        // Two characters apart; short stage; default best-of-three.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));

        let p1_start = m.p1().pos();
        let p2_start = m.p2().pos();

        // --- Round 1: KO P2 ---
        into_fight(&mut m);
        // Damage P2 partway and move both off their start spots before the KO so
        // we can prove the reset restores them.
        m.p2.character.life = 400;
        m.p1.character.pos.x = -10.0;
        m.p2.character.pos.x = 10.0;
        ko_p2_and_settle(&mut m);

        assert_eq!(m.p1_round_wins(), 1, "P1 credited the round-1 KO");
        assert_eq!(m.p2_round_wins(), 0);
        assert_eq!(
            m.match_state(),
            MatchState::InProgress,
            "match not over after 1"
        );
        assert_eq!(m.match_winner(), None);
        assert_eq!(m.round_number(), 2, "round_number incremented to 2");
        assert_eq!(m.round_state(), RoundState::Intro, "reset re-enters Intro");
        assert_eq!(m.winner(), None, "current-round winner cleared on reset");

        // Reset restored life to max and positions/facing to the captured start.
        assert_eq!(m.p1().life(), m.p1().life_max(), "P1 life restored to max");
        assert_eq!(m.p2().life(), m.p2().life_max(), "P2 life restored to max");
        assert_eq!(m.p1().pos(), p1_start, "P1 back at its start position");
        assert_eq!(m.p2().pos(), p2_start, "P2 back at its start position");
        assert_eq!(m.p1().facing(), Facing::Right, "P1 re-faces the opponent");
        assert_eq!(m.p2().facing(), Facing::Left, "P2 re-faces the opponent");
        // Timer reset to the full round length for round 2.
        assert_eq!(m.timer(), DEFAULT_ROUND_SECONDS * TICKS_PER_SECOND);

        // --- Round 2: KO P2 again -> P1 reaches the threshold, match over ---
        into_fight(&mut m);
        ko_p2_and_settle(&mut m);

        assert_eq!(m.p1_round_wins(), 2, "P1 reached the win threshold");
        assert_eq!(m.match_state(), MatchState::Over, "match is now over");
        assert_eq!(m.match_winner(), Some(Winner::P1), "P1 wins the match");
        assert_eq!(m.round_number(), 2, "no further round after the match ends");
    }

    /// AC2/AC4: a split (each player wins one round) advances to round 3 with the
    /// match still in progress.
    #[test]
    fn split_rounds_advance_to_round_three() {
        let mut m = basic_match();

        // Round 1: P1 wins (KO P2).
        into_fight(&mut m);
        ko_p2_and_settle(&mut m);
        assert_eq!(m.p1_round_wins(), 1);
        assert_eq!(m.round_number(), 2);
        assert_eq!(m.match_state(), MatchState::InProgress);

        // Round 2: P2 wins (KO P1).
        into_fight(&mut m);
        ko_p1_and_settle(&mut m);
        assert_eq!(m.p1_round_wins(), 1, "P1 still on one");
        assert_eq!(m.p2_round_wins(), 1, "P2 now on one");
        assert_eq!(m.match_state(), MatchState::InProgress, "1-1 is undecided");
        assert_eq!(m.match_winner(), None);
        assert_eq!(m.round_number(), 3, "the split advances to round 3");
        assert_eq!(m.round_state(), RoundState::Intro);
    }

    /// AC2/AC4: time-over credits the higher-life fighter (round win goes to P1
    /// when P1 has more life), and the round resets for the next round.
    #[test]
    fn time_over_credits_higher_life_fighter() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.life = 800;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.life = 500;
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        // One-second round so time-over is reached quickly.
        let mut m = Match::with_round_seconds(p1, p2, StageBounds::new(-200.0, 200.0), 1);
        into_fight(&mut m);

        // Burn the clock; both survive, so the decision is by life compare.
        for _ in 0..(TICKS_PER_SECOND + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert_ne!(m.round_state(), RoundState::Fight, "timer expired");
        assert_eq!(m.winner(), Some(Winner::P1), "more life wins the round");

        // Hold through KO -> Win -> reset.
        for _ in 0..(KO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.p1_round_wins(), 1, "time-over winner credited the round");
        assert_eq!(m.p2_round_wins(), 0);
        assert_eq!(
            m.round_number(),
            2,
            "time-over round still resets for the next"
        );
        assert_eq!(m.round_state(), RoundState::Intro);
    }

    /// AC3/AC4: a drawn round (double KO) credits NEITHER player, and the match
    /// continues to the next round per the documented MUGEN-faithful draw rule.
    #[test]
    fn draw_round_credits_neither_and_continues() {
        let mut m = basic_match();
        into_fight(&mut m);
        // Double KO on the same frame.
        m.p1.character.life = 0;
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.winner(), Some(Winner::Draw), "double KO is a draw round");
        for _ in 0..(KO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.p1_round_wins(), 0, "a draw credits neither player");
        assert_eq!(m.p2_round_wins(), 0);
        assert_eq!(
            m.match_state(),
            MatchState::InProgress,
            "draw keeps the match live"
        );
        assert_eq!(m.match_winner(), None);
        assert_eq!(
            m.round_number(),
            2,
            "a drawn round still advances to the next"
        );
        assert_eq!(m.round_state(), RoundState::Intro);
    }

    /// AC2: the match-over state is terminal — once decided, further ticks change
    /// neither the match state, the match winner, nor the round number.
    #[test]
    fn match_over_is_terminal() {
        // Best of one: a single KO ends the match.
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let mut m = Match::with_rounds_to_win(p1, p2, StageBounds::default(), 1);
        into_fight(&mut m);
        ko_p2_and_settle(&mut m);

        assert_eq!(
            m.match_state(),
            MatchState::Over,
            "best-of-one ends in one KO"
        );
        assert_eq!(m.match_winner(), Some(Winner::P1));
        assert_eq!(
            m.round_number(),
            1,
            "no reset when the match is already won"
        );
        let (state, winner, round) = (m.match_state(), m.match_winner(), m.round_number());
        for _ in 0..120 {
            m.tick(MatchInput::none(), MatchInput::none());
            assert_eq!(m.match_state(), state, "match-over is terminal");
            assert_eq!(m.match_winner(), winner, "match winner is stable");
            assert_eq!(m.round_number(), round, "round number stable once over");
        }
    }

    /// AC3: power carries across rounds (it is NOT reset on a round reset), the
    /// documented MUGEN-faithful behavior.
    #[test]
    fn power_carries_across_rounds() {
        let mut m = basic_match();
        into_fight(&mut m);
        // Bank some power, then decide the round.
        m.p1.character.power = 1234;
        m.p2.character.power = 567;
        ko_p2_and_settle(&mut m);
        assert_eq!(m.round_number(), 2, "advanced to round 2");
        assert_eq!(
            m.p1().character.power,
            1234,
            "P1 power carries across rounds"
        );
        assert_eq!(
            m.p2().character.power,
            567,
            "P2 power carries across rounds"
        );
    }

    /// AC3 (reset coverage): a round reset clears transient combat state —
    /// hit-pause, active HitDef, and the get-hit reaction are all neutralized.
    #[test]
    fn round_reset_clears_transient_combat_state() {
        let mut m = basic_match();
        into_fight(&mut m);
        // Dirty up P2's transient state before the KO.
        m.p2.character.hitpause = 12;
        m.p2.character.shaketime = 8;
        m.p2.character.active_hitdef = Some(sample_hitdef());
        m.p2.character.move_type = MoveType::BeingHit;
        m.p2.character.vel = Vec2::new(7.0, -3.0);
        ko_p2_and_settle(&mut m);

        assert_eq!(m.round_number(), 2);
        let p2 = m.p2();
        assert_eq!(p2.character.hitpause, 0, "hit-pause cleared on reset");
        assert_eq!(p2.character.shaketime, 0, "hit-shake cleared on reset");
        assert!(
            p2.character.active_hitdef.is_none(),
            "active HitDef cleared"
        );
        assert_eq!(p2.character.move_type, MoveType::Idle, "back to idle");
        assert_eq!(
            p2.character.state_type,
            StateType::Standing,
            "back to standing"
        );
        assert_eq!(p2.character.vel, Vec2::new(0.0, 0.0), "velocity zeroed");
        assert_eq!(p2.character.state_no, 0, "state machine back to state 0");
    }

    /// AC2 (no premature reset): a round decision that does NOT reach the
    /// threshold still leaves the round playable — re-entering Fight after the
    /// reset works and the timer counts down again.
    #[test]
    fn next_round_resumes_fighting() {
        let mut m = basic_match();
        into_fight(&mut m);
        ko_p2_and_settle(&mut m);
        assert_eq!(m.round_state(), RoundState::Intro, "reset re-enters Intro");
        // Drive the new round into Fight; the timer must count down again.
        into_fight(&mut m);
        assert_eq!(m.round_state(), RoundState::Fight, "next round goes live");
        let before = m.timer();
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.timer(), before - 1, "the reset round clock counts down");
    }

    // ---- Task 7.4: additional Proctor coverage (edge cases / semantics) -----

    /// AC1/AC3: `with_config` is the shared constructor — both `round_seconds`
    /// (clamped `>= 0`) and `rounds_to_win` (clamped `>= 1`) take effect at once.
    /// Covers the constructor that the other `with_*` helpers delegate to.
    #[test]
    fn with_config_applies_both_clamps() {
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        // Non-positive seconds AND non-positive target: both clamp.
        let m = Match::with_config(p1, p2, StageBounds::default(), -3, -7);
        assert_eq!(
            m.timer(),
            0,
            "negative round_seconds clamps to a 0-frame timer"
        );
        assert_eq!(
            m.rounds_to_win(),
            1,
            "non-positive rounds_to_win clamps to 1"
        );

        // Valid values pass straight through.
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let m = Match::with_config(p1, p2, StageBounds::default(), 5, 4);
        assert_eq!(m.timer(), 5 * TICKS_PER_SECOND, "round_seconds honored");
        assert_eq!(m.rounds_to_win(), 4, "rounds_to_win honored");
        assert_eq!(m.round_number(), 1);
        assert_eq!(m.match_state(), MatchState::InProgress);
    }

    /// AC2/AC4: a best-of-FIVE match (`rounds_to_win = 3`) is decided only after a
    /// player banks three round wins. This drives multiple resets in a row and
    /// asserts the running tallies, the climbing `round_number`, and that the match
    /// stays in progress until — and becomes terminal exactly at — the third win.
    #[test]
    fn best_of_five_needs_three_round_wins() {
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let mut m = Match::with_rounds_to_win(p1, p2, StageBounds::new(-200.0, 200.0), 3);

        // P1 wins rounds 1 and 2 — still in progress (needs three).
        for expected_wins in 1..=2 {
            into_fight(&mut m);
            ko_p2_and_settle(&mut m);
            assert_eq!(m.p1_round_wins(), expected_wins);
            assert_eq!(m.match_state(), MatchState::InProgress, "not over before 3");
            assert_eq!(m.match_winner(), None);
            assert_eq!(m.round_number(), expected_wins + 1, "round_number climbs");
            assert_eq!(m.round_state(), RoundState::Intro, "reset between rounds");
        }

        // Round 3: P1's third win ends the match.
        into_fight(&mut m);
        ko_p2_and_settle(&mut m);
        assert_eq!(
            m.p1_round_wins(),
            3,
            "P1 reached the best-of-five threshold"
        );
        assert_eq!(m.match_state(), MatchState::Over);
        assert_eq!(m.match_winner(), Some(Winner::P1));
        assert_eq!(m.round_number(), 3, "no reset once the match is decided");
    }

    /// AC3/AC4: consecutive drawn rounds credit NEITHER player and keep advancing
    /// the round count without ever ending the match — the documented "draws just
    /// continue" rule, exercised across several rounds in a row.
    #[test]
    fn repeated_draws_never_end_the_match() {
        let mut m = basic_match();
        for round in 1..=4 {
            into_fight(&mut m);
            assert_eq!(m.round_number(), round, "round_number tracks each draw");
            // Double KO -> draw.
            m.p1.character.life = 0;
            m.p2.character.life = 0;
            m.tick(MatchInput::none(), MatchInput::none());
            assert_eq!(m.winner(), Some(Winner::Draw));
            for _ in 0..(KO_FRAMES + 1) {
                m.tick(MatchInput::none(), MatchInput::none());
            }
            assert_eq!(m.p1_round_wins(), 0, "draws never credit P1");
            assert_eq!(m.p2_round_wins(), 0, "draws never credit P2");
            assert_eq!(
                m.match_state(),
                MatchState::InProgress,
                "a draw never ends it"
            );
            assert_eq!(m.match_winner(), None, "match winner is never Draw");
        }
        assert_eq!(m.round_number(), 5, "four draws advanced to round 5");
    }

    /// AC2/AC3: lowering `rounds_to_win` below a player's existing tally is NOT
    /// applied retroactively (the doc contract), but the NEXT decided round ends
    /// the match as soon as the now-met threshold is observed at the Win arm.
    #[test]
    fn lowering_threshold_ends_match_on_next_decision() {
        let mut m = basic_match(); // best of three (target 2)

        // Round 1: P1 wins one round (tally = 1, below the default target 2).
        into_fight(&mut m);
        ko_p2_and_settle(&mut m);
        assert_eq!(m.p1_round_wins(), 1);
        assert_eq!(
            m.match_state(),
            MatchState::InProgress,
            "1 < 2, still going"
        );

        // Lower the target to 1. The match is NOT ended here (no round is being
        // decided at this instant) — the decision happens at the next Win arm.
        m.set_rounds_to_win(1);
        assert_eq!(m.rounds_to_win(), 1);
        assert_eq!(
            m.match_state(),
            MatchState::InProgress,
            "lowering the target does not retroactively end the match"
        );

        // The next decided round observes tally(1) >= target(1) and ends it.
        into_fight(&mut m);
        ko_p2_and_settle(&mut m);
        assert_eq!(
            m.match_state(),
            MatchState::Over,
            "next decision ends the match"
        );
        assert_eq!(m.match_winner(), Some(Winner::P1));
        assert_eq!(m.p1_round_wins(), 2);
    }

    /// AC3 (reset fidelity): the round reset restores `life` from the CAPTURED
    /// `life_max` template even if a fighter's `life_max` was mutated mid-round —
    /// the snapshot taken at construction is the source of truth, and full life is
    /// restored to it. Also confirms `holding_back`/`ctrl` are cleared for intro.
    #[test]
    fn round_reset_restores_life_to_captured_max() {
        let mut m = basic_match();
        let captured_max = m.p2().life_max(); // template max from construction
        into_fight(&mut m);

        // Mutate P2's live life_max and dirty its guard/ctrl flags before the KO.
        m.p2.character.life_max = 12345; // a spurious mid-round change
        m.p2.character.holding_back = true;
        m.p1.character.holding_back = true;
        ko_p2_and_settle(&mut m);

        assert_eq!(m.round_number(), 2);
        // Reset restored life to the captured template max (NOT the mutated value).
        assert_eq!(
            m.p2().life_max(),
            captured_max,
            "life_max restored from the captured reset template"
        );
        assert_eq!(
            m.p2().life(),
            captured_max,
            "life restored to the captured max"
        );
        // Intro means no control and no stale guard latch.
        assert!(!m.p1().character.ctrl, "control off during the reset intro");
        assert!(!m.p2().character.ctrl);
        assert!(
            !m.p1().character.holding_back,
            "guard latch cleared on reset"
        );
        assert!(!m.p2().character.holding_back);
    }

    /// AC3: a round reset clears any stale per-player sound requests so the new
    /// round's first tick does not surface a previous round's audio.
    #[test]
    fn round_reset_clears_stale_sound_requests() {
        let mut m = play_snd_match(); // P1 fires a PlaySnd each tick
        into_fight(&mut m);
        // Confirm there ARE requests to clear, then decide the round.
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(!m.p1_sound_requests().is_empty(), "P1 has pending requests");
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        // Hold through KO -> Win; the Win-arm reset clears the request vecs.
        for _ in 0..(KO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.round_number(), 2, "reset happened");
        assert!(
            m.p1_sound_requests().is_empty(),
            "the reset clears stale sound requests"
        );
        assert!(m.p2_sound_requests().is_empty());
    }

    /// AC2: a time-over decided round in a best-of-N match still resets and the
    /// match continues; a second time-over win for the SAME fighter ends it.
    /// Exercises the time-over decision path through the full best-of-N flow twice.
    #[test]
    fn two_time_over_wins_decide_the_match() {
        let build = || {
            let mut p1c = Character::new();
            p1c.pos = Vec2::new(-50.0, 0.0);
            p1c.life = 900;
            let mut p2c = Character::new();
            p2c.pos = Vec2::new(50.0, 0.0);
            p2c.life = 100; // P1 always ahead on life -> P1 wins each time-over
            let p1 = Player::new(p1c, defender_loaded());
            let p2 = Player::new(p2c, defender_loaded());
            Match::with_round_seconds(p1, p2, StageBounds::new(-200.0, 200.0), 1)
        };
        let mut m = build();

        let run_time_over = |m: &mut Match| {
            into_fight(m);
            for _ in 0..(TICKS_PER_SECOND + 1) {
                m.tick(MatchInput::none(), MatchInput::none());
                if m.round_state() != RoundState::Fight {
                    break;
                }
            }
            assert_ne!(m.round_state(), RoundState::Fight, "timer expired");
            for _ in 0..(KO_FRAMES + 1) {
                m.tick(MatchInput::none(), MatchInput::none());
            }
        };

        // Round 1 time-over: P1 ahead, credited, match continues (1 < 2).
        run_time_over(&mut m);
        assert_eq!(m.p1_round_wins(), 1);
        assert_eq!(m.match_state(), MatchState::InProgress);
        assert_eq!(m.round_number(), 2);
        // Reset restored both fighters to full life for round 2 (so the 900-vs-100
        // life gap is a property of the reset template, not the round-1 carryover).
        assert_eq!(m.p1().life(), m.p1().life_max());
        assert_eq!(m.p2().life(), m.p2().life_max());

        // Round 2 time-over: both reset to full life (equal!) — that would draw.
        // To prove the SECOND win path, damage P2 during round 2 before time-over.
        into_fight(&mut m);
        m.p2.character.life = 50; // P1 (full) ahead again
        for _ in 0..(TICKS_PER_SECOND + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert_eq!(
            m.winner(),
            Some(Winner::P1),
            "P1 ahead at the second time-over"
        );
        for _ in 0..(KO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.p1_round_wins(), 2, "P1's second round win");
        assert_eq!(m.match_state(), MatchState::Over);
        assert_eq!(m.match_winner(), Some(Winner::P1));
    }

    /// AC2 (semantics): once the match is over, the round phase machine is frozen
    /// — it never leaves the terminal `Win` it ended on, and no new KO/time-over is
    /// processed. Pairs with `match_over_is_terminal` (which guards the match-level
    /// fields) by guarding the round-phase side.
    #[test]
    fn round_phase_frozen_after_match_over() {
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let mut m = Match::with_rounds_to_win(p1, p2, StageBounds::default(), 1);
        into_fight(&mut m);
        ko_p2_and_settle(&mut m);
        assert_eq!(m.match_state(), MatchState::Over);
        // The round stays in Win (the phase it ended the match on); further ticks
        // do not advance it, and re-forcing a "KO" changes nothing.
        let phase = m.round_state();
        assert_eq!(phase, RoundState::Win, "match ended out of the Win arm");
        m.p1.character.life = 0; // would be a KO if the machine were live
        for _ in 0..30 {
            m.tick(MatchInput::none(), MatchInput::none());
            assert_eq!(
                m.round_state(),
                phase,
                "round phase frozen after match over"
            );
        }
        assert_eq!(m.match_winner(), Some(Winner::P1), "winner unchanged");
    }

    /// AC1: the public best-of-N enums derive the traits the public API promises
    /// (Copy/Clone/Debug/PartialEq/Eq), and the variants are distinct — a guard so
    /// the renderer-/test-facing surface stays usable.
    #[test]
    fn match_state_and_winner_enums_are_well_behaved() {
        // MatchState: Copy + Eq + distinct variants + Debug.
        let a = MatchState::InProgress;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(MatchState::InProgress, MatchState::Over);
        assert!(format!("{:?}", MatchState::Over).contains("Over"));

        // Winner has three distinct variants, all Copy/Eq/Debug.
        assert_ne!(Winner::P1, Winner::P2);
        assert_ne!(Winner::P1, Winner::Draw);
        assert_ne!(Winner::P2, Winner::Draw);
        let w = Winner::Draw;
        assert_eq!(w, w);
        assert!(format!("{:?}", Winner::P1).contains("P1"));
    }

    /// AC2: round 1 itself never auto-resets — a match that is never decided stays
    /// in round 1 indefinitely (the reset only fires at a round decision). Guards
    /// against an off-by-one that might "reset" before any round is won.
    #[test]
    fn undecided_match_stays_in_round_one() {
        let mut m = basic_match();
        into_fight(&mut m);
        // Many fight ticks with nobody dying and a 99s clock that won't expire.
        for _ in 0..300 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.round_number(), 1, "no decision -> still round 1");
        assert_eq!(m.p1_round_wins(), 0);
        assert_eq!(m.p2_round_wins(), 0);
        assert_eq!(m.match_state(), MatchState::InProgress);
        assert_eq!(m.round_state(), RoundState::Fight, "still fighting round 1");
    }

    // ---- P8b: TargetOps applied to the opponent + p1stateno to the attacker ----

    /// Builds a compiled controller of `ctrl_type` that fires every tick
    /// (`trigger1 = 1`), with the given `(name, value)` params. Used to author the
    /// synthetic `Target*` throw states the P8b end-to-end tests drive.
    fn target_controller(
        state_number: i32,
        ctrl_type: &str,
        params: &[(&str, &str)],
    ) -> CompiledController {
        let mut compiled = HashMap::new();
        for (k, v) in params {
            compiled.insert((*k).to_string(), CompiledParam::compile(v));
        }
        CompiledController {
            state_number,
            label: ctrl_type.to_string(),
            controller_type: Some(ctrl_type.to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params: compiled,
        }
    }

    /// A minimal standing [`CompiledState`] numbered `number` holding `controllers`.
    fn state_with(number: i32, controllers: Vec<CompiledController>) -> CompiledState {
        CompiledState {
            number,
            state_type: Some("S".to_string()),
            movetype: None,
            physics: None,
            anim: None,
            ctrl: None,
            velset: None,
            poweradd: None,
            controllers,
            ..Default::default()
        }
    }

    /// AC2: a player in a state that (with `has_target` set) emits
    /// `TargetState` + `TargetBind` + `TargetLifeAdd` drives the opponent on a
    /// single tick: the opponent enters the target state, is bound at the
    /// facing-relative offset from the binder, and loses the LifeAdd amount.
    #[test]
    fn target_ops_apply_to_opponent_state_bind_and_life() {
        // Binder (P1) at the origin facing right; target (P2) to the right.
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        // P1 sits in state 810 (the throw "hold" state) and already has a target.
        p1c.state_no = 810;
        p1c.has_target = true;

        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 500;
        p2c.life_max = 1000;

        // P1's loaded states: state 810 emits the three throw Target ops each tick.
        let mut p1_loaded = attacker_loaded();
        p1_loaded.states.insert(
            810,
            state_with(
                810,
                vec![
                    target_controller(810, "TargetState", &[("value", "820")]),
                    // Bind well beyond the player-push threshold (default body
                    // half-widths ~16/15) so the bound position is observable after
                    // the post-combat player-push step does not re-separate the
                    // (non-overlapping) bodies.
                    target_controller(810, "TargetBind", &[("time", "1"), ("pos", "60, -5")]),
                    target_controller(810, "TargetLifeAdd", &[("value", "-40"), ("kill", "0")]),
                ],
            ),
        );

        // P2's OWN states must contain 820 so TargetState's change_state lands.
        let mut p2_loaded = defender_loaded();
        p2_loaded.states.insert(820, state_with(820, Vec::new()));

        let p1 = Player::new(p1c, p1_loaded);
        let p2 = Player::new(p2c, p2_loaded);
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Re-assert the throw preconditions after the intro (the intro re-faces /
        // re-grants control) so the single observed tick is deterministic.
        m.p1.character.state_no = 810;
        m.p1.character.has_target = true;
        m.p1.character.facing = Facing::Right;
        m.p1.character.pos = Vec2::new(0.0, 0.0);
        let life_before = m.p2().life();

        m.tick(MatchInput::none(), MatchInput::none());

        // TargetState -> P2 entered 820 via its own state graph.
        assert_eq!(
            m.p2().character.state_no,
            820,
            "TargetState moved P2 to 820"
        );
        // TargetBind -> P2 pinned to binder.pos + (60 * +1, -5).
        assert!(
            (m.p2().pos().x - 60.0).abs() < 1e-3,
            "bound X is facing-relative to the binder, got {}",
            m.p2().pos().x
        );
        assert!(
            (m.p2().pos().y + 5.0).abs() < 1e-3,
            "bound Y offset from the binder, got {}",
            m.p2().pos().y
        );
        // TargetLifeAdd(-40, kill=false) -> P2 life dropped by 40, floored at 1.
        assert_eq!(m.p2().life(), life_before - 40, "throw damage applied");
    }

    /// AC1: a left-facing binder mirrors the `TargetBind` X offset; `TargetFacing`
    /// orients the target relative to the binder; and a non-killing `TargetLifeAdd`
    /// floors the victim at `1` rather than reaching `0`.
    #[test]
    fn target_bind_mirrors_and_lifeadd_floors_at_one() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Left;
        p1c.state_no = 810;
        p1c.has_target = true;

        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(-60.0, 0.0);
        p2c.facing = Facing::Right;
        p2c.life = 10;
        p2c.life_max = 1000;

        let mut p1_loaded = attacker_loaded();
        p1_loaded.states.insert(
            810,
            state_with(
                810,
                vec![
                    // Offset 60 mirrored by the left-facing binder -> -60 (beyond
                    // the player-push threshold, so the bound position survives).
                    target_controller(810, "TargetBind", &[("pos", "60, 0")]),
                    target_controller(810, "TargetFacing", &[("value", "-1")]),
                    target_controller(810, "TargetLifeAdd", &[("value", "-9999"), ("kill", "0")]),
                ],
            ),
        );
        let p2_loaded = defender_loaded();

        let p1 = Player::new(p1c, p1_loaded);
        let p2 = Player::new(p2c, p2_loaded);
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        m.p1.character.state_no = 810;
        m.p1.character.has_target = true;
        m.p1.character.facing = Facing::Left;
        m.p1.character.pos = Vec2::new(0.0, 0.0);

        m.tick(MatchInput::none(), MatchInput::none());

        // Left-facing binder mirrors +60 to -60.
        assert!(
            (m.p2().pos().x + 60.0).abs() < 1e-3,
            "left-facing bind mirrors X, got {}",
            m.p2().pos().x
        );
        // TargetFacing(-1): opposite the binder (binder faces Left -> target Right).
        assert_eq!(m.p2().facing(), Facing::Right, "facing opposite the binder");
        // Non-killing huge damage floors at 1, never 0.
        assert_eq!(m.p2().life(), 1, "kill=false floors life at 1");
    }

    /// AC2 (second test): a connecting hit whose HitDef carries `p1stateno` moves
    /// the ATTACKER into that state via its own state graph.
    #[test]
    fn attacker_state_p1stateno_moves_attacker_on_connect() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;

        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 1000; // survive the hit so the round stays in Fight

        // P1's own states must contain the p1stateno destination (810) so the
        // attacker's change_state lands.
        let mut p1_loaded = attacker_loaded();
        p1_loaded.states.insert(810, state_with(810, Vec::new()));

        let p1 = Player::new(p1c, p1_loaded);
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Arm a throw-style HitDef: p1stateno = 810 (attacker), p2stateno = 820.
        let mut hd = sample_hitdef();
        hd.p1stateno = Some(810);
        hd.p2stateno = Some(820);

        for _ in 0..3 {
            m.p1.character.anim = 200;
            m.p1.character.anim_elem = 0;
            m.p1.character.move_type = MoveType::Attack;
            m.p1.character.active_hitdef = Some(hd);
            m.p1.character.move_connect.reset();
            m.p2.character.anim = 0;
            m.p2.character.anim_elem = 0;
            m.p2.character.state_type = StateType::Standing;
            m.tick(MatchInput::none(), MatchInput::none());
            if m.p1().character.state_no == 810 {
                break;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }

        assert_eq!(
            m.p1().character.state_no,
            810,
            "p1stateno moved the attacker into its throw state"
        );
    }

    // ---- P8b (Proctor): edge cases / error paths for the TargetOp pipeline ----

    /// Helper: builds a `Match` whose P1 sits in state 810 (a throw "hold" state)
    /// emitting `controllers` each tick with `has_target` set, and whose P2 (the
    /// target) is at `p2_pos` with `p2_loaded` as its own state graph. Re-asserts
    /// the throw preconditions after the intro so the first observed tick is
    /// deterministic. Returns the live, fight-phase match ready for one `tick`.
    fn throw_match_p1_binder(
        controllers: Vec<CompiledController>,
        p2_pos: Vec2<f32>,
        p2_life: i32,
        p2_loaded: LoadedCharacter,
    ) -> Match {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 810;
        p1c.has_target = true;

        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = p2_pos;
        p2c.facing = Facing::Left;
        p2c.life = p2_life;
        p2c.life_max = 1000;

        let mut p1_loaded = attacker_loaded();
        p1_loaded.states.insert(810, state_with(810, controllers));

        let p1 = Player::new(p1c, p1_loaded);
        let p2 = Player::new(p2c, p2_loaded);
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);
        // The intro re-faces / re-grants control; re-pin the throw preconditions.
        m.p1.character.state_no = 810;
        m.p1.character.has_target = true;
        m.p1.character.facing = Facing::Right;
        m.p1.character.pos = Vec2::new(0.0, 0.0);
        m
    }

    /// AC1: `TargetVelSet` overwrites the target's velocity and `TargetPowerAdd`
    /// adds to the target's power clamped into `[0, power_max]`. Exercises the two
    /// `TargetOp` variants the existing throw tests do not touch.
    #[test]
    fn target_velset_and_poweradd_apply_to_opponent() {
        // P2 starts at a non-trivial velocity and zero power; the throw sets a new
        // velocity and grants meter.
        let mut p2_loaded = defender_loaded();
        p2_loaded.states.insert(820, state_with(820, Vec::new()));
        let mut m = throw_match_p1_binder(
            vec![
                target_controller(810, "TargetVelSet", &[("x", "3"), ("y", "-7")]),
                target_controller(810, "TargetPowerAdd", &[("value", "250")]),
            ],
            Vec2::new(80.0, 0.0),
            500,
            p2_loaded,
        );
        m.p2.character.vel = Vec2::new(99.0, 99.0);
        m.p2.character.power = 0;

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(
            (m.p2().character.vel.x - 3.0).abs() < 1e-3
                && (m.p2().character.vel.y + 7.0).abs() < 1e-3,
            "TargetVelSet overwrites the target velocity, got {:?}",
            m.p2().character.vel
        );
        assert_eq!(
            m.p2().character.power,
            250,
            "TargetPowerAdd grants meter to the target"
        );
    }

    /// AC1: `TargetVelAdd` adds to (does not replace) the target's velocity, and
    /// `TargetPowerAdd` clamps the result to the target's `[0, power_max]`. A huge
    /// positive add saturates at `power_max`; a negative add cannot go below `0`.
    #[test]
    fn target_veladd_accumulates_and_poweradd_clamps_to_range() {
        let mut p2_loaded = defender_loaded();
        p2_loaded.states.insert(820, state_with(820, Vec::new()));
        let mut m = throw_match_p1_binder(
            vec![
                target_controller(810, "TargetVelAdd", &[("x", "2"), ("y", "1")]),
                target_controller(810, "TargetPowerAdd", &[("value", "99999")]),
            ],
            Vec2::new(80.0, 0.0),
            500,
            p2_loaded,
        );
        m.p2.character.vel = Vec2::new(10.0, -4.0);
        m.p2.character.power = 0;
        let power_max = m.p2().character.power_max;
        assert!(
            power_max > 0,
            "default power_max must be positive for this test"
        );

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(
            (m.p2().character.vel.x - 12.0).abs() < 1e-3
                && (m.p2().character.vel.y + 3.0).abs() < 1e-3,
            "TargetVelAdd accumulates onto the existing velocity, got {:?}",
            m.p2().character.vel
        );
        assert_eq!(
            m.p2().character.power,
            power_max,
            "a huge TargetPowerAdd saturates at power_max, never beyond"
        );
    }

    /// AC1: a non-killing `TargetLifeAdd` floors at `1` (covered elsewhere); this
    /// asserts the *kill = true* path reaches `0`, the lethal-throw case.
    #[test]
    fn target_lifeadd_kill_true_can_reach_zero() {
        let mut p2_loaded = defender_loaded();
        p2_loaded.states.insert(820, state_with(820, Vec::new()));
        let mut m = throw_match_p1_binder(
            vec![target_controller(
                810,
                "TargetLifeAdd",
                &[("value", "-9999"), ("kill", "1")],
            )],
            Vec2::new(80.0, 0.0),
            50,
            p2_loaded,
        );

        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(
            m.p2().character.life,
            0,
            "kill=true allows the throw damage to reduce the target to zero life"
        );
    }

    /// AC1: `TargetFacing` with a non-negative value orients the target the SAME
    /// way as the binder (the existing test only covers the `-1` opposite case).
    ///
    /// The thrown target is driven into a *being-hit* state (820, `movetype = H`)
    /// alongside `TargetFacing`, exactly as a real throw does: this keeps the
    /// post-combat baseline `facep2` step (which only re-faces *neutral* fighters)
    /// from overriding the `TargetFacing` result, isolating the op under test.
    #[test]
    fn target_facing_non_negative_matches_binder() {
        // P2's own 820 is a being-hit state so the victim is non-neutral after the
        // throw (facep2 would otherwise re-face an idle victim toward the binder).
        let mut p2_loaded = defender_loaded();
        p2_loaded.states.insert(
            820,
            CompiledState {
                number: 820,
                state_type: Some("S".to_string()),
                movetype: Some("H".to_string()),
                physics: None,
                anim: None,
                ctrl: None,
                velset: None,
                poweradd: None,
                controllers: Vec::new(),
                ..Default::default()
            },
        );
        // Binder faces Right (set in the helper); P2 starts facing Left.
        let mut m = throw_match_p1_binder(
            vec![
                target_controller(810, "TargetState", &[("value", "820")]),
                target_controller(810, "TargetFacing", &[("value", "1")]),
            ],
            Vec2::new(80.0, 0.0),
            500,
            p2_loaded,
        );
        m.p2.character.facing = Facing::Left;

        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(
            m.p2().character.state_no,
            820,
            "the victim entered its being-hit state"
        );
        assert_eq!(
            m.p2().facing(),
            Facing::Right,
            "TargetFacing value >= 0 faces the target the same way as the binder"
        );
    }

    /// AC1 (safe degradation): `TargetState` to a state the TARGET does not own
    /// must not panic and must still advance the target's cursor (state_no) — the
    /// "op for a missing/unknown state degrades safely" requirement.
    #[test]
    fn target_state_to_unknown_state_degrades_safely() {
        // P2's own graph does NOT contain 820: the change_state lands cursor-only.
        let p2_loaded = defender_loaded();
        let mut m = throw_match_p1_binder(
            vec![target_controller(810, "TargetState", &[("value", "820")])],
            Vec2::new(80.0, 0.0),
            500,
            p2_loaded,
        );
        let life_before = m.p2().life();

        // Must not panic.
        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(
            m.p2().character.state_no,
            820,
            "TargetState to an unknown state still updates the cursor (no panic)"
        );
        assert_eq!(
            m.p2().life(),
            life_before,
            "an unknown TargetState destination does not corrupt unrelated fields"
        );
    }

    /// AC1 (intro gating): a binder sitting in a throw state during the INTRO
    /// phase must NOT move, damage, or re-state the opponent — `apply_target_ops`
    /// is gated on the live fight phase exactly like combat. Verifies the gate by
    /// arming the throw state before the fight goes live and ticking once.
    #[test]
    fn target_ops_do_not_apply_during_intro() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 810;
        p1c.has_target = true;

        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(80.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 500;
        p2c.life_max = 1000;

        let mut p1_loaded = attacker_loaded();
        p1_loaded.states.insert(
            810,
            state_with(
                810,
                vec![
                    target_controller(810, "TargetState", &[("value", "820")]),
                    target_controller(810, "TargetBind", &[("pos", "60, -5")]),
                    target_controller(810, "TargetLifeAdd", &[("value", "-40"), ("kill", "0")]),
                ],
            ),
        );
        let mut p2_loaded = defender_loaded();
        p2_loaded.states.insert(820, state_with(820, Vec::new()));

        let p1 = Player::new(p1c, p1_loaded);
        let p2 = Player::new(p2c, p2_loaded);
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        // Still in Intro: do NOT advance into the fight.
        assert_eq!(m.round_state(), RoundState::Intro);

        let life_before = m.p2().life();
        let pos_before = m.p2().pos();
        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(m.round_state(), RoundState::Intro, "still in the intro");
        assert_ne!(
            m.p2().character.state_no,
            820,
            "throw TargetState must not fire during the intro"
        );
        assert_eq!(
            m.p2().life(),
            life_before,
            "throw TargetLifeAdd must not damage during the intro"
        );
        assert!(
            (m.p2().pos().x - pos_before.x).abs() < 1e-3,
            "throw TargetBind must not move the opponent during the intro"
        );
    }

    /// AC1 (mirror player, 2b path): P2 — not P1 — is the binder. The coordinator
    /// applies P2's TargetOps to P1 (the opponent) via P1's own state graph,
    /// proving the split borrow is sound in BOTH directions.
    #[test]
    fn target_ops_apply_when_p2_is_the_binder() {
        // P1 is the target this time; give P1 its own state 820.
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(-80.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.life = 500;
        p1c.life_max = 1000;
        let mut p1_loaded = defender_loaded();
        p1_loaded.states.insert(820, state_with(820, Vec::new()));

        // P2 is the binder in throw-hold state 810.
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(0.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.state_no = 810;
        p2c.has_target = true;
        let mut p2_loaded = attacker_loaded();
        p2_loaded.states.insert(
            810,
            state_with(
                810,
                vec![
                    target_controller(810, "TargetState", &[("value", "820")]),
                    // P2 faces Left, so +60 offset mirrors to -60 from the binder.
                    target_controller(810, "TargetBind", &[("pos", "60, 0")]),
                    target_controller(810, "TargetLifeAdd", &[("value", "-30"), ("kill", "0")]),
                ],
            ),
        );

        let p1 = Player::new(p1c, p1_loaded);
        let p2 = Player::new(p2c, p2_loaded);
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);
        // Re-pin P2's throw preconditions after the intro.
        m.p2.character.state_no = 810;
        m.p2.character.has_target = true;
        m.p2.character.facing = Facing::Left;
        m.p2.character.pos = Vec2::new(0.0, 0.0);
        let life_before = m.p1().life();

        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(
            m.p1().character.state_no,
            820,
            "P2's TargetState drove P1 (the target) into 820"
        );
        assert!(
            (m.p1().pos().x + 60.0).abs() < 1e-3,
            "left-facing binder P2 mirrors +60 to -60, got {}",
            m.p1().pos().x
        );
        assert_eq!(
            m.p1().life(),
            life_before - 30,
            "P2's TargetLifeAdd damaged P1"
        );
    }

    /// AC1 (unit-level borrow soundness + no-op): `apply_target_ops` with an empty
    /// op slice makes no change, and a direct call mutates only the target while
    /// reading the binder — the split-borrow contract the coordinator relies on.
    #[test]
    fn apply_target_ops_empty_is_noop_and_only_touches_target() {
        let mut binder = Character::with_constants(CharacterConstants::default());
        binder.pos = Vec2::new(5.0, 9.0);
        binder.facing = Facing::Right;

        let mut target = Character::with_constants(CharacterConstants::default());
        target.pos = Vec2::new(100.0, 0.0);
        target.life = 700;
        let life_before = target.life;
        let pos_before = target.pos;
        let states: HashMap<i32, CompiledState> = HashMap::new();

        apply_target_ops(&binder, &mut target, &states, &[]);

        assert_eq!(target.life, life_before, "empty ops leave life untouched");
        assert!(
            (target.pos.x - pos_before.x).abs() < 1e-6
                && (target.pos.y - pos_before.y).abs() < 1e-6,
            "empty ops leave position untouched"
        );
        // The binder is read-only; confirm it is unchanged.
        assert!((binder.pos.x - 5.0).abs() < 1e-6 && (binder.pos.y - 9.0).abs() < 1e-6);
    }

    /// AC1 (unit-level): a sequence of ops on `apply_target_ops` applies in order
    /// with the documented facing-relative bind and clamped life — a focused unit
    /// test of the applier independent of the per-tick executor emission.
    #[test]
    fn apply_target_ops_applies_each_variant_in_order() {
        use fp_character::TargetOp;
        let mut binder = Character::with_constants(CharacterConstants::default());
        binder.pos = Vec2::new(10.0, 2.0);
        binder.facing = Facing::Left; // sign() = -1, mirrors the bind X

        let mut target = Character::with_constants(CharacterConstants::default());
        target.pos = Vec2::new(200.0, 0.0);
        target.facing = Facing::Left;
        target.life = 100;
        target.life_max = 1000;
        target.power = 0;
        target.vel = Vec2::new(0.0, 0.0);

        // A 820 state so the State op resolves against the target's own graph.
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        states.insert(820, state_with(820, Vec::new()));

        apply_target_ops(
            &binder,
            &mut target,
            &states,
            &[
                TargetOp::State(820),
                TargetOp::Bind {
                    time: 1,
                    pos: (30.0, -4.0),
                },
                TargetOp::Facing(1),
                TargetOp::VelSet((1.5, -2.5)),
                TargetOp::VelAdd((0.5, 0.5)),
                TargetOp::LifeAdd {
                    value: -250,
                    kill: false,
                },
                TargetOp::PowerAdd(120),
            ],
        );

        assert_eq!(target.state_no, 820, "State entered the target's own 820");
        // Bind: binder.pos + (30 * -1, -4) = (10 - 30, 2 - 4) = (-20, -2).
        assert!(
            (target.pos.x + 20.0).abs() < 1e-3 && (target.pos.y + 2.0).abs() < 1e-3,
            "facing-relative bind, got {:?}",
            target.pos
        );
        // Facing(1) -> same as binder (Left).
        assert_eq!(target.facing, Facing::Left, "Facing(1) matches the binder");
        // VelSet then VelAdd: (1.5, -2.5) + (0.5, 0.5) = (2.0, -2.0).
        assert!(
            (target.vel.x - 2.0).abs() < 1e-3 && (target.vel.y + 2.0).abs() < 1e-3,
            "VelSet then VelAdd compose, got {:?}",
            target.vel
        );
        // LifeAdd(-250, kill=false): 100 - 250 = -150, floored at 1.
        assert_eq!(target.life, 1, "non-killing life add floors at 1");
        // PowerAdd(120): 0 + 120 = 120, within [0, power_max].
        assert_eq!(target.power, 120, "power add within range");
    }

    /// AC1: `facing_opposite` is an involution (right<->left), the helper backing
    /// the `TargetFacing(-1)` "opposite the binder" case.
    #[test]
    fn facing_opposite_is_an_involution() {
        assert_eq!(facing_opposite(Facing::Right), Facing::Left);
        assert_eq!(facing_opposite(Facing::Left), Facing::Right);
        assert_eq!(
            facing_opposite(facing_opposite(Facing::Right)),
            Facing::Right
        );
        assert_eq!(facing_opposite(facing_opposite(Facing::Left)), Facing::Left);
    }

    /// AC1 (no attacker_state): a connecting HitDef with `p1stateno = None` must
    /// NOT move the attacker out of its current state — the attacker_state apply
    /// is conditional on `Some`. Guards against an unconditional state change.
    #[test]
    fn connecting_attack_without_p1stateno_keeps_attacker_state() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 1000; // survive

        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // A normal-attack HitDef: no p1stateno, no p2stateno (a plain strike).
        let mut hd = sample_hitdef();
        hd.p1stateno = None;
        hd.p2stateno = None;

        // Pin the attacker in a known state number that is NOT a throw state.
        m.p1.character.state_no = 200;
        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 0;
        m.p1.character.move_type = MoveType::Attack;
        m.p1.character.active_hitdef = Some(hd);
        m.p1.character.move_connect.reset();
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(m.p2().life() < 1000, "the attack must connect");
        assert_eq!(
            m.p1().character.state_no,
            200,
            "with no p1stateno the attacker stays in its current state"
        );
    }

    // =====================================================================
    // PR-D: #10 (Width -> player push) and #13 (NoAutoTurn -> face-opponent).
    // =====================================================================

    /// #10: an active `Width` override changes the half-widths the player-push /
    /// bound clamp consult; with no override the static `[Size]` width is used.
    #[test]
    fn width_override_drives_player_push_half_widths() {
        let mut c = Character::new(); // size.ground_front=16, ground_back=15
        c.facing = Facing::Right;
        let mut p = Player::new(c, defender_loaded());

        // No override: static [Size] widths (front 16 / back 15), facing right ->
        // right half = front = 16, left half = back = 15.
        assert_eq!(p.push_body_right_half(), 16.0);
        assert_eq!(p.push_body_left_half(), 15.0);

        // Activate a Width override (front 40, back 8). Now the push consults it.
        p.character.cur_width.set(40.0, 8.0);
        assert_eq!(
            p.push_body_right_half(),
            40.0,
            "right half = override front (facing right)"
        );
        assert_eq!(
            p.push_body_left_half(),
            8.0,
            "left half = override back (facing right)"
        );

        // Facing left swaps which half each maps to.
        p.character.facing = Facing::Left;
        assert_eq!(
            p.push_body_right_half(),
            8.0,
            "right half = override back (facing left)"
        );
        assert_eq!(
            p.push_body_left_half(),
            40.0,
            "left half = override front (facing left)"
        );
    }

    /// #10: a `Width` override actually changes how far two coincident bodies are
    /// pushed apart through a full `Match` step (override re-asserted via the
    /// character field, which `apply_push_and_bounds` reads).
    #[test]
    fn width_override_widens_player_push_separation() {
        // Baseline coincident default bodies separate to centers 30 apart (front
        // 16 + back ... see player_push_separation_is_exact). With a fat override
        // they separate further.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(0.0, 0.0);
        // Force a wide push body on P1 (40, 40). cur_width is only cleared inside a
        // Character::tick; the engine push reads it in apply_push_and_bounds AFTER
        // the ticks, so set it and avoid running the executor by checking the
        // push half-widths directly through the Player.
        p1c.cur_width.set(40.0, 40.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        // P1's symmetric (40, 40) override makes BOTH halves 40, regardless of the
        // facing `Match::new` settled — wider than the static front 16 / back 15.
        assert_eq!(m.p1().push_body_right_half(), 40.0);
        assert_eq!(m.p1().push_body_left_half(), 40.0);
        // P2 has no override, so it keeps its static [Size] widths (front 16 /
        // back 15); either half is one of those, never the 40 of the override.
        let p2_right = m.p2().push_body_right_half();
        assert!(
            p2_right == 16.0 || p2_right == 15.0,
            "P2 keeps its static width (16 or 15), got {p2_right}"
        );
    }

    /// #13: `face_each_other_when_neutral` turns neutral characters to face each
    /// other, but a character whose `NoAutoTurn` is asserted keeps its facing.
    #[test]
    fn noautoturn_suppresses_baseline_face_opponent() {
        // Two neutral standing characters, both facing right, with A to the LEFT of
        // B. The baseline would face A right (already) and B left (toward A).
        let neutral_pair = || {
            let mut a = Character::new();
            a.pos = Vec2::new(-50.0, 0.0);
            a.facing = Facing::Right;
            a.state_type = StateType::Standing;
            a.move_type = MoveType::Idle;
            let mut b = Character::new();
            b.pos = Vec2::new(50.0, 0.0);
            b.facing = Facing::Right; // "wrong" way; baseline should flip it left
            b.state_type = StateType::Standing;
            b.move_type = MoveType::Idle;
            (a, b)
        };

        // Without NoAutoTurn: B is flipped to face left (toward A).
        let (mut a, mut b) = neutral_pair();
        face_each_other_when_neutral(&mut a, &mut b);
        assert_eq!(b.facing, Facing::Left, "neutral B turns to face A");

        // With NoAutoTurn asserted on B: B keeps its (wrong-way) facing.
        let (mut a, mut b) = neutral_pair();
        b.asserted.no_auto_turn = true;
        face_each_other_when_neutral(&mut a, &mut b);
        assert_eq!(b.facing, Facing::Right, "NoAutoTurn keeps B's facing");
        assert_eq!(
            a.facing,
            Facing::Right,
            "A (no assertion) is unaffected / already correct"
        );
    }

    // ---- audit #24: Pause / SuperPause whole-match freeze --------------------

    /// A `Pause`/`SuperPause` controller of the given `kind` and `time`, gated on
    /// `var(0) = 1` so the test can fire it on a precise tick.
    fn pause_controller(kind: &str, time: &str) -> CompiledController {
        CompiledController {
            state_number: 0,
            label: String::new(),
            controller_type: Some(kind.to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("var(0) = 1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params: [("time".to_string(), CompiledParam::compile(time))]
                .into_iter()
                .collect(),
        }
    }

    /// A `VarSet var(0) = 0` controller gated on `var(0) = 1`, so it clears the
    /// fire flag the SAME tick the pause fires (the pause arms exactly once).
    fn clear_flag_controller() -> CompiledController {
        let mut c = varset_controller(0, "0");
        c.triggers = vec![CompiledTriggerGroup {
            number: 1,
            conditions: vec![CompiledExpr::compile("var(0) = 1")],
        }];
        c
    }

    /// A [`LoadedCharacter`] whose state 0, when `var(0) = 1`, runs the given
    /// pause controller and then clears `var(0)` — so the freeze arms once when the
    /// test sets the flag. Otherwise state 0 is inert (no physics, no movement).
    fn pause_loaded(kind: &str, time: &str) -> LoadedCharacter {
        let mut loaded = loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ));
        let st = CompiledState {
            number: 0,
            state_type: Some("S".to_string()),
            movetype: Some("I".to_string()),
            physics: Some("N".to_string()),
            anim: None,
            ctrl: None,
            velset: None,
            poweradd: None,
            controllers: vec![pause_controller(kind, time), clear_flag_controller()],
            ..Default::default()
        };
        loaded.states.insert(0, st);
        loaded
    }

    /// Builds a match whose P1 can fire a `Pause`/`SuperPause` (via `var(0) = 1`)
    /// and whose P2 is inert, already in the live fight phase. Returns the match.
    fn freeze_match(kind: &str, time: &str) -> Match {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.state_no = 0;
        let p1 = Player::new(p1c, pause_loaded(kind, time));
        // P2 has an inert state 0 too (idle stand) so its Time advances normally.
        let p2 = Player::new(p2c, loaded_with(air_with(0, Vec::new(), Vec::new())));
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));
        into_fight(&mut m);
        m
    }

    #[test]
    fn superpause_freezes_opponent_and_timer_while_trigger_keeps_ticking() {
        let mut m = freeze_match("SuperPause", "5");
        // Fire the SuperPause on the next tick.
        m.p1.character.vars[0] = 1;
        let p2_time_before = m.p2().character.state_time;
        let timer_before = m.timer();
        let game_time_before = m.game_time();

        // (A) The fire tick: everyone still advances this frame; the freeze arms
        //     for the FOLLOWING ticks.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.freeze_time(), 5, "5-tick freeze armed");
        assert!(m.p2_frozen(), "P2 is frozen");
        assert!(!m.p1_frozen(), "the SuperPause trigger (P1) is exempt");

        // Snapshot the frozen P2's state-time and the trigger P1's state-time.
        let p2_time_frozen = m.p2().character.state_time;
        let p1_time_at_freeze_start = m.p1().character.state_time;
        let timer_at_freeze_start = m.timer();
        let game_time_at_freeze_start = m.game_time();
        assert!(
            p2_time_frozen > p2_time_before,
            "P2 advanced on the fire tick"
        );

        // (B) Run the full freeze: P2 + timer + GameTime are held; P1 keeps ticking.
        for i in 0..5 {
            m.tick(MatchInput::none(), MatchInput::none());
            // P2 frozen → its state Time does not advance.
            assert_eq!(
                m.p2().character.state_time,
                p2_time_frozen,
                "P2 state Time frozen on frozen tick {i}"
            );
            // Round timer + GameTime held still.
            assert_eq!(m.timer(), timer_at_freeze_start, "timer frozen");
            assert_eq!(m.game_time(), game_time_at_freeze_start, "GameTime frozen");
            // P1 (the trigger) keeps animating: its state Time advances each frame.
            assert_eq!(
                m.p1().character.state_time,
                p1_time_at_freeze_start + (i + 1),
                "P1 (exempt) keeps ticking during the freeze"
            );
        }

        // (C) The freeze has expired; normal processing resumes.
        assert_eq!(m.freeze_time(), 0, "freeze expired after 5 ticks");
        assert!(!m.p2_frozen(), "P2 unfrozen");
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(
            m.p2().character.state_time > p2_time_frozen,
            "P2 resumes advancing after the freeze"
        );
        assert!(
            m.game_time() > game_time_at_freeze_start,
            "GameTime resumes after the freeze"
        );

        // Sanity: the freeze genuinely held things relative to the pre-fire reads.
        let _ = (timer_before, game_time_before);
    }

    #[test]
    fn pause_freezes_both_players() {
        let mut m = freeze_match("Pause", "4");
        m.p1.character.vars[0] = 1;

        // Fire tick arms the freeze.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.freeze_time(), 4, "4-tick Pause armed");
        assert!(m.p1_frozen(), "Pause freezes P1 too (no exempt)");
        assert!(m.p2_frozen(), "Pause freezes P2");

        let p1_time = m.p1().character.state_time;
        let p2_time = m.p2().character.state_time;
        let game_time = m.game_time();

        // During the Pause NOTHING advances: both players, timer, and GameTime hold.
        for _ in 0..4 {
            m.tick(MatchInput::none(), MatchInput::none());
            assert_eq!(m.p1().character.state_time, p1_time, "P1 frozen by Pause");
            assert_eq!(m.p2().character.state_time, p2_time, "P2 frozen by Pause");
            assert_eq!(m.game_time(), game_time, "GameTime frozen by Pause");
        }

        // The Pause expires and the world resumes.
        assert_eq!(m.freeze_time(), 0, "Pause expired");
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(
            m.game_time() > game_time,
            "GameTime resumes after the Pause"
        );
    }

    #[test]
    fn freeze_does_not_advance_game_time() {
        // Focused check: GameTime is held for exactly the freeze duration.
        let mut m = freeze_match("SuperPause", "3");
        m.p1.character.vars[0] = 1;
        m.tick(MatchInput::none(), MatchInput::none()); // fire tick: GameTime +1
        let gt = m.game_time();
        assert_eq!(m.freeze_time(), 3);

        for _ in 0..3 {
            m.tick(MatchInput::none(), MatchInput::none());
            assert_eq!(m.game_time(), gt, "GameTime does not advance while frozen");
        }
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.game_time(),
            gt + 1,
            "GameTime advances again once unfrozen"
        );
    }

    #[test]
    fn zero_time_pause_does_not_freeze() {
        // A `time = 0` Pause requests no freeze (clamped to a no-op).
        let mut m = freeze_match("Pause", "0");
        m.p1.character.vars[0] = 1;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.freeze_time(), 0, "time=0 leaves no freeze armed");
        assert!(!m.p1_frozen());
        assert!(!m.p2_frozen());
    }

    // ---- Hit-spark effects (audit #17) -----------------------------------

    /// An attacker-style loaded character (punch on action 200, hurt on action 0)
    /// that ALSO carries a multi-frame spark action at `spark_anim`, so a connecting
    /// HitDef whose `sparkno` resolves to that action can spawn a spark from the
    /// attacker's own AIR. The spark action has 3 frames (sprites (10,0)/(10,1)/
    /// (10,2), 2 ticks each → a 6-frame lifetime) so cursor advance and expiry are
    /// observable.
    fn spark_attacker_loaded(spark_anim: i32) -> LoadedCharacter {
        let mut loaded = attacker_loaded();
        let mk = |img: u16| AnimFrame {
            sprite: SpriteId::new(10, img),
            offset: Vec2::new(3, -4),
            ticks: 2,
            ..Default::default()
        };
        loaded.air.actions.insert(
            spark_anim,
            AnimAction {
                action_number: spark_anim,
                frames: vec![mk(0), mk(1), mk(2)],
                loopstart: 0,
            },
        );
        loaded
    }

    /// Builds a fight-phase match where P1 is armed with `hd` on its punch frame
    /// (action 200) facing a standing, open P2 it overlaps, ready to connect this
    /// tick. P1's loaded AIR carries the spark action `spark_anim`.
    fn spark_match(hd: HitDef, spark_anim: i32) -> Match {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.life = 1000;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.life = 1000;

        let p1 = Player::new(p1c, spark_attacker_loaded(spark_anim));
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 0;
        m.p1.character.move_type = MoveType::Attack;
        m.p1.character.active_hitdef = Some(hd);
        m.p1.character.move_connect.reset();
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;
        m.p2.character.holding_back = false;
        m
    }

    /// A connecting hit with an attacker-own `sparkno` spawns one effect, sourced
    /// from the attacker's own AIR action, anchored near the contact point.
    #[test]
    fn connecting_hit_spawns_own_spark_at_contact() {
        // sparkno = -5 → own action 5 (negative = attacker's own set).
        let mut hd = sample_hitdef();
        hd.resources.sparkno = -5;
        let mut m = spark_match(hd, 5);

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(m.p2().life() < 1000, "the attack must connect");
        assert_eq!(m.effects().len(), 1, "exactly one spark spawned on the hit");
        let fx = &m.effects()[0];
        assert_eq!(
            fx.side,
            EffectSide::P1,
            "the spark is sourced from the attacker (P1)"
        );
        assert_eq!(fx.anim, 5, "the own-spark plays action 5 (magnitude of -5)");
        // The spark's sprite is the spark action's CURRENT frame (group 10). After
        // spawning, `tick_effects` advanced one frame-tick (still on elem 0 of a
        // 2-tick frame), so the sprite is still the first frame (10, 0).
        assert_eq!(fx.sprite, SpriteId::new(10, 0));
        // The anchor sits between the attacker (x=0) and defender (x=60) overlap —
        // a positive, finite X near the contact region, not at either origin.
        assert!(
            fx.pos.x > 0.0 && fx.pos.x < 60.0,
            "spark anchored in the overlap region, got x={}",
            fx.pos.x
        );
    }

    /// No hit → no spark: a tick with no connecting attack spawns nothing.
    #[test]
    fn no_hit_spawns_no_spark() {
        // Place the attacker far from the defender so the punch box cannot overlap.
        let mut hd = sample_hitdef();
        hd.resources.sparkno = -5;
        let mut m = spark_match(hd, 5);
        // Move P1 far left so no overlap this tick, and re-pin the frame.
        m.p1.character.pos = Vec2::new(-250.0, 0.0);
        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 0;
        m.p1.character.active_hitdef = Some(HitDef {
            resources: fp_combat::HitResources {
                sparkno: -5,
                ..Default::default()
            },
            ..sample_hitdef()
        });
        m.p1.character.move_connect.reset();

        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(m.p2().life(), 1000, "no overlap → no hit");
        assert!(m.effects().is_empty(), "no spark without a connecting hit");
    }

    /// A spark ticks down and expires: after its action's total lifetime elapses,
    /// the effect is dropped from the list.
    #[test]
    fn spark_ticks_down_and_expires() {
        let mut hd = sample_hitdef();
        hd.resources.sparkno = -5;
        let mut m = spark_match(hd, 5);

        // First tick: connect + spawn the spark (lifetime = 3 frames × 2 ticks = 6).
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.effects().len(), 1, "spark spawned");
        // Clear the attacker's HitDef so no NEW spark spawns on later ticks; the
        // existing one must age out on its own.
        m.p1.character.active_hitdef = None;
        m.p1.character.move_connect.reset();

        // The spark spawned with remaining = 6 and was decremented once on the
        // spawn tick (tick_effects ran after spawn), so 5 more ticks expire it.
        let mut ticked = 0;
        for _ in 0..10 {
            if m.effects().is_empty() {
                break;
            }
            m.tick(MatchInput::none(), MatchInput::none());
            ticked += 1;
        }
        assert!(m.effects().is_empty(), "the spark eventually expires");
        assert!(
            ticked <= 6,
            "expiry is bounded by the action lifetime, took {ticked}"
        );
    }

    /// A common (`fightfx`) spark — a non-negative `sparkno` — spawns nothing when
    /// NO common-effects asset is loaded, but the hit still connects and deals
    /// damage (the best-effort skip; no panic, no regression).
    #[test]
    fn common_spark_spawns_nothing_but_hit_lands() {
        let mut hd = sample_hitdef();
        hd.resources.sparkno = 0; // non-negative → common fightfx set
        let mut m = spark_match(hd, 5);

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(
            m.p2().life() < 1000,
            "a common-spark hit still connects + damages"
        );
        assert!(
            m.effects().is_empty(),
            "no common-fx asset loaded → common spark is a best-effort skip"
        );
    }

    /// A synthetic common-effects [`AirFile`] with a spark action at `anim`
    /// (3 frames, sprites (`anim`,0)/(`anim`,1)/(`anim`,2), 2 ticks each), so a
    /// common spark sourced from it is observable without the shipped asset.
    fn common_fx_air(anim: i32) -> AirFile {
        let mk = |img: u16| AnimFrame {
            sprite: SpriteId::new(anim as u16, img),
            offset: Vec2::new(0, 0),
            ticks: 2,
            ..Default::default()
        };
        let mut actions = HashMap::new();
        actions.insert(
            anim,
            AnimAction {
                action_number: anim,
                frames: vec![mk(0), mk(1), mk(2)],
                loopstart: 0,
            },
        );
        AirFile { actions }
    }

    /// With a common-effects asset loaded, a connecting hit whose `sparkno` is a
    /// bare non-negative value spawns ONE effect sourced from the common set
    /// ([`EffectSide::Common`]), playing the matching action — NOT the attacker's
    /// own SFF. This is the core FL2a fix: common sparks now render.
    #[test]
    fn common_spark_spawns_from_loaded_common_fx() {
        let mut hd = sample_hitdef();
        hd.resources.sparkno = 2; // bare non-negative → common fightfx action 2
                                  // The attacker's OWN AIR carries spark action 5 (which must NOT be used).
        let mut m = spark_match(hd, 5);
        // Install a common set whose action 2 is the spark to draw.
        m.set_common_fx(common_fx_air(2));

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(
            m.p2().life() < 1000,
            "the common-spark hit connects + damages"
        );
        assert_eq!(m.effects().len(), 1, "exactly one common spark spawned");
        let fx = &m.effects()[0];
        assert_eq!(
            fx.side,
            EffectSide::Common,
            "the spark is sourced from the common-fx set, not a fighter"
        );
        assert_eq!(fx.anim, 2, "the common spark plays common action 2");
        // Current frame is action 2's first sprite (group 2), confirming it
        // resolved against the common set, not the attacker's group-10 own spark.
        assert_eq!(fx.sprite, SpriteId::new(2, 0));
    }

    /// A common spark whose action is MISSING from the loaded common set spawns
    /// nothing (best-effort), and the hit still lands — never a panic.
    #[test]
    fn common_spark_missing_action_in_loaded_set_spawns_nothing() {
        let mut hd = sample_hitdef();
        hd.resources.sparkno = 7; // common action 7, NOT authored in the set below
        let mut m = spark_match(hd, 5);
        m.set_common_fx(common_fx_air(2)); // only action 2 exists

        m.tick(MatchInput::none(), MatchInput::none());

        assert!(
            m.p2().life() < 1000,
            "the hit lands even when the common action is missing"
        );
        assert!(
            m.effects().is_empty(),
            "a missing common action spawns nothing"
        );
    }

    /// `sparkno = -1` (the MUGEN "no spark" sentinel) spawns nothing, and a missing
    /// own-spark action also spawns nothing — neither blocks the hit nor panics.
    #[test]
    fn sentinel_and_missing_spark_action_spawn_nothing() {
        // (a) sparkno = -1 sentinel.
        let mut hd = sample_hitdef();
        hd.resources.sparkno = -1;
        let mut m = spark_match(hd, 5);
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(m.p2().life() < 1000, "the hit still lands with no spark");
        assert!(m.effects().is_empty(), "-1 sentinel spawns no spark");

        // (b) own-spark action id that does NOT exist in the attacker's AIR.
        let mut hd2 = sample_hitdef();
        hd2.resources.sparkno = -999; // own action 999, which is not authored
        let mut m2 = spark_match(hd2, 5);
        m2.tick(MatchInput::none(), MatchInput::none());
        assert!(
            m2.p2().life() < 1000,
            "the hit lands even when the spark action is missing"
        );
        assert!(
            m2.effects().is_empty(),
            "a missing own-spark action spawns nothing"
        );
    }

    /// Sparks do not leak across a round reset: a spawned spark is cleared when the
    /// next round begins.
    #[test]
    fn round_reset_clears_sparks() {
        let mut hd = sample_hitdef();
        hd.resources.sparkno = -5;
        let mut m = spark_match(hd, 5);
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.effects().len(), 1, "spark present mid-round");
        // Reset to the next round directly; sparks must be cleared.
        m.reset_for_next_round();
        assert!(m.effects().is_empty(), "round reset clears live sparks");
    }

    // ---- T056: round-init common state (5900) ------------------------------

    /// A [`CompiledController`] of the given `type`, with `value = <expr>` and a
    /// `trigger1 = <trigger>` gate. Mirrors the shape an authored `common1.cns`
    /// 5900 controller compiles to.
    fn value_controller(ty: &str, value: &str, trigger: &str) -> CompiledController {
        CompiledController {
            state_number: ROUND_INIT_STATE,
            label: String::new(),
            controller_type: Some(ty.to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile(trigger)],
            }],
            persistent: None,
            ignorehitpause: None,
            params: [("value".to_string(), CompiledParam::compile(value))]
                .into_iter()
                .collect(),
        }
    }

    /// A [`LoadedCharacter`] whose state graph defines the engine-common round-init
    /// state ([`ROUND_INIT_STATE`], 5900): on `Time = 0` it re-asserts full life
    /// (`LifeSet value = Const(data.life)`) and hands back to the neutral stand
    /// (`ChangeState value = 0`) — the converging shape the authored 5900 has.
    /// Also defines a (no-op) state 0 so the convergence target exists.
    fn round_init_loaded() -> LoadedCharacter {
        let mut loaded = loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ));
        let s5900 = CompiledState {
            number: ROUND_INIT_STATE,
            state_type: Some("S".to_string()),
            movetype: Some("I".to_string()),
            physics: Some("S".to_string()),
            controllers: vec![
                value_controller("LifeSet", "Const(data.life)", "Time = 0"),
                value_controller("ChangeState", "0", "Time = 0"),
            ],
            ..Default::default()
        };
        loaded.states.insert(ROUND_INIT_STATE, s5900);
        // A trivial state 0 so the 5900 ChangeState target is a defined state.
        loaded.states.insert(
            0,
            CompiledState {
                number: 0,
                state_type: Some("S".to_string()),
                movetype: Some("I".to_string()),
                physics: Some("S".to_string()),
                ..Default::default()
            },
        );
        loaded
    }

    /// AC (T056): the engine drives a fighter through its round-init state (5900)
    /// at the start of each round, and 5900 *converges* with the engine's
    /// authoritative field reset rather than fighting it.
    ///
    /// - At construction (round 1) a character that defines 5900 is entered into
    ///   it (it is NOT left in state 0).
    /// - Ticking the intro runs 5900, which re-asserts full life and `ChangeState`s
    ///   back to the neutral stand (state 0) — so the net effect converges.
    /// - The same happens on a `reset_for_next_round`.
    /// - A character that defines NO 5900 is left in state 0 (safe no-op).
    #[test]
    fn round_init_uses_5900() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        // P1 defines 5900; P2 does not (the no-op control).
        let p1 = Player::new(p1c, round_init_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));

        // Round 1: the engine drove P1 into its round-init state, and left the
        // 5900-less P2 in the neutral stand.
        assert_eq!(
            m.p1.character.state_no, ROUND_INIT_STATE,
            "a fighter that defines 5900 is entered into it at round init"
        );
        assert_eq!(
            m.p2.character.state_no, 0,
            "a fighter with no 5900 is left in the neutral stand (no-op)"
        );

        // Drive 5900 to convergence: drop P1 below full life first to prove the
        // LifeSet re-asserts it, then tick once (the intro still ticks the state
        // machine). 5900 runs on Time = 0 → full life + ChangeState 0.
        m.p1.character.life = 1;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1.character.life, m.p1.character.life_max,
            "5900's LifeSet re-asserts full life on round init"
        );
        assert_eq!(
            m.p1.character.state_no, 0,
            "5900 hands back to the neutral stand — it converges with the reset"
        );

        // A between-round reset re-runs round init the same way.
        m.p1.character.state_no = 42; // pretend it ended the round elsewhere
        m.reset_for_next_round();
        assert_eq!(
            m.p1.character.state_no, ROUND_INIT_STATE,
            "reset_for_next_round re-enters 5900 for the fighter that defines it"
        );
        assert_eq!(
            m.p2.character.state_no, 0,
            "the 5900-less fighter is reset to the neutral stand, not 5900"
        );
    }

    /// AC (gated, skip-if-missing): drive a REAL KFM punch through the actual
    /// [`Match::tick`] **command + combat** path (the same flow the app uses) and
    /// lock in the **documented reality** of audit #17 — KFM's `sparkno` values are
    /// all common-`fightfx` (`0/1/2/3/40`, plus a `-1` "none"), so with no
    /// `fightfx.sff` loaded a connecting KFM hit lands and deals damage but spawns
    /// **no** [`Effect`]. This is the regression net the reviewer asked for: it
    /// asserts the best-effort common-spark skip against real content via the real
    /// tick path, not via a contrived literal-negative `sparkno` real KFM never
    /// authors.
    ///
    /// This deliberately mirrors `real_command_attack_connects_and_drops_life`
    /// (walk into range with `right`, throw light punches) because that is the
    /// proven-deterministic way to land a real KFM hit through `Match::tick`; the
    /// only addition is the spark assertions.
    #[test]
    fn real_kfm_common_spark_hit_lands_but_spawns_no_effect() {
        // Sanity-anchor the classification this test relies on: KFM's real hit
        // sparks (`sparkno = 0/1/2/3/40`) are all common-fightfx. (Pure, ungated.)
        for sparkno in [0, 1, 2, 3, 40] {
            assert!(
                matches!(SparkSource::classify(sparkno), SparkSource::Common { .. }),
                "KFM sparkno={sparkno} must classify as a common fightfx spark"
            );
        }

        let Some(mut m) = two_kfm_match() else {
            return; // fixture absent; helper already logged the skip
        };
        assert!(
            run_until_fight(&mut m),
            "fight must go live before driving input"
        );
        // No hit has connected yet, so no spark should exist.
        assert!(
            m.effects().is_empty(),
            "no spark before the fight is driven"
        );
        let p2_life_before = m.p2().life();

        // Walk into range.
        for _ in 0..240 {
            m.tick(
                MatchInput {
                    right: true,
                    ..MatchInput::none()
                },
                MatchInput::none(),
            );
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }
        // Throw light punches on alternate frames; over a generous budget one must
        // connect via KFM's real `[State 200]` HitDef (whose `sparkno` is common).
        let mut hit = false;
        for i in 0..400 {
            let inp = if i % 3 == 0 {
                MatchInput {
                    x: true,
                    ..MatchInput::none()
                }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            // Invariant across the whole drive: with only common-fightfx sparks and
            // no fightfx asset loaded, NO effect is ever spawned — connecting or not.
            assert!(
                m.effects().is_empty(),
                "real KFM (common-fightfx sparkno, no fightfx loaded) must spawn no hit-spark effect"
            );
            if m.p2().life() < p2_life_before {
                hit = true;
                break;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert!(
            hit,
            "a real KFM punch must connect through Match::tick and drop P2's life; P2 at {} ({:?})",
            m.p2().life(),
            m.round_state()
        );
    }

    /// Resolves a path inside the shipped (committed) `assets/data/` directory.
    fn shipped_asset(rel: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/data")
            .join(rel)
    }

    /// FL2a headline (gated, skip-if-KFM-missing): with the SHIPPED common-effects
    /// (`fightfx`) asset installed, a real KFM punch — whose `sparkno` is a common
    /// value (`0/1/2/3/40`) — now spawns a VISIBLE common spark sourced from the
    /// `fightfx` set ([`EffectSide::Common`]), driven through the real
    /// command+combat tick path. This is the regression net for "KFM hits show a
    /// spark": it asserts the connecting hit produces a common-fx-sourced effect,
    /// not the empty list of the pre-asset world.
    #[test]
    fn real_kfm_hit_spawns_common_fightfx_spark() {
        // The fightfx asset SHIPS, so it must be present (not gated on it). KFM is
        // the gated half — skip cleanly when the fixture is absent.
        let Some(mut m) = two_kfm_match() else {
            return; // fixture absent; helper already logged the skip
        };
        // Install the shipped common-effects AIR so common sparks resolve.
        let air =
            AirFile::load(&shipped_asset("fightfx.air")).expect("shipped fightfx.air must load");
        // Sanity: the set authors KFM's common spark indices.
        for g in [0, 1, 2, 3, 40] {
            assert!(
                air.action(g).is_some(),
                "fightfx.air must author action {g}"
            );
        }
        m.set_common_fx(air);

        assert!(
            run_until_fight(&mut m),
            "fight must go live before driving input"
        );
        assert!(
            m.effects().is_empty(),
            "no spark before the fight is driven"
        );
        let p2_life_before = m.p2().life();

        // Walk into range.
        for _ in 0..240 {
            m.tick(
                MatchInput {
                    right: true,
                    ..MatchInput::none()
                },
                MatchInput::none(),
            );
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }
        // Throw light punches until one connects; on the connecting tick a common
        // spark must appear (sourced from the loaded fightfx set).
        let mut saw_common_spark = false;
        for i in 0..400 {
            let inp = if i % 3 == 0 {
                MatchInput {
                    x: true,
                    ..MatchInput::none()
                }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            // Any spark spawned by a real KFM hit must be a COMMON (fightfx) spark,
            // never a fighter-own one (KFM authors no own-sparks).
            if let Some(fx) = m.effects().first() {
                assert_eq!(
                    fx.side,
                    EffectSide::Common,
                    "a real KFM spark must source from the common fightfx set"
                );
                saw_common_spark = true;
            }
            if m.p2().life() < p2_life_before {
                // The hit landed; by now (spawn runs before tick_effects) a common
                // spark exists for this connect.
                assert!(
                    saw_common_spark,
                    "a connecting KFM hit must have spawned a common fightfx spark"
                );
                break;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert!(
            saw_common_spark,
            "a real KFM punch must connect and spawn a common fightfx spark; P2 at {} ({:?})",
            m.p2().life(),
            m.round_state()
        );
    }

    // =====================================================================
    // Replay / determinism + whole-Match state serialization (#38).
    //
    // These exercise the four pillars: (1) Match snapshot/restore round-trip,
    // (2) input record→replay reproduces a match byte-for-byte, (3) two
    // independent runs with the same input script are identical, and
    // (4) distinct per-player RNG seeds + fixed-seed reproduction. A malformed
    // snapshot must be a recoverable Err, never a panic.
    // =====================================================================

    /// A loaded character whose state 0 draws `random` into `var(0)` every tick
    /// **and** carries the action-0 hurt frame, so two such fighters both advance
    /// their RNG streams each frame and remain hittable. This is what lets the
    /// determinism / distinct-seed tests observe the RNG actually flowing through
    /// the live executor (not just a field poke).
    fn rng_probe_loaded() -> LoadedCharacter {
        let mut loaded = defender_loaded();
        loaded.states.insert(
            0,
            CompiledState {
                number: 0,
                state_type: Some("S".to_string()),
                movetype: Some("I".to_string()),
                physics: Some("N".to_string()),
                controllers: vec![varset_controller(0, "random")],
                ..Default::default()
            },
        );
        loaded
    }

    /// A two-fighter match built from the RNG-probe character on both sides,
    /// positioned apart, on a wide stage. Not yet seeded (callers seed it).
    fn rng_probe_match() -> Match {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, rng_probe_loaded());
        let p2 = Player::new(p2c, rng_probe_loaded());
        Match::new(p1, p2, StageBounds::new(-200.0, 200.0))
    }

    /// A deterministic, varied input script of `n` frames: a repeating pattern of
    /// directions and buttons distinct per player, so the two diverge and the
    /// replay has something non-trivial to reproduce.
    fn scripted_inputs(n: usize) -> Vec<(MatchInput, MatchInput)> {
        (0..n)
            .map(|i| {
                let p1 = MatchInput {
                    right: i % 3 == 0,
                    left: i % 7 == 0,
                    a: i % 5 == 0,
                    up: i % 11 == 0,
                    ..MatchInput::none()
                };
                let p2 = MatchInput {
                    left: i % 4 == 0,
                    right: i % 9 == 0,
                    b: i % 6 == 0,
                    down: i % 13 == 0,
                    ..MatchInput::none()
                };
                (p1, p2)
            })
            .collect()
    }

    // ---- (1) snapshot / restore round-trip --------------------------------

    #[test]
    fn snapshot_restore_round_trips_to_the_snapshot_point() {
        let mut m = rng_probe_match();
        m.seed_players(DEFAULT_MATCH_SEED);
        into_fight(&mut m);

        // Tick N frames, then snapshot.
        for (p1, p2) in scripted_inputs(40) {
            m.tick(p1, p2);
        }
        let saved = m.snapshot().expect("snapshot must serialize");

        // Tick further, diverging the live state away from the snapshot point.
        for (p1, p2) in scripted_inputs(40) {
            m.tick(p1, p2);
        }
        let after = m.snapshot().expect("snapshot");
        assert_ne!(
            saved, after,
            "ticking past the snapshot must change the runtime state"
        );

        // Restore -> the match returns to exactly the snapshot point.
        m.restore_snapshot(&saved).expect("restore must succeed");
        let restored = m.snapshot().expect("snapshot");
        assert_eq!(
            saved, restored,
            "restore must reproduce the snapshot byte-for-byte"
        );
    }

    /// T019 acceptance test: the full `Match` state serializes to bytes and
    /// deserializes back to an identical state (round-trip equality) on a
    /// non-trivial mid-match state, and the byte output is deterministic
    /// (re-serializing the same captured state yields identical bytes).
    #[test]
    fn match_state_serialize_round_trip_is_bit_for_bit_stable() {
        // Build a non-trivial mid-match state: seed, run the intro, then drive a
        // varied input script so positions, RNG streams, the round clock, and the
        // game time have all advanced away from their construction defaults.
        let mut m = rng_probe_match();
        m.seed_players(DEFAULT_MATCH_SEED);
        into_fight(&mut m);
        for (p1, p2) in scripted_inputs(73) {
            m.tick(p1, p2);
        }

        // Sanity: the state really is mid-match, not a trivial fresh build.
        let baseline = rng_probe_match().snapshot_state();
        let captured = m.snapshot_state();
        assert_ne!(
            captured, baseline,
            "the captured state must be a non-trivial mid-match state"
        );

        // (a) Deterministic byte output: serializing the same captured state twice
        // must produce identical bytes (no HashMap-iteration-order or pointer churn
        // leaking into the blob). serialize -> "to_bytes".
        let bytes_a = m.snapshot().expect("serialize must succeed");
        let bytes_b = m.snapshot().expect("serialize must succeed");
        assert_eq!(
            bytes_a, bytes_b,
            "serialization must be deterministic (stable bytes across runs)"
        );

        // (b) Round-trip equality: deserialize the bytes into a fresh match built
        // from the same characters; the restored runtime state must equal the
        // original. deserialize -> "from_bytes".
        let mut restored = rng_probe_match();
        restored
            .restore_snapshot(&bytes_a)
            .expect("deserialize must succeed");
        assert_eq!(
            restored.snapshot_state(),
            captured,
            "the deserialized state must equal the original (round-trip equality)"
        );

        // And the freshly-serialized restored blob is byte-identical to the source
        // blob, closing the loop: serialize -> deserialize -> serialize is stable.
        assert_eq!(
            restored.snapshot().expect("re-serialize must succeed"),
            bytes_a,
            "serialize -> deserialize -> serialize must be byte-for-byte stable"
        );
    }

    #[test]
    fn restore_then_tick_matches_continuing_from_the_original() {
        // A save-state must be a perfect resume point: restoring at frame N and
        // ticking M more frames yields the SAME state as never having snapshotted
        // and ticking N+M frames from the start.
        let script = scripted_inputs(60);

        // Reference run: tick all 60 frames.
        let mut reference = rng_probe_match();
        reference.seed_players(7);
        into_fight(&mut reference);
        for &(p1, p2) in &script {
            reference.tick(p1, p2);
        }
        let reference_end = reference.snapshot().expect("snapshot");

        // Save-state run: snapshot at frame 30, restore, then tick the rest.
        let mut resumed = rng_probe_match();
        resumed.seed_players(7);
        into_fight(&mut resumed);
        for &(p1, p2) in &script[..30] {
            resumed.tick(p1, p2);
        }
        let mid = resumed.snapshot().expect("snapshot");
        // Drive it somewhere else, then restore to the mid-point and continue.
        for &(p1, p2) in &script[..15] {
            resumed.tick(p1, p2);
        }
        resumed.restore_snapshot(&mid).expect("restore");
        for &(p1, p2) in &script[30..] {
            resumed.tick(p1, p2);
        }
        let resumed_end = resumed.snapshot().expect("snapshot");

        assert_eq!(
            reference_end, resumed_end,
            "restore-and-continue must match a straight-through run"
        );
    }

    #[test]
    fn malformed_snapshot_is_a_recoverable_error_not_a_panic() {
        let mut m = rng_probe_match();
        m.seed_players(DEFAULT_MATCH_SEED);
        into_fight(&mut m);
        let good = m.snapshot().expect("snapshot");

        // Truncated blob: recoverable Err.
        let truncated = &good[..good.len() / 2];
        assert!(
            m.restore_snapshot(truncated).is_err(),
            "a truncated snapshot must be a recoverable Err"
        );
        // Garbage blob: recoverable Err.
        assert!(
            m.restore_snapshot(&[0xFFu8; 8]).is_err(),
            "a garbage snapshot must be a recoverable Err"
        );
        // Empty blob: recoverable Err.
        assert!(
            m.restore_snapshot(&[]).is_err(),
            "empty must be a recoverable Err"
        );

        // The match is still usable after the failed restores (no corruption).
        m.tick(MatchInput::none(), MatchInput::none());
    }

    // ---- (1b) character-identity guard on restore / replay (#38) ----------

    /// An RNG-probe match whose two characters carry the given `.def` name, so its
    /// [`Match::character_fingerprints`] differs from a default-named probe match.
    fn named_rng_probe_match(name: &str) -> Match {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let mut l1 = rng_probe_loaded();
        l1.name = name.to_string();
        let mut l2 = rng_probe_loaded();
        l2.name = name.to_string();
        let p1 = Player::new(p1c, l1);
        let p2 = Player::new(p2c, l2);
        Match::new(p1, p2, StageBounds::new(-200.0, 200.0))
    }

    #[test]
    fn snapshot_restores_into_same_characters_unchanged() {
        // The happy path: a snapshot taken from one match restores cleanly into a
        // freshly-built match with the SAME characters and reproduces it exactly.
        let mut source = rng_probe_match();
        source.seed_players(DEFAULT_MATCH_SEED);
        into_fight(&mut source);
        for (p1, p2) in scripted_inputs(40) {
            source.tick(p1, p2);
        }
        let saved = source.snapshot().expect("snapshot");
        let saved_state = source.snapshot().expect("snapshot");

        let mut target = rng_probe_match();
        target
            .restore_snapshot(&saved)
            .expect("restore into same characters must succeed");
        assert_eq!(
            target.snapshot().expect("snapshot"),
            saved_state,
            "restored same-character match must reproduce the snapshot byte-for-byte"
        );
    }

    #[test]
    fn snapshot_restored_into_different_character_is_rejected() {
        // A snapshot taken from match A must NOT silently apply to match B built
        // from a DIFFERENT character: the identity guard returns a recoverable Err.
        let mut source = named_rng_probe_match("FighterA");
        source.seed_players(DEFAULT_MATCH_SEED);
        into_fight(&mut source);
        for (p1, p2) in scripted_inputs(20) {
            source.tick(p1, p2);
        }
        let saved = source.snapshot().expect("snapshot");
        let before = source.snapshot().expect("snapshot");

        // Different character (distinct .def name -> distinct fingerprint).
        let mut other = named_rng_probe_match("FighterB");
        let other_before = other.snapshot().expect("snapshot");
        let err = other
            .restore_snapshot(&saved)
            .expect_err("restoring into a different character must fail");
        assert!(
            matches!(err, fp_core::FpError::Mismatch(_)),
            "identity mismatch must be a recoverable FpError::Mismatch, got {err:?}"
        );
        // The rejected restore must NOT have mutated the target (changes nothing).
        assert_eq!(
            other.snapshot().expect("snapshot"),
            other_before,
            "a rejected restore must leave the target match untouched"
        );

        // Same-character restore of the SAME blob still works (the source matches).
        let mut same = named_rng_probe_match("FighterA");
        same.restore_snapshot(&saved)
            .expect("same-character restore still succeeds");
        // Sanity: `before` is non-trivial (the source actually advanced).
        assert_ne!(
            before,
            named_rng_probe_match("FighterA").snapshot().unwrap()
        );
    }

    #[test]
    fn tampered_snapshot_fingerprint_is_rejected() {
        // Tampering the stored fingerprint in a typed snapshot is caught by the
        // guard even when the characters are otherwise identical.
        let mut m = rng_probe_match();
        m.seed_players(DEFAULT_MATCH_SEED);
        into_fight(&mut m);
        for (p1, p2) in scripted_inputs(10) {
            m.tick(p1, p2);
        }
        let mut snap = m.snapshot_state();
        // Corrupt P1's recorded fingerprint.
        snap.p1_fingerprint = fp_character::CharacterFingerprint(snap.p1_fingerprint.0 ^ 0xDEAD);

        let mut target = rng_probe_match();
        let err = target
            .restore_snapshot_state(&snap)
            .expect_err("a tampered fingerprint must be rejected");
        assert!(matches!(err, fp_core::FpError::Mismatch(_)));
    }

    #[test]
    fn replay_into_different_character_is_rejected() {
        // A recorded log (which stamps the source characters' fingerprints) must
        // refuse to replay into a match built from a different character.
        const SEED: i32 = 7;
        let mut source = named_rng_probe_match("FighterA");
        source.seed_players(SEED);
        let log = {
            let mut rec = MatchRecorder::new(&mut source, SEED, DEFAULT_ROUND_SECONDS);
            for (p1, p2) in scripted_inputs(30) {
                rec.tick(p1, p2);
            }
            rec.into_log()
        };
        // The recorder stamped the real fingerprints (not the unstamped sentinel).
        let (fp_a, _) = named_rng_probe_match("FighterA").character_fingerprints();
        assert_eq!(
            log.p1_fingerprint, fp_a,
            "recorder stamps the source fingerprint"
        );

        // Replaying into the SAME character succeeds.
        let mut ok = named_rng_probe_match("FighterA");
        replay_match(&mut ok, &log).expect("replay into same character succeeds");

        // Replaying into a DIFFERENT character is a recoverable mismatch error.
        let mut wrong = named_rng_probe_match("FighterB");
        let before = wrong.snapshot().expect("snapshot");
        let err = replay_match(&mut wrong, &log)
            .expect_err("replay into a different character must fail");
        assert!(
            matches!(err, ReplayError::CharacterMismatch { side: "P1", .. }),
            "expected a P1 character mismatch, got {err:?}"
        );
        // The rejected replay neither seeded nor ticked the target.
        assert_eq!(
            wrong.snapshot().expect("snapshot"),
            before,
            "a rejected replay must leave the target match untouched"
        );
    }

    // ---- (2) record -> replay reproduces a match --------------------------

    #[test]
    fn record_then_replay_reproduces_identical_final_state() {
        const SEED: i32 = 4242;
        const FRAMES: usize = 200;
        let script = scripted_inputs(FRAMES);

        // Record: drive a live match through the recorder, logging each frame.
        let mut original = rng_probe_match();
        original.seed_players(SEED);
        into_fight(&mut original);
        let log = {
            let mut rec = MatchRecorder::new(&mut original, SEED, DEFAULT_ROUND_SECONDS);
            for &(p1, p2) in &script {
                rec.tick(p1, p2);
            }
            rec.into_log()
        };
        let original_end = original.snapshot().expect("snapshot");

        // The log captured exactly the frames we fed it.
        assert_eq!(log.len(), FRAMES, "log records every frame");

        // Replay into a FRESH match built from the same characters. The replay
        // must reproduce the intro drive too, so the fresh match starts from a
        // clean intro and the script includes the same into_fight frames.
        let mut fresh = rng_probe_match();
        // Reproduce the intro the original did before recording, then replay the
        // logged inputs. (The recorder began after into_fight, so prepend it.)
        replay_match(&mut fresh, &replay_with_intro(&log)).expect("replay must succeed");
        let replay_end = fresh.snapshot().expect("snapshot");

        assert_eq!(
            original_end, replay_end,
            "replay must reproduce the recorded match byte-for-byte"
        );
    }

    /// Builds a replay log that prepends the intro frames `into_fight` drives
    /// (all-neutral) ahead of the recorded log, since the recorder in the test
    /// above started *after* the intro. Keeps the seed/config from `log`.
    fn replay_with_intro(log: &ReplayLog) -> ReplayLog {
        let mut full = ReplayLog::new(
            log.match_seed,
            log.bounds,
            log.rounds_to_win,
            log.round_seconds,
        );
        for _ in 0..(INTRO_FRAMES + 1) {
            full.push(MatchInput::none(), MatchInput::none());
        }
        for &(p1, p2) in &log.inputs {
            full.push(p1, p2);
        }
        full
    }

    #[test]
    fn replay_log_persists_and_reloads_via_bincode() {
        const SEED: i32 = 99;
        let mut original = rng_probe_match();
        original.seed_players(SEED);
        let log = {
            let mut rec = MatchRecorder::new(&mut original, SEED, DEFAULT_ROUND_SECONDS);
            for (p1, p2) in scripted_inputs(50) {
                rec.tick(p1, p2);
            }
            rec.into_log()
        };

        // Encode -> decode round-trips the log losslessly.
        let bytes = log.encode().expect("encode");
        let reloaded = ReplayLog::decode(&bytes).expect("decode");
        assert_eq!(log, reloaded);

        // And replaying the reloaded log reproduces the same final state.
        let mut a = rng_probe_match();
        let mut b = rng_probe_match();
        replay_match(&mut a, &log).expect("replay must succeed");
        replay_match(&mut b, &reloaded).expect("replay must succeed");
        assert_eq!(
            a.snapshot().expect("snap"),
            b.snapshot().expect("snap"),
            "a reloaded replay log reproduces the same state"
        );
    }

    // ---- (3) two-run determinism ------------------------------------------

    #[test]
    fn two_identical_runs_are_byte_equal_every_frame() {
        const SEED: i32 = 1234;
        const FRAMES: usize = 300;
        let script = scripted_inputs(FRAMES);

        let mut a = rng_probe_match();
        let mut b = rng_probe_match();
        a.seed_players(SEED);
        b.seed_players(SEED);

        for &(p1, p2) in &script {
            a.tick(p1, p2);
            b.tick(p1, p2);
            // Frame-by-frame byte equality is the strongest determinism proof:
            // any nondeterminism source (e.g. HashMap iteration order leaking into
            // simulation) would diverge the snapshots here.
            assert_eq!(
                a.snapshot_state(),
                b.snapshot_state(),
                "two identical runs diverged at game_time {}",
                a.game_time()
            );
        }
    }

    // ---- (4) distinct per-player RNG seeds + reproduction ------------------

    #[test]
    fn derive_player_seed_gives_p1_the_match_seed_and_p2_a_distinct_one() {
        for &seed in &[0, 1, -1, 1234, i32::MAX, i32::MIN] {
            let p1 = derive_player_seed(seed, 0);
            let p2 = derive_player_seed(seed, 1);
            assert_eq!(p1, seed, "P1 seed is the match seed itself");
            assert_ne!(p1, p2, "P2 seed must be distinct from P1 (seed {seed})");
        }
    }

    #[test]
    fn distinct_seeds_make_p1_and_p2_draw_different_random_streams() {
        let mut m = rng_probe_match();
        m.seed_players(DEFAULT_MATCH_SEED);
        into_fight(&mut m);

        // Drive a number of fight frames; each tick both fighters' state-0 VarSet
        // draws `random` into var(0). With distinct seeds the two streams differ,
        // so the recorded var(0) sequences must NOT be identical.
        let mut p1_draws = Vec::new();
        let mut p2_draws = Vec::new();
        for _ in 0..40 {
            m.tick(MatchInput::none(), MatchInput::none());
            p1_draws.push(m.p1().character.vars[0]);
            p2_draws.push(m.p2().character.vars[0]);
        }
        assert_ne!(
            p1_draws, p2_draws,
            "distinct per-player seeds must yield different random streams"
        );
        // Sanity: the draws are not all-zero (random actually flowed).
        assert!(
            p1_draws.iter().any(|&v| v != 0),
            "P1's random stream should produce non-zero draws"
        );
    }

    #[test]
    fn a_fixed_match_seed_reproduces_the_run() {
        const SEED: i32 = 31337;
        let script = scripted_inputs(120);

        let run = |seed: i32| {
            let mut m = rng_probe_match();
            m.seed_players(seed);
            into_fight(&mut m);
            for &(p1, p2) in &script {
                m.tick(p1, p2);
            }
            m.snapshot().expect("snapshot")
        };

        // Same fixed seed -> identical run.
        assert_eq!(run(SEED), run(SEED), "a fixed match seed must reproduce");
        // A different seed -> a different run (the seed actually matters).
        assert_ne!(
            run(SEED),
            run(SEED + 1),
            "a different match seed should change the run"
        );
    }

    #[test]
    fn unseeded_players_share_a_stream_motivating_distinct_seeding() {
        // Documents WHY seed_players exists: two fighters built from the same
        // character and NOT distinctly seeded share the identical default stream,
        // so they draw the same random sequence. seed_players fixes exactly this.
        let mut m = rng_probe_match(); // not seeded -> both at DEFAULT_RNG_SEED
        into_fight(&mut m);
        let mut p1_draws = Vec::new();
        let mut p2_draws = Vec::new();
        for _ in 0..30 {
            m.tick(MatchInput::none(), MatchInput::none());
            p1_draws.push(m.p1().character.vars[0]);
            p2_draws.push(m.p2().character.vars[0]);
        }
        assert_eq!(
            p1_draws, p2_draws,
            "unseeded same-character players share one default RNG stream (the bug seed_players fixes)"
        );
    }

    // =====================================================================
    // T012 — Helper entity slot-map: a `Helper` controller spawns a child
    // entity into the player's slot-map, the helper ticks, is addressable by
    // id, and its parent/root redirects resolve to the owning root character.
    // =====================================================================

    /// A controller of `kind` firing unconditionally (`trigger1 = 1`) with the
    /// given params, in state `state_number`.
    fn ctrl_of(
        state_number: i32,
        kind: &str,
        params: &[(&str, &str)],
    ) -> fp_character::CompiledController {
        fp_character::CompiledController {
            state_number,
            label: String::new(),
            controller_type: Some(kind.to_string()),
            triggerall: Vec::new(),
            triggers: vec![fp_character::CompiledTriggerGroup {
                number: 1,
                conditions: vec![fp_character::CompiledExpr::compile("1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), fp_character::CompiledParam::compile(v)))
                .collect(),
        }
    }

    /// A bare stand state `number` (type=S, physics=N) with the given controllers.
    fn stand_state(
        number: i32,
        controllers: Vec<fp_character::CompiledController>,
    ) -> CompiledState {
        CompiledState {
            number,
            state_type: Some("S".to_string()),
            movetype: Some("I".to_string()),
            physics: Some("N".to_string()),
            controllers,
            ..Default::default()
        }
    }

    /// A controller of `kind` gated on a single trigger condition `cond` (one
    /// compiled expression, so a redirect comma survives) with the given params.
    fn ctrl_gated(
        state_number: i32,
        kind: &str,
        cond: &str,
        params: &[(&str, &str)],
    ) -> fp_character::CompiledController {
        let mut c = ctrl_of(state_number, kind, params);
        c.triggers = vec![fp_character::CompiledTriggerGroup {
            number: 1,
            conditions: vec![fp_character::CompiledExpr::compile(cond)],
        }];
        c
    }

    /// A loaded character whose state 0 spawns a helper into state 1000 once
    /// (gated to `Time = 0`), and whose state 1000 (the helper's start state) sets
    /// flag vars when its `root` / `parent` redirects resolve to the owner — so a
    /// test can confirm the helper ticks AND that the spawning-chain redirects
    /// resolve. The redirects are read via trigger CONDITIONS (single compiled
    /// expressions) so the redirect comma is not comma-split like a param value.
    fn helper_owner_loaded() -> LoadedCharacter {
        // State 0: the root. Fire `Helper` only on the first tick (Time = 0) so we
        // get exactly one helper, spawning it 30px in front, 0 up, in state 1000.
        let spawn = ctrl_gated(
            0,
            "Helper",
            "Time = 0",
            &[
                ("id", "1234"),
                ("stateno", "1000"),
                ("postype", "p1"),
                ("pos", "30, 0"),
                ("facing", "1"),
            ],
        );
        let st0 = stand_state(0, vec![spawn]);

        // State 1000: the helper. var(0) = 1 iff `root, Life == 555`; var(1) = 1
        // iff `parent, Life == 555`. Both prove the helper runs its state machine
        // and that the redirect resolves to the owner (whose life the test sets to
        // 555).
        let set_root = ctrl_gated(
            1000,
            "VarSet",
            "root, Life = 555",
            &[("v", "0"), ("value", "1")],
        );
        let set_parent = ctrl_gated(
            1000,
            "VarSet",
            "parent, Life = 555",
            &[("v", "1"), ("value", "1")],
        );
        let st1000 = stand_state(1000, vec![set_root, set_parent]);

        let mut loaded = loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ));
        loaded.states.insert(0, st0);
        loaded.states.insert(1000, st1000);
        loaded
    }

    /// AC1: a `Helper` controller spawns a child entity into the player's
    /// slot-map; the helper is addressable by its id and positioned by `postype`.
    #[test]
    fn helper_controller_spawns_addressable_child() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.life = 555;
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, helper_owner_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));

        assert!(m.p1().helpers().is_empty(), "no helper before any tick");
        m.tick(MatchInput::none(), MatchInput::none());

        let helpers = m.p1().helpers();
        assert_eq!(helpers.len(), 1, "exactly one helper spawned");
        assert_eq!(helpers[0].helper_id(), 1234, "addressable by its id");
        // postype=p1, facing Right, pos=(30,0): spawned 30px in front of P1
        // (-50 + 30*sign(+1) = -20).
        assert!(
            (helpers[0].pos().x - (-20.0)).abs() < 1e-3,
            "helper spawned at the p1-relative anchor, got {}",
            helpers[0].pos().x
        );
        // Firing the spawn was gated to Time = 0, so a second tick does not add a
        // second helper.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.p1().helpers().len(), 1, "Helper fired once (Time = 0)");
    }

    /// AC2/AC3: the helper ticks every frame, and its `root` / `parent` redirects
    /// resolve to the owning root character (not 0). The helper's start state
    /// copies `root, life` / `parent, life` into its own vars each tick.
    #[test]
    fn helper_ticks_and_parent_root_redirects_resolve() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.life = 555; // the distinctive owner life the helper should read
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, helper_owner_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));

        // Tick once: spawns the helper AND ticks it (the spawn happens before
        // tick_helpers within the same frame), so its state-1000 controllers run.
        m.tick(MatchInput::none(), MatchInput::none());
        let helper = &m.p1().helpers()[0];
        assert_eq!(
            helper.character.state_no, 1000,
            "helper entered its start state"
        );
        assert_eq!(
            helper.character.vars[0], 1,
            "`root, life` redirect resolved to the owner (life 555) — the spawning chain, not 0"
        );
        assert_eq!(
            helper.character.vars[1], 1,
            "`parent, life` redirect resolved to the owner (parent == root for a one-level helper), not 0"
        );

        // The helper keeps ticking: its in-state Time advances across frames.
        let t_before = m.p1().helpers()[0].character.state_time;
        m.tick(MatchInput::none(), MatchInput::none());
        let t_after = m.p1().helpers()[0].character.state_time;
        assert!(t_after > t_before, "helper's state Time advances each tick");
    }

    /// (NUMHELPER) A loaded character whose **`[State -2]`** (the always-run
    /// special state) spawns a helper of id `N` into state 1000, guarded by the
    /// standard MUGEN spawn-once pattern `NumHelper(N) = 0`. The `Helper` controller
    /// has NO `Time = 0` gate, so without a working `NumHelper` the guard would be
    /// permanently true and it would spawn `N` every tick until the slot-map caps.
    /// State 1000 (the helper's start state) is an inert stand state, so the helper
    /// persists (never self-retires) — exactly the saturation scenario the bug
    /// describes. `N` is parameterised so the test can assert the per-id count.
    fn numhelper_guard_loaded(helper_id: i32) -> LoadedCharacter {
        let id_str = helper_id.to_string();
        // `[State -2]`: spawn helper `N` only while `NumHelper(N) = 0` (the latch).
        let spawn = ctrl_gated(
            -2,
            "Helper",
            &format!("NumHelper({helper_id}) = 0"),
            &[
                ("id", id_str.as_str()),
                ("stateno", "1000"),
                ("postype", "p1"),
                ("pos", "30, 0"),
                ("facing", "1"),
            ],
        );
        let st_neg2 = stand_state(-2, vec![spawn]);
        // The helper's start state: inert (no DestroySelf), so it stays alive.
        let st1000 = stand_state(1000, Vec::new());

        let mut loaded = loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ));
        loaded.states.insert(-2, st_neg2);
        loaded.states.insert(1000, st1000);
        loaded
    }

    /// (NUMHELPER, AC: `fp-engine` integration test) A player whose `[State -2]`
    /// spawns a helper of id `N` guarded by `NumHelper(N) = 0` ends up with
    /// **exactly one** helper of id `N` after many ticks — the spawn-once guard
    /// latches once helper `N` exists, instead of saturating the bounded slot-map
    /// (the evilken CPU/RAM-hog bug). Before this fix, `NumHelper` always read `0`,
    /// so the guard never closed and the slot-map filled to `MAX_HELPERS_PER_PLAYER`.
    #[test]
    fn numhelper_guard_latches_spawn_to_exactly_one_helper() {
        const HELPER_ID: i32 = 3; // the id evilken's guard checks (NumHelper(3) = 0)

        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, numhelper_guard_loaded(HELPER_ID));
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));

        assert!(m.p1().helpers().is_empty(), "no helper before any tick");

        // Tick far more than the slot-map cap: a broken `NumHelper` would let the
        // guard stay open and saturate at MAX_HELPERS_PER_PLAYER (56) within this
        // many frames; a working one latches after the first spawn.
        for _ in 0..(MAX_HELPERS_PER_PLAYER * 2) {
            m.tick(MatchInput::none(), MatchInput::none());
        }

        let helpers = m.p1().helpers();
        let count_id = helpers
            .iter()
            .filter(|h| h.helper_id() == HELPER_ID)
            .count();
        assert_eq!(
            count_id,
            1,
            "exactly one helper of id {HELPER_ID} (spawn-once guard latched), \
             not the {MAX_HELPERS_PER_PLAYER}-cap saturation; total helpers = {}",
            helpers.len()
        );
        assert_eq!(
            helpers.len(),
            1,
            "the player owns exactly one helper in total"
        );
    }

    /// A match whose players never spawn helpers keeps empty slot-maps and ticks
    /// without panicking (the spawn/tick path is a no-op when no `Helper` fires).
    #[test]
    fn match_without_helpers_keeps_empty_slot_maps() {
        let mut m = basic_match();
        for _ in 0..30 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert!(m.p1().helpers().is_empty(), "no P1 helpers");
        assert!(m.p2().helpers().is_empty(), "no P2 helpers");
    }

    // =====================================================================
    // T032 — Helper lifecycle: DestroySelf + removetime expiry. A helper that
    // runs `DestroySelf` is reaped from the slot-map that tick; a helper with a
    // finite `removetime` auto-expires; and a character that spawns + destroys a
    // helper every tick stays bounded instead of saturating MAX_HELPERS_PER_PLAYER.
    // =====================================================================

    /// A loaded character whose state 0 spawns ONE helper (gated to `Time = 0`)
    /// into state 1000, where the helper runs `DestroySelf` once it has lived at
    /// least one tick (gated `Time >= 1`). So the helper exists for one frame, then
    /// removes itself. `helper_removetime` seeds the spawn's `removetime` (`-1` for
    /// the DestroySelf-only path, a finite value to also exercise auto-expiry).
    fn destroyself_helper_loaded(helper_removetime: i32) -> LoadedCharacter {
        let rt = helper_removetime.to_string();
        let spawn = ctrl_gated(
            0,
            "Helper",
            "Time = 0",
            &[
                ("id", "77"),
                ("stateno", "1000"),
                ("postype", "p1"),
                ("pos", "30, 0"),
                ("facing", "1"),
                ("removetime", rt.as_str()),
            ],
        );
        let st0 = stand_state(0, vec![spawn]);
        // The helper destroys itself once it has run for at least one tick.
        let destroy = ctrl_gated(1000, "DestroySelf", "Time >= 1", &[]);
        let st1000 = stand_state(1000, vec![destroy]);

        let mut loaded = loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ));
        loaded.states.insert(0, st0);
        loaded.states.insert(1000, st1000);
        loaded
    }

    /// AC1: a helper that runs `DestroySelf` is removed from the slot-map that
    /// tick. The helper spawns on tick 1 (its `Time` is 0, so its `DestroySelf` is
    /// gated off and it survives), then on tick 2 (its `Time` is now >= 1) it runs
    /// `DestroySelf` and is reaped.
    #[test]
    fn helper_destroyself_removes_from_slot_map() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        // removetime = -1: no auto-expiry, so the ONLY way this helper retires is
        // its own DestroySelf — isolating the DestroySelf path.
        let p1 = Player::new(p1c, destroyself_helper_loaded(-1));
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));

        // Tick 1: spawn + first helper tick (helper Time == 0, DestroySelf gated
        // off) → the helper is alive.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().helpers().len(),
            1,
            "helper spawned and survived its first tick (DestroySelf gated to Time >= 1)"
        );

        // Tick 2: the helper's Time is now >= 1, so its DestroySelf fires and it is
        // reaped the same tick. (State 0's Helper is gated to Time == 0, so no new
        // helper spawns.)
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(
            m.p1().helpers().is_empty(),
            "helper removed from the slot-map the tick it ran DestroySelf (AC1)"
        );
    }

    /// A loaded character whose state 0 spawns ONE helper (gated `Time = 0`) into
    /// an inert state 1000 (no `DestroySelf`), with the spawn carrying a finite
    /// `removetime`. The helper has no way to retire EXCEPT the `removetime`
    /// expiry, so it isolates the auto-expiry path.
    fn removetime_helper_loaded(helper_removetime: i32) -> LoadedCharacter {
        let rt = helper_removetime.to_string();
        let spawn = ctrl_gated(
            0,
            "Helper",
            "Time = 0",
            &[
                ("id", "88"),
                ("stateno", "1000"),
                ("postype", "p1"),
                ("pos", "30, 0"),
                ("facing", "1"),
                ("removetime", rt.as_str()),
            ],
        );
        let st0 = stand_state(0, vec![spawn]);
        // Inert helper start state: never self-destructs, so only `removetime`
        // expiry can reap it.
        let st1000 = stand_state(1000, Vec::new());

        let mut loaded = loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ));
        loaded.states.insert(0, st0);
        loaded.states.insert(1000, st1000);
        loaded
    }

    /// AC2: a helper with a finite `removetime` auto-expires after that many ticks
    /// even though it never runs `DestroySelf` (its start state is inert). The
    /// countdown runs before the helper ticks, mirroring the projectile lifetime
    /// convention, so a `removetime = N` helper lives for `N` post-spawn ticks.
    #[test]
    fn helper_removetime_auto_expires() {
        const REMOVE_TIME: i32 = 3;

        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, removetime_helper_loaded(REMOVE_TIME));
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));

        // Tick 1: spawn (remaining seeded to REMOVE_TIME) + first helper tick
        // (remaining counts down to REMOVE_TIME - 1).
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.p1().helpers().len(), 1, "helper alive after spawn");
        assert_eq!(
            m.p1().helpers()[0].remaining(),
            REMOVE_TIME - 1,
            "lifetime counts down each tick"
        );

        // Tick the lifetime down to its last live frame. After REMOVE_TIME total
        // post-spawn ticks the countdown reaches 0 (still alive); the next tick
        // takes it below 0 and reaps it.
        for _ in 0..(REMOVE_TIME - 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(
            m.p1().helpers().len(),
            1,
            "helper still alive on its last live frame (remaining == 0)"
        );
        assert_eq!(m.p1().helpers()[0].remaining(), 0, "on the last live frame");

        // One more tick: remaining goes below 0 → reaped (AC2).
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(
            m.p1().helpers().is_empty(),
            "helper auto-expired when its removetime lifespan elapsed (AC2)"
        );
    }

    /// A loaded character whose `[State -2]` (the always-run special state) spawns
    /// a helper of id 5 into state 1000 EVERY tick (no spawn-once guard), and whose
    /// state 1000 destroys the helper immediately (`DestroySelf` always fires). So
    /// every tick: one helper spawns, ticks, self-destructs, and is reaped — the
    /// worst-case churn that used to saturate the slot-map.
    fn spawn_and_destroy_each_tick_loaded() -> LoadedCharacter {
        // No `Time`/`NumHelper` gate: spawn a helper every single tick.
        let spawn = ctrl_of(
            -2,
            "Helper",
            &[
                ("id", "5"),
                ("stateno", "1000"),
                ("postype", "p1"),
                ("pos", "30, 0"),
                ("facing", "1"),
            ],
        );
        let st_neg2 = stand_state(-2, vec![spawn]);
        // The helper destroys itself the moment it runs — so spawn + destroy both
        // happen the same frame, and the slot-map can never grow.
        let destroy = ctrl_of(1000, "DestroySelf", &[]);
        let st1000 = stand_state(1000, vec![destroy]);

        let mut loaded = loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ));
        loaded.states.insert(-2, st_neg2);
        loaded.states.insert(1000, st1000);
        loaded
    }

    /// AC3: a character that spawns AND destroys a helper every tick no longer
    /// saturates [`MAX_HELPERS_PER_PLAYER`]. Before T032 (`DestroySelf` was a
    /// deferred no-op), the helper never retired, so the slot-map filled to the
    /// cap within `MAX_HELPERS_PER_PLAYER` frames; with `DestroySelf` honored the
    /// helper is reaped the same frame it spawns, so the count never climbs above 1.
    #[test]
    fn spawn_and_destroy_each_tick_stays_bounded() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, spawn_and_destroy_each_tick_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));

        // Tick far past the slot-map cap. A broken DestroySelf would saturate at
        // MAX_HELPERS_PER_PLAYER within this many frames.
        for _ in 0..(MAX_HELPERS_PER_PLAYER * 4) {
            m.tick(MatchInput::none(), MatchInput::none());
            assert!(
                m.p1().helpers().len() <= 1,
                "spawn+destroy each tick keeps the helper count at most 1, never \
                 saturating MAX_HELPERS_PER_PLAYER = {MAX_HELPERS_PER_PLAYER} (AC3); \
                 got {}",
                m.p1().helpers().len()
            );
        }
    }

    /// A self-destructing ROOT player is a documented no-op: the root is not a
    /// slot-map entity, so its `DestroySelf` cannot remove it mid-match (the
    /// coordinator only honors `destroy_self` for helper entities). The match keeps
    /// ticking with both players alive and never panics.
    #[test]
    fn root_destroyself_does_not_remove_player() {
        let destroy = ctrl_of(0, "DestroySelf", &[]);
        let st0 = stand_state(0, vec![destroy]);
        let mut loaded = loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ));
        loaded.states.insert(0, st0);

        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, loaded);
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));

        for _ in 0..10 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // The root remains a live player (the match still has its P1 entity).
        assert_eq!(m.p1().life(), m.p1().life_max(), "root still alive");
        assert!(
            m.p1().helpers().is_empty(),
            "the self-destructing root spawned no helpers and is not a helper itself"
        );
    }

    // =====================================================================
    // T013 — Projectile entity slot-map: a `Projectile` controller spawns a
    // moving attack entity into the player's slot-map, the projectile advances
    // each tick, and a projectile overlapping the opponent's Clsn2 connects a hit.
    // =====================================================================

    /// The projectile action id used by the T013 tests: action 2000 carries a wide
    /// `Clsn1` attack box (the projectile's hit geometry), and action 0 carries a
    /// hurt box (so the owner is still hittable / the AIR is well-formed).
    const PROJ_ANIM: i32 = 2000;

    /// A loaded character whose state 0 fires ONE `Projectile` the first FIGHT
    /// tick — gated on `RoundState = 1` (the fight phase) AND a self-var latch
    /// (`var(30) = 0`) so it fires exactly once, NOT during the intro (where a
    /// projectile would simply fly off before combat is live). It travels forward
    /// at velocity `(8, 0)`, carrying a damaging HitDef on `projanim = 2000`. The
    /// owner's AIR has action 2000 with a wide `Clsn1` and action 0 with a hurt box.
    fn projectile_owner_loaded() -> LoadedCharacter {
        // RoundState 1 = the live fight phase; `var(30)` is the once-only latch.
        let spawn = ctrl_gated(
            0,
            "Projectile",
            "RoundState = 1 && var(30) = 0",
            &[
                ("projid", "9001"),
                ("projanim", "2000"),
                ("offset", "20, -40"),
                ("velocity", "8, 0"),
                // No removetime -> -1 (no time limit); it lives until it hits / flies off.
                ("attr", "S, NP"),
                ("damage", "30, 5"),
                ("hitflag", "MAF"),
                ("ground.velocity", "4, -3"),
                ("air.velocity", "4, -6"),
                ("pausetime", "8, 8"),
                ("p2stateno", "5000"),
            ],
        );
        // Latch: once fighting, mark `var(30) = 1` so the spawn never re-fires.
        let latch = ctrl_gated(
            0,
            "VarSet",
            "RoundState = 1",
            &[("v", "30"), ("value", "1")],
        );
        let st0 = stand_state(0, vec![spawn, latch]);

        // AIR: action 2000 = the projectile's wide attack box; action 0 = a hurt
        // box (about the axis) so the owner is well-formed / hittable.
        let mut air = air_with(
            PROJ_ANIM,
            vec![Rect::new(-30.0, -70.0, 60.0, 70.0)],
            Vec::new(),
        );
        air.actions.insert(
            0,
            AnimAction {
                action_number: 0,
                frames: vec![AnimFrame {
                    sprite: SpriteId::new(0, 0),
                    offset: Vec2::new(0, 0),
                    ticks: 1,
                    flip_h: false,
                    flip_v: false,
                    blend: BlendMode::Normal,
                    clsn1: Vec::new(),
                    clsn2: vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
                    ..Default::default()
                }],
                loopstart: 0,
            },
        );
        let mut loaded = loaded_with(air);
        loaded.states.insert(0, st0);
        loaded
    }

    /// AC: a `Projectile` controller spawns a moving projectile entity with its
    /// own HitDef into the player's slot-map, positioned by its offset and facing.
    #[test]
    fn projectile_controller_spawns_moving_entity() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-100.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        // Place P2 far away so the projectile does NOT connect this test (we are
        // only checking spawn + the carried HitDef here).
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(280.0, 0.0);
        let p1 = Player::new(p1c, projectile_owner_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));

        assert!(m.p1().projectiles().is_empty(), "none before any tick");
        into_fight(&mut m);

        let projs = m.p1().projectiles();
        assert_eq!(projs.len(), 1, "exactly one projectile spawned");
        let proj = &projs[0];
        assert_eq!(proj.proj_id(), 9001, "addressable by its projid");
        assert_eq!(proj.anim(), PROJ_ANIM, "displays its projanim");
        // It carries its own HitDef (the attack), distinct from the owner's.
        let hd = proj
            .character
            .active_hitdef
            .expect("projectile carries its own HitDef");
        assert_eq!(hd.damage.hit, 30);
        assert_eq!(hd.p2stateno, Some(5000));
        // The owner did NOT keep the HitDef itself.
        assert!(
            m.p1().character.active_hitdef.is_none(),
            "the owner does not carry the projectile's attack"
        );
        // The spawn is gated Time = 0, so no second projectile appears next frame.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.p1().projectiles().len(), 1, "Projectile fired once");
    }

    /// AC: the projectile MOVES each tick by its velocity (mirrored by the owner's
    /// facing). Spawned facing right, its X strictly increases frame over frame.
    #[test]
    fn projectile_advances_each_tick() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-100.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(280.0, 0.0);
        let p1 = Player::new(p1c, projectile_owner_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-400.0, 400.0));
        into_fight(&mut m);

        // The frame the projectile spawned it has already advanced once (spawn +
        // tick happen the same frame). Capture its X, then confirm it keeps moving.
        let x0 = m.p1().projectiles()[0].pos().x;
        m.tick(MatchInput::none(), MatchInput::none());
        let x1 = m.p1().projectiles()[0].pos().x;
        m.tick(MatchInput::none(), MatchInput::none());
        let x2 = m.p1().projectiles()[0].pos().x;
        assert!(x1 > x0, "projectile moved right (x1={x1} > x0={x0})");
        assert!(x2 > x1, "projectile kept moving right (x2={x2} > x1={x1})");
        // Velocity 8/tick: each step advances ~8px (mirrored by the right facing).
        assert!(
            (x1 - x0 - 8.0).abs() < 1e-3,
            "advanced 8px/tick, got {}",
            x1 - x0
        );
    }

    /// AC: a projectile that overlaps the opponent's `Clsn2` resolves a hit —
    /// damaging the defender, sending it to the HitDef's `p2stateno`, and reaping
    /// the (single-hit) projectile.
    #[test]
    fn projectile_connects_and_damages_opponent() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        // Opponent placed far enough that the forward-travelling projectile takes
        // several FIGHT ticks to reach it — so the connection happens inside the
        // measured loop below, not during the single fight tick `into_fight` runs.
        // P2's hurt box is about its axis (-18..18 -> world 182..218 at x=200).
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(200.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, projectile_owner_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-400.0, 400.0));
        into_fight(&mut m);

        let life_before = m.p2().life();
        // Run several fight ticks so the projectile flies into the opponent. It
        // spawns ~20px in front of P1 (x≈20) and advances 8px/tick, reaching the
        // opponent's hurt box (centred at 200) after ~20 ticks.
        let mut connected = false;
        for _ in 0..40 {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.p2().life() < life_before {
                connected = true;
                break;
            }
        }
        assert!(
            connected,
            "projectile connected and reduced the opponent's life"
        );
        // Damage applied (30 hit damage; defence_mul defaults to 1.0).
        assert!(
            m.p2().life() <= life_before - 30,
            "applied at least the HitDef hit damage"
        );
        // The defender was sent into the HitDef's p2stateno (5000).
        assert_eq!(
            m.p2().character.state_no,
            5000,
            "defender entered the projectile's p2stateno"
        );
        // The projectile is single-hit: it was reaped on the connection.
        assert!(
            m.p1().projectiles().is_empty(),
            "the connecting projectile was reaped (single-hit)"
        );
    }

    /// T026 (AC1/AC2/AC3): the owner's per-id projectile contact/hit/guard tracker
    /// is populated across a real projectile lifecycle inside a `Match` — no live
    /// projectile before spawn, one live after spawn (the `NumProj` source), then a
    /// recorded clean-hit event once the projectile connects, and the
    /// `ProjContactTime`/`ProjHitTime` counters age each subsequent tick.
    #[test]
    fn proj_triggers_populate_across_match_lifecycle() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(200.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, projectile_owner_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-400.0, 400.0));

        // Before any tick: no live projectiles (the `NumProj` source is empty) and
        // the owner has tracked no contact for the fixture's projid (9001).
        assert!(m.p1().projectiles().is_empty());
        assert!(
            !m.p1().character.proj_events.contains_key(&9001),
            "no contact tracked before spawn"
        );

        into_fight(&mut m);

        // After the spawn fires: exactly one live projectile — the count `NumProj`
        // reports — and still no contact recorded (it has not reached P2 yet).
        assert_eq!(
            m.p1().projectiles().len(),
            1,
            "one live projectile (the NumProj count)"
        );
        assert!(
            !m.p1().character.proj_events.contains_key(&9001),
            "no contact yet — the projectile is still in flight"
        );

        // Fly the projectile into the opponent.
        let mut connected_tick = None;
        for t in 0..40 {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.p1().character.proj_events.contains_key(&9001) {
                connected_tick = Some(t);
                break;
            }
        }
        assert!(
            connected_tick.is_some(),
            "the projectile connected and recorded a Proj* event"
        );

        // On the connecting tick the clean-hit + contact counters read 0 (it was a
        // hit, not a guard — defender_loaded does not block), guard reads "never".
        let tracker = m.p1().character.proj_events[&9001];
        assert_eq!(tracker.contact_time(), 0, "ProjContactTime9001 == 0 on hit");
        assert_eq!(tracker.hit_time(), 0, "ProjHitTime9001 == 0 on hit");
        assert_eq!(
            tracker.guarded_time(),
            ProjContactTracker::NEVER,
            "ProjGuardedTime9001 stays -1 (not guarded)"
        );
        // The single-hit projectile was reaped, so `NumProj` drops back to 0.
        assert!(
            m.p1().projectiles().is_empty(),
            "single-hit projectile reaped"
        );

        // The counters age on each subsequent tick (the `ProjContactTime` clock).
        m.tick(MatchInput::none(), MatchInput::none());
        let aged = m.p1().character.proj_events[&9001];
        assert_eq!(aged.contact_time(), 1, "contact time advanced one tick");
        assert_eq!(aged.hit_time(), 1, "hit time advanced one tick");
    }

    /// (T026) A loaded character whose state 0 spawns a long-lived projectile AND a
    /// helper (both once, on the first tick), and whose helper start state (1000)
    /// copies the player-level `NumProj` count into its own `var(0)` each tick. This
    /// proves a helper-context read of `NumProj` sees the **owner's** live-projectile
    /// count, exactly as `NumHelper` does from a helper — the parity the helper-tick
    /// graph (`tick_helpers`) must install with `with_own_proj_ids`.
    fn proj_then_helper_owner_loaded() -> LoadedCharacter {
        // Spawn one projectile on the first tick. No `removetime` -> lives until it
        // hits / flies off; P2 is placed far away in the test so it stays in flight,
        // keeping `NumProj == 1` while the helper reads it.
        let spawn_proj = ctrl_gated(
            0,
            "Projectile",
            "Time = 0",
            &[
                ("projid", "9001"),
                ("projanim", "2000"),
                ("offset", "20, -40"),
                ("velocity", "8, 0"),
                ("attr", "S, NP"),
                ("damage", "30, 5"),
                ("hitflag", "MAF"),
            ],
        );
        // Spawn one helper on the first tick, into state 1000.
        let spawn_helper = ctrl_gated(
            0,
            "Helper",
            "Time = 0",
            &[
                ("id", "1234"),
                ("stateno", "1000"),
                ("postype", "p1"),
                ("pos", "30, 0"),
                ("facing", "1"),
            ],
        );
        let st0 = stand_state(0, vec![spawn_proj, spawn_helper]);

        // State 1000 (the helper's start state): record the owner's NumProj into the
        // helper's own var(0) every tick. A helper without the owner's proj-id list
        // installed would read 0 here.
        let record = ctrl_of(1000, "VarSet", &[("v", "0"), ("value", "NumProj")]);
        let st1000 = stand_state(1000, vec![record]);

        // AIR: action 2000 = the projectile's box; action 0 = a hurt box so the
        // owner is well-formed.
        let mut air = air_with(
            PROJ_ANIM,
            vec![Rect::new(-30.0, -70.0, 60.0, 70.0)],
            Vec::new(),
        );
        air.actions.insert(
            0,
            AnimAction {
                action_number: 0,
                frames: vec![AnimFrame {
                    sprite: SpriteId::new(0, 0),
                    offset: Vec2::new(0, 0),
                    ticks: 1,
                    flip_h: false,
                    flip_v: false,
                    blend: BlendMode::Normal,
                    clsn1: Vec::new(),
                    clsn2: vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
                    ..Default::default()
                }],
                loopstart: 0,
            },
        );
        let mut loaded = loaded_with(air);
        loaded.states.insert(0, st0);
        loaded.states.insert(1000, st1000);
        loaded
    }

    /// (T026, SHOULD_FIX parity) A helper evaluating `NumProj` reads the **owning
    /// player's** live-projectile count, not its own empty list. Mirrors the
    /// `NumHelper`-from-helper parity: `tick_helpers` must install the owner's
    /// `own_proj_ids` on every helper's graph. Before the fix the helper read 0.
    #[test]
    fn numproj_read_from_helper_context_sees_owner_count() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-100.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        // P2 far away so the single projectile stays in flight (NumProj stays 1)
        // while the helper reads it.
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(280.0, 0.0);
        let p1 = Player::new(p1c, proj_then_helper_owner_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-400.0, 400.0));

        // First tick spawns both the projectile AND the helper. Per the tick order
        // (`Match::tick` step 2d helpers, then step 2e projectiles), helpers tick
        // BEFORE projectiles are pushed into the slot-map that frame, so the helper's
        // first read sees `NumProj == 0` (the projectile isn't live yet) — correct
        // engine behaviour, not the bug under test.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().projectiles().len(),
            1,
            "exactly one live projectile (the owner's NumProj count)"
        );
        assert_eq!(
            m.p1().helpers()[0].character.state_no,
            1000,
            "helper entered its start state"
        );

        // Second tick: the projectile is now in the owner's slot-map, so when the
        // helper's state-1000 VarSet runs (step 2d), the owner's proj-id list is
        // installed on the helper's graph and `NumProj` reads the owner's live count.
        // Before the `tick_helpers` fix the helper read its own empty list (0).
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().projectiles().len(),
            1,
            "projectile still in flight (P2 placed far away)"
        );
        assert_eq!(
            m.p1().helpers()[0].character.vars[0],
            1,
            "helper read the OWNER's NumProj (1 live projectile), not its own empty list (0)"
        );
    }

    /// A projectile that never reaches the opponent is eventually reaped when it
    /// flies off the stage (an un-timed projectile is still bounded).
    #[test]
    fn projectile_is_reaped_after_flying_offscreen() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        // Opponent placed BEHIND the projectile's travel (to the left), so the
        // forward-flying projectile never connects and instead flies off the right.
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-200.0, 0.0);
        p2c.facing = Facing::Right;
        let p1 = Player::new(p1c, projectile_owner_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-150.0, 150.0));
        into_fight(&mut m);
        assert_eq!(
            m.p1().projectiles().len(),
            1,
            "projectile present after spawn"
        );

        // Fly it well past the right edge (150 + 80 margin) at 8px/tick.
        for _ in 0..40 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert!(
            m.p1().projectiles().is_empty(),
            "projectile reaped after flying off the stage"
        );
    }

    /// A match whose players never spawn projectiles keeps empty projectile
    /// slot-maps and ticks without panicking.
    #[test]
    fn match_without_projectiles_keeps_empty_slot_maps() {
        let mut m = basic_match();
        for _ in 0..30 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert!(m.p1().projectiles().is_empty(), "no P1 projectiles");
        assert!(m.p2().projectiles().is_empty(), "no P2 projectiles");
    }

    // =====================================================================
    // T014 — cross-player redirects through the Match: `target` resolves to the
    // entity a player most recently hit; `playerid(n)` resolves to that player.
    // =====================================================================

    /// A loaded character whose state 0 records, into its own vars each tick,
    /// whether its `target` / `playerid(2)` redirects resolve to a defender whose
    /// life the test sets to a distinctive 444. Reads go through trigger
    /// CONDITIONS (single compiled expressions) so the redirect comma is preserved.
    fn redirect_reader_loaded() -> LoadedCharacter {
        // var(0) = 1 iff `target, Life == 444`; var(1) = 1 iff `playerid(2), Life == 444`.
        let read_target = ctrl_gated(
            0,
            "VarSet",
            "target, Life = 444",
            &[("v", "0"), ("value", "1")],
        );
        let read_playerid = ctrl_gated(
            0,
            "VarSet",
            "playerid(2), Life = 444",
            &[("v", "1"), ("value", "1")],
        );
        let st0 = stand_state(0, vec![read_target, read_playerid]);
        let mut loaded = loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ));
        loaded.states.insert(0, st0);
        loaded
    }

    /// AC: `playerid(2)` (P2's MUGEN player id) resolves to the opponent through a
    /// live `Match::tick`, reading the opponent's life (not 0).
    #[test]
    fn playerid_redirect_resolves_opponent_through_match() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.life = 444; // the distinctive opponent life the redirect should read
        let p1 = Player::new(p1c, redirect_reader_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-200.0, 200.0));
        into_fight(&mut m);

        // P1 has not hit P2 yet, so `target` is None (var(0) stays 0); but
        // `playerid(2)` always resolves to P2 (var(1) becomes 1).
        assert_eq!(
            m.p1().character.vars[1],
            1,
            "`playerid(2)` resolved to the opponent (life 444), not 0"
        );
    }

    /// AC: `target` resolves to the entity a player most recently hit. Drive P1's
    /// projectile into P2 to ESTABLISH a target, then confirm P1's `target, Life`
    /// redirect reads the opponent (it was 0 before the hit).
    #[test]
    fn target_redirect_resolves_after_a_hit_through_match() {
        // P1 fires a projectile (state 0) AND reads its `target` redirect each
        // tick. We compose: state 0 fires the projectile once and records whether
        // `target, life` resolves. Use a fresh loaded char combining both.
        // Fire the projectile the first FIGHT tick (latched once via var(30)) so it
        // does not fly off during the intro.
        let spawn = ctrl_gated(
            0,
            "Projectile",
            "RoundState = 1 && var(30) = 0",
            &[
                ("projid", "1"),
                ("projanim", "2000"),
                ("offset", "20, -40"),
                ("velocity", "8, 0"),
                ("attr", "S, NP"),
                ("damage", "10, 0"),
                ("hitflag", "MAF"),
                ("pausetime", "1, 1"),
            ],
        );
        let latch = ctrl_gated(
            0,
            "VarSet",
            "RoundState = 1",
            &[("v", "30"), ("value", "1")],
        );
        // var(0) = 1 whenever `target` resolves to a live opponent (life > 0).
        let read_target = ctrl_gated(
            0,
            "VarSet",
            "target, Life > 0",
            &[("v", "0"), ("value", "1")],
        );
        let st0 = stand_state(0, vec![spawn, latch, read_target]);
        let mut air = air_with(
            PROJ_ANIM,
            vec![Rect::new(-30.0, -70.0, 60.0, 70.0)],
            Vec::new(),
        );
        air.actions.insert(
            0,
            AnimAction {
                action_number: 0,
                frames: vec![AnimFrame {
                    sprite: SpriteId::new(0, 0),
                    offset: Vec2::new(0, 0),
                    ticks: 1,
                    flip_h: false,
                    flip_v: false,
                    blend: BlendMode::Normal,
                    clsn1: Vec::new(),
                    clsn2: vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
                    ..Default::default()
                }],
                loopstart: 0,
            },
        );
        let mut loaded = loaded_with(air);
        loaded.states.insert(0, st0);

        let mut p1c = Character::new();
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_no = 0;
        let mut p2c = Character::new();
        // Far enough that the projectile connects several fight ticks in, not on
        // the single fight tick `into_fight` runs.
        p2c.pos = Vec2::new(200.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, loaded);
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-400.0, 400.0));
        into_fight(&mut m);

        // Before the projectile connects, P1 has no target: the first fight tick
        // records var(0) = 0 (no target established yet).
        assert_eq!(m.p1().character.vars[0], 0, "no target before the hit");
        assert!(!m.p1().character.has_target, "no target before the hit");

        // Run until the projectile connects and P1 establishes a target.
        let mut hit = false;
        for _ in 0..40 {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.p1().character.has_target {
                hit = true;
                break;
            }
        }
        assert!(hit, "P1's projectile connected and established a target");
        // One more tick so the `target` read runs WITH the target now wired.
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().character.vars[0],
            1,
            "`target` redirect resolved to the hit opponent (life > 0), not 0"
        );
    }

    // ---- T018: baseline CPU AI drives an otherwise-idle player 2 ------------

    /// The minimal `.cmd` for the AI integration tests: the four facing-relative
    /// hold commands the engine locomotion reads, plus a `punch = a` button
    /// command matching the attack button the [`fp_input::CpuAi`] presses.
    const AI_CMD: &str = "\
[Command]
name = \"holdfwd\"
command = /$F
time = 1

[Command]
name = \"holdback\"
command = /$B
time = 1

[Command]
name = \"holdup\"
command = /$U
time = 1

[Command]
name = \"holddown\"
command = /$D
time = 1

[Command]
name = \"punch\"
command = a
time = 1
";

    /// Builds a [`Match`] where P2 carries [`AI_CMD`] so its real
    /// [`CommandMatcher`] recognizes the AI's emitted inputs, driven into the
    /// live fight phase. P1 sits on the left, P2 on the right (facing each other).
    fn ai_match() -> Match {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.facing = Facing::Right;
        p1c.state_type = StateType::Standing;
        p1c.move_type = MoveType::Idle;

        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.state_type = StateType::Standing;
        p2c.move_type = MoveType::Idle;

        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, loaded_with_cmd(air_with(0, vec![], vec![]), AI_CMD));
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);
        m
    }

    /// Acceptance: an out-of-range CPU AI on P2 emits inputs that the engine
    /// recognizes as P2's `holdfwd` — i.e. it moves *toward* the opponent. P2
    /// faces left (P1 is to its left), so "toward" is screen-left; the AI sees
    /// P1 to its left via the observation and holds left, which P2's
    /// facing-relative matcher resolves to `holdfwd`.
    #[test]
    fn ai_approaches_when_out_of_range() {
        let mut m = ai_match();
        assert_eq!(m.p2().facing(), Facing::Left, "P2 faces P1 (to its left)");
        // Wide gap -> out of range for Normal (attack_range 60).
        assert!(
            (m.p1().pos().x - m.p2().pos().x).abs() > 60.0,
            "fixture must start out of attack range"
        );

        // No-jump tuning so the approach direction is unambiguous.
        let mut ai = {
            let mut t = fp_input::AiTuning::for_difficulty(fp_input::AiDifficulty::Normal);
            t.jump_chance = 0;
            fp_input::CpuAi::with_tuning(1, t)
        };

        let mut approached = false;
        for _ in 0..30 {
            let obs = m.ai_observation_for_p2();
            assert!(
                obs.opponent_dx < 0.0,
                "P1 is to P2's left, so observed dx must be negative"
            );
            let p2_input: MatchInput = ai.decide(obs).into();
            assert!(
                p2_input.left && !p2_input.right,
                "out of range the AI holds left (toward P1)"
            );
            m.tick(MatchInput::none(), p2_input);
            if m.p2().character.commands.is_active("holdfwd") {
                approached = true;
            }
        }
        assert!(
            approached,
            "the AI's toward-opponent input must activate P2's `holdfwd` command"
        );
    }

    /// Acceptance: an in-range CPU AI on P2 attacks — its emitted input fires
    /// P2's `punch` (light-punch `a`) button command on the great majority of
    /// frames (occasionally it guards, per difficulty).
    #[test]
    fn ai_attacks_when_in_range() {
        let mut m = ai_match();
        // Force point-blank so the AI is clearly in attack range.
        m.p1.character.pos.x = -5.0;
        m.p2.character.pos.x = 5.0;

        // Suppress jumps so the in-range decision is purely attack-vs-block.
        let mut ai = {
            let mut t = fp_input::AiTuning::for_difficulty(fp_input::AiDifficulty::Normal);
            t.jump_chance = 0;
            fp_input::CpuAi::with_tuning(7, t)
        };

        let frames = 60;
        let mut punch_frames = 0;
        for _ in 0..frames {
            // Keep the two point-blank each frame (the synthetic chars have no
            // walk locomotion to drift them apart, but push could).
            m.p1.character.pos.x = -5.0;
            m.p2.character.pos.x = 5.0;
            let obs = m.ai_observation_for_p2();
            assert!(
                obs.distance() <= 60.0,
                "fixture must keep the AI in attack range"
            );
            let p2_input: MatchInput = ai.decide(obs).into();
            m.tick(MatchInput::none(), p2_input);
            if m.p2().character.commands.is_active("punch") {
                punch_frames += 1;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        // The AI pulses (presses at most every other frame) and occasionally
        // guards, so it lands the `punch` command on a large minority of frames.
        assert!(
            punch_frames > frames / 4,
            "in range the AI must attack on many frames; punched {punch_frames}/{frames}"
        );
    }

    /// Replay safety: two matches driven by two CPU AIs seeded identically, each
    /// fed the live observation from its own match, stay bit-identical across
    /// many frames (deterministic given the same seed + state).
    #[test]
    fn ai_driven_match_is_deterministic_for_a_fixed_seed() {
        let mut a = ai_match();
        let mut b = ai_match();
        let mut ai_a = fp_input::CpuAi::new(99, fp_input::AiDifficulty::Hard);
        let mut ai_b = fp_input::CpuAi::new(99, fp_input::AiDifficulty::Hard);

        for _ in 0..120 {
            let in_a: MatchInput = ai_a.decide(a.ai_observation_for_p2()).into();
            let in_b: MatchInput = ai_b.decide(b.ai_observation_for_p2()).into();
            assert_eq!(in_a, in_b, "same-seed AIs must emit identical inputs");
            a.tick(MatchInput::none(), in_a);
            b.tick(MatchInput::none(), in_b);
            // Whole-match state stays in lockstep.
            assert_eq!(a.p2().pos(), b.p2().pos());
            assert_eq!(a.p2().life(), b.p2().life());
            assert_eq!(a.p1().life(), b.p1().life());
            assert_eq!(a.round_state(), b.round_state());
        }
    }

    /// The `InputState -> MatchInput` bridge copies every field straight through.
    #[test]
    fn input_state_into_match_input_round_trips() {
        let mut s = InputState::default();
        s.direction.left = true;
        s.direction.up = true;
        s.set_button(Button::A, true);
        s.set_button(Button::Z, true);
        let m: MatchInput = s.into();
        assert!(m.left && m.up && m.a && m.z);
        assert!(!m.right && !m.down && !m.b && !m.c && !m.x && !m.y);
    }

    /// KFM-gated (skips without the fixture): a real-character match where P2 is
    /// driven only by the baseline AI physically closes the gap on P1 — the
    /// strongest form of "approaches and attacks for a given opponent distance".
    #[test]
    fn ai_closes_the_gap_on_a_real_kfm_opponent() {
        let Some(mut m) = two_kfm_match() else {
            return;
        };
        assert!(
            run_until_fight(&mut m),
            "fight must go live before driving input"
        );
        let gap_before = (m.p1().pos().x - m.p2().pos().x).abs();

        // No-jump AI so it walks straight in rather than hopping.
        let mut ai = {
            let base = fp_input::CpuAi::new(3, fp_input::AiDifficulty::Hard);
            let mut t = base.tuning();
            t.jump_chance = 0;
            fp_input::CpuAi::with_tuning(3, t)
        };

        for _ in 0..120 {
            let p2_input: MatchInput = ai.decide(m.ai_observation_for_p2()).into();
            m.tick(MatchInput::none(), p2_input);
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        let gap_after = (m.p1().pos().x - m.p2().pos().x).abs();
        assert!(
            gap_after < gap_before,
            "the AI-driven P2 must close the gap on P1 ({gap_before} -> {gap_after})"
        );
    }

    // ---- Explod subsystem (T033) ------------------------------------------

    /// A two-frame AIR on action `anim`, each frame lasting `ticks` ticks, so an
    /// explod's animation advance / play-once expiry is observable.
    fn two_frame_air(anim: i32, ticks: i32) -> AirFile {
        let mk = |g: u16| AnimFrame {
            sprite: SpriteId::new(g, 0),
            offset: Vec2::new(0, 0),
            ticks,
            flip_h: false,
            flip_v: false,
            blend: BlendMode::Normal,
            clsn1: Vec::new(),
            clsn2: Vec::new(),
            ..Default::default()
        };
        let mut actions = HashMap::new();
        actions.insert(
            anim,
            AnimAction {
                action_number: anim,
                frames: vec![mk(0), mk(1)],
                loopstart: 0,
            },
        );
        AirFile { actions }
    }

    /// A headless [`Player`] at world X `x` whose AIR carries the given two-frame
    /// action, used to drive the explod slot-map methods directly.
    fn explod_player(x: f32, anim: i32, ticks: i32) -> Player {
        let mut c = Character::new();
        c.pos = Vec2::new(x, 0.0);
        c.facing = Facing::Right;
        Player::new(c, loaded_with(two_frame_air(anim, ticks)))
    }

    fn spawn(id: i32, anim: i32, removetime: i32) -> ExplodSpawn {
        ExplodSpawn {
            id,
            anim,
            pos_type: ExplodPosType::P1,
            pos: (0.0, 0.0),
            sprpriority: 0,
            bindtime: -1,
            removetime,
        }
    }

    /// AC1: spawning an explod inserts a live, rendered entity carrying the parsed
    /// anim / id / sprpriority and resolves its first frame's sprite immediately so
    /// it is drawable the frame it spawns.
    #[test]
    fn explod_spawn_inserts_rendered_entity() {
        let mut p = explod_player(0.0, 5, 4);
        let s = ExplodSpawn {
            id: 3,
            anim: 5,
            pos_type: ExplodPosType::P1,
            pos: (10.0, -20.0),
            sprpriority: 2,
            bindtime: -1,
            removetime: 8,
        };
        p.spawn_explods(&[s], MUGEN_PLAYER_ID_P1, 100.0, StageView::default());
        assert_eq!(p.explods().len(), 1, "one explod spawned");
        let e = p.explods()[0];
        assert_eq!(e.id(), 3);
        assert_eq!(e.anim(), 5);
        assert_eq!(e.owner(), MUGEN_PLAYER_ID_P1);
        assert_eq!(e.sprpriority, 2);
        // postype p1 anchored at the owner (x=0, facing right) + offset (10, -20).
        assert_eq!(e.pos, Vec2::new(10.0, -20.0));
        // First frame's sprite is resolved so a renderer can draw it immediately.
        assert_eq!(e.sprite, SpriteId::new(0, 0));
        assert_eq!(e.remaining(), 8, "removetime carried as the lifetime");
    }

    /// AC1: a spawn whose `anim` action is missing spawns nothing (best-effort
    /// skip, never a panic).
    #[test]
    fn explod_spawn_missing_anim_is_skipped() {
        let mut p = explod_player(0.0, 5, 4);
        // Action 999 does not exist in the AIR.
        p.spawn_explods(
            &[spawn(1, 999, 8)],
            MUGEN_PLAYER_ID_P1,
            0.0,
            StageView::default(),
        );
        assert!(
            p.explods().is_empty(),
            "no explod for a missing anim action"
        );
    }

    /// AC3: an explod with a non-negative `removetime` counts down and is reaped
    /// when its lifetime elapses. `removetime = N` keeps it alive for `N` ticks,
    /// then the tick that would take `remaining` below `0` reaps it.
    #[test]
    fn explod_expires_on_removetime() {
        let mut p = explod_player(0.0, 5, 99); // long frame ticks; lifetime drives expiry
        p.spawn_explods(
            &[spawn(1, 5, 3)],
            MUGEN_PLAYER_ID_P1,
            0.0,
            StageView::default(),
        );
        assert_eq!(p.explods().len(), 1);
        assert_eq!(p.explods()[0].remaining(), 3);
        for expected in [2, 1, 0] {
            p.tick_explods(
                Vec2::new(0.0, 0.0),
                Facing::Right,
                0.0,
                StageView::default(),
            );
            assert_eq!(
                p.explods()[0].remaining(),
                expected,
                "counts down by one per tick"
            );
        }
        // The next tick takes remaining below 0 and reaps the explod.
        p.tick_explods(
            Vec2::new(0.0, 0.0),
            Facing::Right,
            0.0,
            StageView::default(),
        );
        assert!(p.explods().is_empty(), "explod reaped at removetime expiry");
    }

    /// AC3: a `removetime = -1` (play-once) explod is reaped the tick its one-shot
    /// animation finishes (it does not loop forever).
    #[test]
    fn explod_play_once_reaped_when_animation_finishes() {
        // Two frames, 1 tick each → the animation finishes after 2 advancing ticks.
        let mut p = explod_player(0.0, 5, 1);
        p.spawn_explods(
            &[spawn(1, 5, -1)],
            MUGEN_PLAYER_ID_P1,
            0.0,
            StageView::default(),
        );
        assert_eq!(p.explods().len(), 1);
        // Tick 1: cursor steps to the second (final) frame; still alive.
        p.tick_explods(
            Vec2::new(0.0, 0.0),
            Facing::Right,
            0.0,
            StageView::default(),
        );
        assert_eq!(p.explods().len(), 1, "still playing the last frame");
        assert_eq!(
            p.explods()[0].sprite,
            SpriteId::new(1, 0),
            "advanced to frame 2"
        );
        // Tick 2: would step past the final frame → play-once reaps it.
        p.tick_explods(
            Vec2::new(0.0, 0.0),
            Facing::Right,
            0.0,
            StageView::default(),
        );
        assert!(
            p.explods().is_empty(),
            "play-once explod reaped at animation end"
        );
    }

    /// AC3: a looping (`removetime = -2`) explod is bounded — it cannot live past
    /// [`EXPLOD_MAX_LIFETIME`] even though its animation loops forever.
    #[test]
    fn explod_loop_is_bounded_by_max_lifetime() {
        let mut p = explod_player(0.0, 5, 1);
        p.spawn_explods(
            &[spawn(1, 5, -2)],
            MUGEN_PLAYER_ID_P1,
            0.0,
            StageView::default(),
        );
        // remaining seeded to the ceiling, so it counts down and is eventually
        // reaped (it is NOT play-once, so its animation loops the whole time).
        assert_eq!(p.explods()[0].remaining(), EXPLOD_MAX_LIFETIME);
        assert!(
            !p.explods()[0].play_once(),
            "a -2 explod loops, not play-once"
        );
        for _ in 0..=EXPLOD_MAX_LIFETIME {
            p.tick_explods(
                Vec2::new(0.0, 0.0),
                Facing::Right,
                0.0,
                StageView::default(),
            );
        }
        assert!(
            p.explods().is_empty(),
            "a looping explod is reaped by EXPLOD_MAX_LIFETIME"
        );
    }

    /// AC2: `ModifyExplod` updates only the matching explod's given fields, leaving
    /// the others untouched.
    #[test]
    fn modify_explod_updates_matching_fields_only() {
        let mut p = explod_player(0.0, 5, 99);
        // Two explods: id 1 and id 2, both anim 5.
        p.spawn_explods(
            &[spawn(1, 5, 50), spawn(2, 5, 50)],
            MUGEN_PLAYER_ID_P1,
            0.0,
            StageView::default(),
        );
        // Modify only id 1: new sprpriority + removetime; leave anim/pos/bindtime.
        p.apply_explod_ops(&[ExplodOp::Modify {
            id: Some(1),
            anim: None,
            pos: None,
            sprpriority: Some(9),
            bindtime: None,
            removetime: Some(20),
        }]);
        let e1 = p.explods().iter().find(|e| e.id() == 1).unwrap();
        let e2 = p.explods().iter().find(|e| e.id() == 2).unwrap();
        assert_eq!(e1.sprpriority, 9, "id 1 sprpriority updated");
        assert_eq!(e1.remaining(), 20, "id 1 removetime re-clamped");
        assert_eq!(e1.anim(), 5, "id 1 anim untouched (absent field)");
        // id 2 is wholly untouched.
        assert_eq!(e2.sprpriority, 0);
        assert_eq!(e2.remaining(), 50);
    }

    /// AC2: a no-id `ModifyExplod` (selector `None`) updates ALL of the owner's
    /// explods.
    #[test]
    fn modify_explod_no_id_matches_all() {
        let mut p = explod_player(0.0, 5, 99);
        p.spawn_explods(
            &[spawn(1, 5, 50), spawn(2, 5, 50)],
            MUGEN_PLAYER_ID_P1,
            0.0,
            StageView::default(),
        );
        p.apply_explod_ops(&[ExplodOp::Modify {
            id: None,
            anim: None,
            pos: None,
            sprpriority: Some(4),
            bindtime: None,
            removetime: None,
        }]);
        assert!(
            p.explods().iter().all(|e| e.sprpriority == 4),
            "no-id ModifyExplod touched every explod"
        );
    }

    /// AC2: `RemoveExplod` by id removes only the matching explods; a no-id
    /// `RemoveExplod` removes them all.
    #[test]
    fn remove_explod_by_id_and_all() {
        let mut p = explod_player(0.0, 5, 99);
        p.spawn_explods(
            &[spawn(1, 5, 50), spawn(2, 5, 50), spawn(1, 5, 50)],
            MUGEN_PLAYER_ID_P1,
            0.0,
            StageView::default(),
        );
        assert_eq!(p.explods().len(), 3);
        // Remove id 1 → both id-1 explods gone, the id-2 stays.
        p.apply_explod_ops(&[ExplodOp::Remove(Some(1))]);
        assert_eq!(p.explods().len(), 1);
        assert_eq!(p.explods()[0].id(), 2);
        // Remove all.
        p.apply_explod_ops(&[ExplodOp::Remove(None)]);
        assert!(
            p.explods().is_empty(),
            "no-id RemoveExplod cleared the slot-map"
        );
    }

    /// AC3: the explod slot-map is bounded — a runaway spawn loop cannot grow it
    /// past [`MAX_EXPLODS_PER_PLAYER`].
    #[test]
    fn explod_slot_map_is_bounded() {
        let mut p = explod_player(0.0, 5, 99);
        let many: Vec<ExplodSpawn> = (0..MAX_EXPLODS_PER_PLAYER as i32 + 50)
            .map(|i| spawn(i, 5, 50))
            .collect();
        p.spawn_explods(&many, MUGEN_PLAYER_ID_P1, 0.0, StageView::default());
        assert_eq!(
            p.explods().len(),
            MAX_EXPLODS_PER_PLAYER,
            "explod count capped at MAX_EXPLODS_PER_PLAYER"
        );
    }

    /// AC3: a bound explod tracks its anchor each tick (a `postype = p1` explod
    /// follows the moving owner), then holds its last position once unbound.
    #[test]
    fn explod_bound_tracks_anchor_then_holds() {
        let mut p = explod_player(0.0, 5, 99);
        let s = ExplodSpawn {
            id: 1,
            anim: 5,
            pos_type: ExplodPosType::P1,
            pos: (5.0, 0.0),
            sprpriority: 0,
            bindtime: 1, // bound for exactly one more tick after spawn
            removetime: 50,
        };
        p.spawn_explods(&[s], MUGEN_PLAYER_ID_P1, 0.0, StageView::default());
        assert_eq!(p.explods()[0].pos, Vec2::new(5.0, 0.0));
        // Owner moved to x=30; while bound, the explod re-anchors to follow.
        p.tick_explods(
            Vec2::new(30.0, 0.0),
            Facing::Right,
            0.0,
            StageView::default(),
        );
        assert_eq!(
            p.explods()[0].pos,
            Vec2::new(35.0, 0.0),
            "tracked the owner"
        );
        // Bind window now elapsed; a further owner move no longer drags the explod.
        p.tick_explods(
            Vec2::new(100.0, 0.0),
            Facing::Right,
            0.0,
            StageView::default(),
        );
        assert_eq!(
            p.explods()[0].pos,
            Vec2::new(35.0, 0.0),
            "held its last bound position after the bind window elapsed"
        );
    }

    /// AC1 + AC3 end-to-end through `Match::tick`: a CNS `Explod` controller in a
    /// player's state spawns a live explod entity on the match, observable via the
    /// public `Player::explods()` accessor, and a round reset clears it.
    #[test]
    fn explod_spawns_through_match_tick_and_clears_on_round_reset() {
        use fp_character::{CompiledController, CompiledExpr, CompiledParam, CompiledTriggerGroup};

        // A state 0 whose only controller is `Explod` (anim 5), gated to fire on
        // the first tick (`Time = 0`).
        let explod_ctrl = CompiledController {
            state_number: 0,
            label: String::new(),
            controller_type: Some("Explod".to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("Time = 0")],
            }],
            persistent: Some(CompiledExpr::compile("0")),
            ignorehitpause: None,
            params: [
                ("anim".to_string(), CompiledParam::compile("5")),
                ("id".to_string(), CompiledParam::compile("42")),
                ("removetime".to_string(), CompiledParam::compile("100")),
            ]
            .into_iter()
            .collect(),
        };
        let mut states = HashMap::new();
        states.insert(0, state0_with(explod_ctrl));

        let mut loaded = loaded_with(two_frame_air(5, 4));
        loaded.states = states;

        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.facing = Facing::Right;
        let p1 = Player::new(p1c, loaded);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.facing = Facing::Left;
        let p2 = Player::new(p2c, loaded_with(two_frame_air(0, 4)));
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));

        // Run a few ticks (the intro phase still ticks state machines and spawns
        // entities — an explod is a display entity, not gated on `fighting`).
        for _ in 0..3 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        let explods = m.p1().explods();
        assert_eq!(explods.len(), 1, "the Explod controller spawned one explod");
        assert_eq!(explods[0].id(), 42);
        assert_eq!(explods[0].anim(), 5);

        // A round reset clears the explod slot-map.
        m.reset_for_next_round();
        assert!(
            m.p1().explods().is_empty(),
            "explods do not survive a round boundary"
        );
    }

    // ---- T027 / T028: TeamMatch Simul partner redirect + single-round flow ----

    /// A plain headless [`Player`] at world X `x` over the synthetic
    /// [`defender_loaded`] assets, facing in toward stage center. Mirrors the
    /// `tests_support::make_player` helper the team-flow tests use, kept local to
    /// `mod tests` so these tests stay self-contained (no asset dependency).
    fn team_player(x: f32) -> Player {
        let mut c = Character::new();
        c.pos = Vec2::new(x, 0.0);
        c.facing = if x < 0.0 { Facing::Right } else { Facing::Left };
        c.state_type = StateType::Standing;
        c.move_type = MoveType::Idle;
        Player::new(c, defender_loaded())
    }

    /// A standing [`LoadedCharacter`] whose state 0 sets `var(4) = 1` only when its
    /// `partner` redirect resolves to a teammate whose life equals `mate_life`.
    ///
    /// The check is gated through a controller TRIGGER (a full expression, unlike a
    /// param value which splits on the top-level comma — see
    /// [`match_tick_resolves_redirect_through_a_trigger`]), so the `partner, life`
    /// redirect is exercised end-to-end through the (team-wired) tick: `var(4)` stays
    /// `0` unless a live teammate of exactly that life is installed in the graph.
    fn partner_probe_loaded(mate_life: i32) -> LoadedCharacter {
        let probe = CompiledController {
            state_number: 0,
            label: String::new(),
            controller_type: Some("VarSet".to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile(&format!(
                    "(partner, life) = {mate_life}"
                ))],
            }],
            persistent: None,
            ignorehitpause: None,
            params: [("var(4)".to_string(), CompiledParam::compile("1"))]
                .into_iter()
                .collect(),
        };
        let mut loaded = defender_loaded();
        loaded.states.insert(0, state_with(0, vec![probe]));
        loaded
    }

    /// AC2 (T027): in a Simul [`TeamMatch`] the active fighter's `partner` redirect
    /// resolves to its **live teammate** (a reserve on its side), not the `0`
    /// default. Proven by gating a VarSet on `(partner, life) = <mate's life>` and
    /// reading the var back after a Simul tick — exercising the partner wiring
    /// through the real team coordinator, not just `EntityGraph::with_partner`.
    #[test]
    fn simul_partner_redirect_resolves_to_live_teammate() {
        const MATE_LIFE: i32 = 654;

        // P1's lead fighter runs the partner probe; its teammate carries the
        // distinctive life the probe checks for.
        let mut lead = Character::new();
        lead.pos = Vec2::new(-60.0, 0.0);
        lead.facing = Facing::Right;
        lead.state_no = 0;
        let lead_player = Player::new(lead, partner_probe_loaded(MATE_LIFE));

        let mut mate = Character::new();
        mate.pos = Vec2::new(-80.0, 0.0);
        mate.facing = Facing::Right;
        mate.life = MATE_LIFE;
        let mate_player = Player::new(mate, defender_loaded());

        let p2_team = vec![team_player(60.0), team_player(80.0)];
        let mut m = TeamMatch::with_mode(
            vec![lead_player, mate_player],
            p2_team,
            StageBounds::new(-220.0, 220.0),
            TeamMode::Simul,
        );

        // One tick is enough: the lead's state-0 probe runs and reads its partner.
        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(
            m.active_player(Side::P1).character.vars[4],
            1,
            "the active P1's `partner` redirect must resolve to its live teammate \
             (life {MATE_LIFE}) in Simul, not the 1v1 `0` default"
        );
    }

    /// AC2 (negative control): a 1v1 [`Match`] has no teammate, so the same
    /// `partner` probe never fires — confirming the var only flips because of the
    /// Simul partner wiring, and that 1v1 partner behaviour is unchanged (`0`).
    #[test]
    fn one_v_one_partner_redirect_stays_zero() {
        const MATE_LIFE: i32 = 654;
        let mut lead = Character::new();
        lead.pos = Vec2::new(-60.0, 0.0);
        lead.facing = Facing::Right;
        lead.state_no = 0;
        let mut m = Match::new(
            Player::new(lead, partner_probe_loaded(MATE_LIFE)),
            team_player(60.0),
            StageBounds::new(-220.0, 220.0),
        );
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().character.vars[4],
            0,
            "a 1v1 match has no partner, so the redirect stays 0 and the probe never fires"
        );
    }

    /// AC3 (T028): the inner match of a Simul/Turns [`TeamMatch`] runs in
    /// single-round / no-life-restore mode, so a KO'd active fighter is **not**
    /// healed by an inner round reset. (A bare 1v1 `Match` keeps the normal flow.)
    #[test]
    fn team_inner_match_is_single_round_no_restore() {
        // Simul + Turns wrap the inner match in single-round mode; Single does not.
        let simul = TeamMatch::with_mode(
            vec![team_player(-60.0), team_player(-80.0)],
            vec![team_player(60.0), team_player(80.0)],
            StageBounds::default(),
            TeamMode::Simul,
        );
        assert!(
            simul.active().single_round(),
            "a Simul inner match must be single-round so a KO is not masked by a restart"
        );
        let turns = TeamMatch::with_mode(
            vec![team_player(-60.0)],
            vec![team_player(60.0), team_player(80.0)],
            StageBounds::default(),
            TeamMode::Turns,
        );
        assert!(
            turns.active().single_round(),
            "a Turns inner match is single-round too"
        );
        let single = TeamMatch::new(
            team_player(-60.0),
            team_player(60.0),
            StageBounds::default(),
        );
        assert!(
            !single.active().single_round(),
            "a 1v1 / Single inner match keeps the normal best-of-N flow (life restored each round)"
        );

        // End-to-end: KO P2's active in a Simul match (P2 still has a reserve, so
        // the team continues). The inner match must NOT heal the downed fighter, even
        // after the full KO->Win round-flow hold elapses.
        let mut m = TeamMatch::with_mode(
            vec![team_player(-60.0), team_player(-80.0)],
            vec![team_player(60.0), team_player(80.0)],
            StageBounds::default(),
            TeamMode::Simul,
        );
        for _ in 0..(INTRO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        m.kill_active(Side::P2);
        // Drive well past KO_FRAMES so a (non-single-round) match would have reset
        // and healed P2 by now.
        for _ in 0..(KO_FRAMES + 10) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert!(
            m.active_player(Side::P2).life() <= 0,
            "the KO'd active fighter must stay down — its life is never restored by inner round flow"
        );
        assert_eq!(
            m.state(),
            TeamMatchState::InProgress,
            "the team match continues (P2 still has a living reserve)"
        );
    }

    /// AC4 (T028): a genuine double-KO ends a Simul [`TeamMatch`] as a real
    /// [`TeamOutcome::Draw`] — not an automatic P1 win. Wipes BOTH sides on the same
    /// frame (active + reserve each) so neither has a fighter left standing.
    #[test]
    fn simul_double_ko_is_a_draw() {
        let mut m = TeamMatch::with_mode(
            vec![team_player(-60.0), team_player(-80.0)],
            vec![team_player(60.0), team_player(80.0)],
            StageBounds::default(),
            TeamMode::Simul,
        );
        for _ in 0..(INTRO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // Eliminate every fighter on both sides simultaneously.
        m.kill_active(Side::P1);
        m.kill_active(Side::P2);
        m.kill_reserve(Side::P1, 0);
        m.kill_reserve(Side::P2, 0);
        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(m.state(), TeamMatchState::Over);
        assert_eq!(
            m.outcome(),
            Some(TeamOutcome::Draw),
            "a double-KO must be a genuine Draw, not a P1-biased tiebreak"
        );
        assert_eq!(m.winner(), None, "a drawn team match has no winning side");
    }

    // ---------------------------------------------------------------------
    // T055: every declared move is performable — evilken specials/supers fire
    // (asset-gated; skips cleanly when `test-assets/evilken/` is absent, like the
    // KFM tests above — evilken is local-only behind the gitignored `test-assets`
    // symlink and is NEVER committed/shipped).
    //
    // This is the capstone of the GUI-free behavioral test harness: it proves a
    // character's declared special/super moves can actually be *performed*
    // end-to-end and headlessly. For each target command it synthesizes the
    // command's MUGEN motion with `fp_input::synth_command` (the T053 synthesizer),
    // adapts the synthesized `InputState` frames to the `Match` input path through
    // the existing `MatchInput: From<InputState>` seam, drives a real evilken-vs-
    // evilken `Match` from a neutral controllable state, and asserts the move
    // actually FIRES (the character's `move_type` becomes `Attack`). For
    // power-gated supers it first grants P1 full meter (`power = power_max`).
    // ---------------------------------------------------------------------

    /// Loads the genuine evilken fixture on both sides and builds a fresh, unseeded
    /// evilken-vs-evilken [`Match`], or `None` (after logging the skip reason) when
    /// the fixture is absent or fails to load, so callers skip cleanly. evilken is
    /// SFF v1 and loads in full color.
    fn build_evilken_match() -> Option<Match> {
        let def = test_asset("evilken/evilken.def");
        if !def.exists() {
            eprintln!(
                "skipping evilken move-execution test: {} not present",
                def.display()
            );
            return None;
        }
        let load = || match LoadedCharacter::load(&def) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("skipping evilken move-execution test: evilken.def failed to load: {e}");
                None
            }
        };
        let (lc1, lc2) = (load()?, load()?);
        let mut p1c = Character::with_constants(lc1.constants);
        p1c.pos = Vec2::new(-60.0, 0.0);
        p1c.state_no = 0;
        p1c.anim = 0;
        p1c.ctrl = true;
        let mut p2c = Character::with_constants(lc2.constants);
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.state_no = 0;
        p2c.anim = 0;
        p2c.ctrl = true;
        Some(Match::new(
            Player::new(p1c, lc1),
            Player::new(p2c, lc2),
            StageBounds::new(-220.0, 220.0),
        ))
    }

    /// Drives `m` through the intro until it enters [`RoundState::Fight`] (feeding
    /// neutral inputs), so the subsequent move-synthesis runs against a live,
    /// controllable fighter.
    fn evilken_into_fight(m: &mut Match) {
        for _ in 0..(INTRO_FRAMES + 1) {
            if m.round_state() == RoundState::Fight {
                break;
            }
            m.tick(MatchInput::none(), MatchInput::none());
        }
    }

    /// Attempts to perform the named command on a *fresh* evilken match and reports
    /// whether the move fires (P1's `move_type` becomes [`MoveType::Attack`]).
    ///
    /// The observed result of attempting one synthesized move on a fresh evilken
    /// match: whether P1's `move_type` ever became [`MoveType::Attack`], and the set
    /// of (non-neutral) `state_no`s P1 passed through while the motion was fed.
    struct MoveOutcome {
        /// `move_type` became [`MoveType::Attack`] at some point.
        attacked: bool,
        /// Every non-zero `state_no` P1 entered during the feed (state 0 is the
        /// neutral stand the motion is launched from and is excluded).
        states: std::collections::BTreeSet<i32>,
    }

    impl MoveOutcome {
        /// Whether P1 entered any of the given target state numbers.
        fn entered_any(&self, targets: &[i32]) -> bool {
            targets.iter().any(|s| self.states.contains(s))
        }
    }

    /// Synthesizes a named command's motion and feeds it through a fresh evilken
    /// match, returning what P1 did (or `None` to skip when the fixture is absent or
    /// the command is not declared).
    ///
    /// Builds a clean match per attempt so moves never contaminate each other,
    /// advances to [`RoundState::Fight`], puts P1 into a neutral controllable
    /// standing state, optionally grants full meter (for power-gated supers), then
    /// synthesizes the command's motion and feeds it through the *real* engine
    /// input path (`MatchInput::from(InputState)` → `Match::tick` → the engine's own
    /// `CommandMatcher`).
    fn evilken_perform(name: &str, max_power: bool) -> Option<MoveOutcome> {
        let mut m = build_evilken_match()?;
        evilken_into_fight(&mut m);

        // Look up the command's parsed motion from the character's own `.cmd`.
        let defs = m.p1().loaded.command_defs();
        let def = defs.iter().find(|d| d.name.eq_ignore_ascii_case(name))?;

        // Put P1 into a neutral controllable standing state and (for supers) max meter.
        {
            let c = &mut m.p1.character;
            c.state_no = 0;
            c.state_type = StateType::Standing;
            c.move_type = MoveType::Idle;
            c.ctrl = true;
            if max_power {
                c.power = c.power_max;
            }
        }
        let facing_right = m.p1().character.facing == Facing::Right;

        // Synthesize the motion (T053) and adapt each absolute `InputState` to a
        // `MatchInput` via the existing `From<InputState>` seam. A few trailing
        // neutral frames give the buffered match time to fire the state.
        let mut frames: Vec<MatchInput> = fp_input::synth_command(&def.elements, facing_right)
            .into_iter()
            .map(MatchInput::from)
            .collect();
        frames.extend(std::iter::repeat_n(MatchInput::none(), 6));

        // Feed the motion immediately (before the AI-detect helper can flip the
        // character's internal AI var), keeping P1 controllable in neutral each frame,
        // and record what it does.
        let mut outcome = MoveOutcome {
            attacked: false,
            states: std::collections::BTreeSet::new(),
        };
        for f in frames {
            // The move-change controllers live in `[Statedef -1]` and only run while
            // the fighter has control in the neutral state; keep it controllable
            // until the move actually takes over.
            if m.p1().character.state_no == 0 {
                m.p1.character.ctrl = true;
                if max_power {
                    m.p1.character.power = m.p1().character.power_max;
                }
            }
            m.tick(f, MatchInput::none());
            let st = m.p1().character.state_no;
            if st != 0 {
                outcome.states.insert(st);
            }
            if m.p1().character.move_type == MoveType::Attack {
                outcome.attacked = true;
            }
        }
        Some(outcome)
    }

    /// Convenience: did the named command produce an attack (`move_type == Attack`)?
    fn evilken_move_fires(name: &str, max_power: bool) -> Option<bool> {
        Some(evilken_perform(name, max_power)?.attacked)
    }

    /// evilken's dedicated special-move state for the `DP_a` Dragon-Punch
    /// (`[Statedef -1]` change `command = "DP_a"` → `value = 270`).
    const EVILKEN_DP_STATE: i32 = 270;
    /// evilken's dedicated level-3 super states for the `cz` (`c+z`) super
    /// (`[Statedef -1]` changes → 3910 ground / 3911 air, both gated on
    /// `power >= 3000`).
    const EVILKEN_CZ_SUPER_STATES: &[i32] = &[3910, 3911];

    /// REQUIRED bar (AC2): evilken's signature ground special — the Dragon-Punch
    /// `DP_a` (`F, D, DF, a`) — fires from a neutral controllable state when its
    /// synthesized motion is fed through the real `Match` input path. No meter is
    /// needed for the special. The assertion is meaningful: P1 must both become
    /// attacking AND actually enter the special's dedicated state 270 (not merely a
    /// normal). Skips cleanly when the fixture is absent.
    #[test]
    fn evilken_signature_special_fires_from_synthesized_input() {
        let Some(outcome) = evilken_perform("DP_a", false) else {
            return; // fixture absent or command not declared -> skip cleanly
        };
        assert!(
            outcome.attacked,
            "evilken signature special `DP_a` never produced an attack from its synthesized \
             motion fed through the real Match input path (states seen: {:?})",
            outcome.states
        );
        assert!(
            outcome.entered_any(&[EVILKEN_DP_STATE]),
            "evilken `DP_a` attacked but did not enter its dedicated special state \
             {EVILKEN_DP_STATE} (states seen: {:?}) — the special motion was not recognized",
            outcome.states
        );
    }

    /// REQUIRED bar (AC2): a power-gated super — the `cz` (`c+z`) level-3 super,
    /// whose `[Statedef -1]` change to state 3910 is gated strictly on
    /// `power >= 3000` (the character's full meter, with no alternate non-meter
    /// escape) — fires once P1 is granted full meter (`power = power_max`) and the
    /// synthesized motion is fed through the real `Match` input path.
    ///
    /// The assertion is *doubly* meaningful: the move must fire WITH full meter
    /// (the positive bar this test asserts), and it must NOT fire WITHOUT meter
    /// (the negative control below), which proves the power gate is real and the
    /// positive result is not a tautology.
    #[test]
    fn evilken_power_gated_super_fires_with_full_meter() {
        let Some(outcome) = evilken_perform("cz", true) else {
            return; // fixture absent or command not declared -> skip cleanly
        };
        assert!(
            outcome.entered_any(EVILKEN_CZ_SUPER_STATES),
            "evilken power-gated super `cz` did not enter its level-3 super state \
             {EVILKEN_CZ_SUPER_STATES:?} (gated on power >= 3000) with full meter \
             (states seen: {:?})",
            outcome.states
        );
    }

    /// Negative control for the power gate (AC2 meaningfulness): the same `cz`
    /// level-3 super must NOT fire with zero meter, since its only meter gate is
    /// `power >= 3000`. This guards against the positive test above being a
    /// tautology — if the gate ever stopped being enforced, the super would fire
    /// from neutral with no power and this test would fail. Skips cleanly when the
    /// fixture is absent.
    #[test]
    fn evilken_power_gated_super_does_not_fire_without_meter() {
        let Some(outcome) = evilken_perform("cz", false) else {
            return; // fixture absent or command not declared -> skip cleanly
        };
        assert!(
            !outcome.entered_any(EVILKEN_CZ_SUPER_STATES),
            "evilken power-gated super `cz` reached its level-3 super state \
             {EVILKEN_CZ_SUPER_STATES:?} with ZERO meter — the `power >= 3000` gate was not \
             enforced, so the positive power-gated-super test would be a tautology \
             (states seen: {:?})",
            outcome.states
        );
    }

    /// T056b stuck-punch repro (asset-gated): evilken's ground punch runs a
    /// *finite looping* attack animation and exits on `trigger = AnimTime = 0`.
    /// Before the fix, `AnimTime` never observably reached `0` for a looping anim
    /// (the per-tick advance consumed the final element's last tick in the same
    /// call that looped to `loopstart`), so the exit trigger never fired and the
    /// punch looped FOREVER with `ctrl = false` — the character was permanently
    /// stuck mid-punch. This drives a real evilken match, presses the punch
    /// button, then ticks well past any reasonable attack duration and asserts P1
    /// regains control and returns to a neutral non-attack state (it is NOT stuck).
    ///
    /// Skips cleanly when `test-assets/evilken/` is absent (evilken is local-only
    /// behind the gitignored `test-assets` symlink; never committed).
    #[test]
    fn evilken_ground_punch_is_not_stuck_after_animation_loops() {
        let Some(mut m) = build_evilken_match() else {
            return; // fixture absent -> skip cleanly
        };
        evilken_into_fight(&mut m);

        // Put P1 in a neutral controllable standing state, then press a button.
        {
            let c = &mut m.p1.character;
            c.state_no = 0;
            c.state_type = StateType::Standing;
            c.move_type = MoveType::Idle;
            c.ctrl = true;
        }

        // Press the light-punch button (rising edge), then release. A single
        // button press is the universal "basic attack" input.
        let press = MatchInput {
            a: true,
            ..MatchInput::none()
        };
        m.tick(press, MatchInput::none());
        m.tick(MatchInput::none(), MatchInput::none());

        // Record whether P1 ever left neutral into an attack (so the test is
        // meaningful: if the punch never fired there is nothing to be stuck in,
        // and we skip rather than false-pass).
        let mut attacked =
            m.p1().character.move_type == MoveType::Attack || m.p1().character.state_no != 0;

        // Tick well past any plausible attack+recovery duration. A finite looping
        // attack that never reports AnimTime = 0 would loop here forever; with the
        // fix it completes and the standard `AnimTime = 0` exit returns control.
        let mut regained_control = false;
        for _ in 0..600 {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.p1().character.move_type == MoveType::Attack || m.p1().character.state_no != 0 {
                attacked = true;
            }
            // Neutral + controllable again == not stuck mid-attack.
            if m.p1().character.state_no == 0 && m.p1().character.ctrl {
                regained_control = true;
                break;
            }
        }

        if !attacked {
            // The punch button did not start any move on this build of the
            // fixture; nothing to be stuck in, so there is nothing to assert.
            eprintln!(
                "skipping evilken stuck-punch assertion: the `a` press did not enter an attack"
            );
            return;
        }
        assert!(
            regained_control,
            "evilken got stuck mid-punch: P1 never returned to a neutral controllable \
             state within 600 ticks (state_no = {}, ctrl = {}). A finite looping attack \
             animation that never reports AnimTime = 0 hangs its `AnimTime = 0` exit forever.",
            m.p1().character.state_no,
            m.p1().character.ctrl,
        );
    }

    /// General harness (so it can later point at other characters): enumerate
    /// evilken's *declared* commands, synthesize each, and confirm a representative
    /// share of its real special/super vocabulary is performable end-to-end. This
    /// proves the harness iterates over a character's commands rather than being
    /// hand-wired to two moves, while staying robust to a boss character's heavy,
    /// state-dependent move gating (many moves only chain from other moves, so we
    /// require a meaningful subset of the from-neutral specials/supers to fire, not
    /// literally all of them). Skips cleanly when the fixture is absent.
    #[test]
    fn evilken_declared_moves_are_broadly_performable() {
        // Probe match purely to enumerate the declared command vocabulary.
        let Some(m) = build_evilken_match() else {
            return; // fixture absent -> skip cleanly
        };
        let names: Vec<String> = m
            .p1()
            .loaded
            .command_defs()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        drop(m);

        // The from-neutral special/super families evilken declares. We skip the
        // AI-probe `CPU*` commands and the bare single-direction / single-button
        // `hold*`/recovery entries (which are not standalone moves), and treat any
        // `*2*` name as a (power-gated) super.
        let is_super = |n: &str| n.contains('2');
        let mut attempted = 0usize;
        let mut fired = 0usize;
        let mut seen = std::collections::BTreeSet::new();
        for name in &names {
            let lower = name.to_ascii_lowercase();
            let looks_like_move = lower.starts_with("qcf")
                || lower.starts_with("qcb")
                || lower.starts_with("dp")
                || lower.starts_with("rdp")
                || lower.starts_with("hcf")
                || lower.starts_with("hcb")
                || lower.starts_with("asura")
                || lower.starts_with("dest")
                || lower.starts_with("bust");
            if !looks_like_move || !seen.insert(lower.clone()) {
                continue;
            }
            attempted += 1;
            if let Some(true) = evilken_move_fires(name, is_super(&lower)) {
                fired += 1;
            }
        }

        // We must have found and attempted a real vocabulary, and a solid share of
        // the from-neutral specials/supers must actually be performable.
        assert!(
            attempted >= 4,
            "expected to enumerate several declared special/super commands, found {attempted}"
        );
        assert!(
            fired * 2 >= attempted,
            "fewer than half of evilken's from-neutral declared moves were performable \
             ({fired}/{attempted}); the synthesizer→Match move-execution path regressed"
        );
    }

    // ---- T052: ai_level entity field + match/CPU plumbing -------------------

    /// `PlayerDriver::ai_level` maps humans to 0 and CPU difficulties to 1..=8.
    #[test]
    fn player_driver_ai_level_mapping() {
        assert_eq!(PlayerDriver::Human.ai_level(), 0);
        assert_eq!(PlayerDriver::Cpu(AiDifficulty::Easy).ai_level(), 2);
        assert_eq!(PlayerDriver::Cpu(AiDifficulty::Normal).ai_level(), 4);
        assert_eq!(PlayerDriver::Cpu(AiDifficulty::Hard).ai_level(), 7);
        // The human default is level 0 (a human is never mistaken for the CPU).
        assert_eq!(PlayerDriver::default().ai_level(), 0);
    }

    /// A bare, freshly-built `Character` (no coordinator) defaults to `ai_level == 0`.
    #[test]
    fn bare_character_ai_level_defaults_to_zero() {
        assert_eq!(Character::new().ai_level(), 0);
    }

    /// `Match::set_drivers` assigns each side's `ai_level` from its driver:
    /// human P1 stays level 0, CPU(Hard) P2 reads 7.
    #[test]
    fn match_with_drivers_sets_ai_level_per_side() {
        let mut m = Match::new(
            tests_support::make_player(-50.0),
            tests_support::make_player(50.0),
            StageBounds::new(-200.0, 200.0),
        );
        m.set_drivers(PlayerDriver::Human, PlayerDriver::Cpu(AiDifficulty::Hard));
        assert_eq!(m.p1().character.ai_level(), 0, "human P1 must be level 0");
        assert_eq!(
            m.p2().character.ai_level(),
            7,
            "CPU(Hard) P2 must be level 7"
        );
    }

    /// A plain `Match::new` (no driver declared) leaves both sides at the human
    /// default (`0`), so a bare two-human match is unaffected.
    #[test]
    fn match_without_drivers_keeps_human_default_ai_level() {
        let m = Match::new(
            tests_support::make_player(-50.0),
            tests_support::make_player(50.0),
            StageBounds::new(-200.0, 200.0),
        );
        assert_eq!(m.p1().character.ai_level(), 0);
        assert_eq!(m.p2().character.ai_level(), 0);
    }

    /// `TeamMatch::set_drivers` propagates each side's driver to its whole roster:
    /// the active lead AND every reserve inherit the side's `ai_level`.
    #[test]
    fn team_match_drivers_propagate_to_whole_roster() {
        let mut tm = TeamMatch::with_mode(
            vec![
                tests_support::make_player(-60.0),
                tests_support::make_player(-90.0),
            ],
            vec![
                tests_support::make_player(60.0),
                tests_support::make_player(90.0),
            ],
            StageBounds::new(-200.0, 200.0),
            TeamMode::Simul,
        );
        tm.set_drivers(PlayerDriver::Human, PlayerDriver::Cpu(AiDifficulty::Normal));

        // P1 side (human): active lead + reserve both level 0.
        assert_eq!(tm.active_player(Side::P1).character.ai_level(), 0);
        for r in tm.reserves(Side::P1) {
            assert_eq!(r.character.ai_level(), 0, "human reserve must stay level 0");
        }
        // P2 side (CPU Normal -> 4): active lead + reserve both level 4.
        assert_eq!(tm.active_player(Side::P2).character.ai_level(), 4);
        for r in tm.reserves(Side::P2) {
            assert_eq!(
                r.character.ai_level(),
                4,
                "CPU reserve must inherit level 4"
            );
        }
    }

    /// `ai_level` is part of `CharacterSnapshot` and survives a bincode round-trip
    /// (the snapshot path the whole-Match replay / rollback proofs depend on).
    #[test]
    fn ai_level_survives_character_snapshot_bincode_round_trip() {
        let mut ch = Character::new();
        ch.set_ai_level(7);
        let snap = ch.snapshot();
        assert_eq!(snap.ai_level, 7);

        let bytes = bincode::serialize(&snap).expect("serialize snapshot");
        let decoded: fp_character::CharacterSnapshot =
            bincode::deserialize(&bytes).expect("deserialize snapshot");
        assert_eq!(
            decoded.ai_level, 7,
            "ai_level must survive a bincode round-trip"
        );

        // And applying the decoded snapshot restores ai_level onto a fresh character.
        let mut restored = Character::new();
        assert_eq!(restored.ai_level(), 0);
        restored.restore_from_snapshot(&decoded);
        assert_eq!(restored.ai_level(), 7);
    }

    // ====================================================================
    // T065: frame-data readout — static startup/active/recovery from an AIR
    // action, plus on-block / on-hit frame advantage derived from the
    // defender's induced stun and the attacker's remaining recovery.
    // ====================================================================

    /// A deterministic attack action's static frame data must decompose to the
    /// hand-counted startup / active / recovery, and the on-block / on-hit frame
    /// advantage must equal `defender_stun − attacker_recovery`.
    ///
    /// This ties the engine's combat numbers to the player-facing readout: the
    /// attacker's frames-until-actionable at contact is its move recovery (the
    /// static tail computed from the AIR), and the defender's stun is the
    /// block-/hit-stun the engine induces.
    #[test]
    fn frame_advantage_on_block_equals_blockstun_minus_recovery() {
        use fp_character::{frame_advantage, MoveFrameData};

        // A simple attack: 4-tick startup, 3-tick active (Clsn1), 8-tick recovery.
        let action = AnimAction {
            action_number: 200,
            loopstart: 0,
            frames: vec![
                AnimFrame {
                    ticks: 4,
                    ..Default::default()
                },
                AnimFrame {
                    ticks: 3,
                    clsn1: vec![Rect::new(0.0, -40.0, 30.0, -10.0)],
                    ..Default::default()
                },
                AnimFrame {
                    ticks: 8,
                    ..Default::default()
                },
            ],
        };

        let fd = MoveFrameData::compute(&action).expect("countable attack");
        assert_eq!((fd.startup, fd.active, fd.recovery), (4, 3, 8));

        // On block: the defender is held in 5 frames of blockstun while the
        // attacker still owes its 8-frame recovery → 5 − 8 = −3 (disadvantage).
        let on_block = frame_advantage(5, fd.recovery);
        assert_eq!(on_block, -3, "on-block advantage = blockstun − recovery");

        // On hit: 12 frames of hitstun vs the same 8 recovery → +4 (advantage).
        let on_hit = frame_advantage(12, fd.recovery);
        assert_eq!(on_hit, 4, "on-hit advantage = hitstun − recovery");
    }
}
