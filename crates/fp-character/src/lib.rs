//! # fp-character
//!
//! Character management for the Fighters Paradise engine. Contains the main
//! character struct, state machine execution, helper entity lifecycle, and
//! projectile management.
//!
//! ## What this module provides (task 5.1)
//!
//! This is Phase 5's *foundation*: the live MUGEN [`Character`] entity and its
//! [`fp_vm::EvalContext`] implementation. A [`Character`] holds the runtime
//! state a trigger expression can read — position/velocity, life/power, the
//! current state and animation cursors, and the int/float/system variable banks
//! — and answers parsed trigger expressions through the
//! [`EvalContext`] trait. With this in place the expression
//! VM ([`fp-vm`](fp_vm)) can evaluate real KFM triggers (`Time`, `StateNo`,
//! `Vel Y`, `var(1)`, `command = "fwd"`, …) against a concrete entity.
//!
//! ## Loading characters from disk (task 5.2)
//!
//! [`LoadedCharacter::load`] turns a character `.def` path into a ready-to-run
//! [`LoadedCharacter`]: it parses the `.def`, resolves and loads the referenced
//! SFF/AIR/CNS(+`stcommon`)/CMD/SND files relative to the `.def` directory,
//! reads the character constants from the CNS `[Data]`/`[Size]`/`[Velocity]`/
//! `[Movement]` groups (task 5.3 expands this beyond 5.2's `[Data]`-only read to
//! the modeled `[Size]`/`[Velocity]`/`[Movement]` fields — see
//! [`CharacterConstants`]), merges the CNS state files in MUGEN order
//! (`stcommon` last, fill-missing only), and **compiles every trigger and
//! controller parameter expression** via [`fp_vm::parse_str`] at load time. A
//! bad expression compiles to a const-`0` [`fp_vm::Expr`] with a
//! `tracing::warn!`; missing optional files are warn-logged and skipped. See the
//! [`loader`] module.
//!
//! ## Running the state machine (task 5.3)
//!
//! [`Character::tick`] advances a live [`Character`] one 60Hz tick against its
//! [`LoadedCharacter`]: it processes the special states (`-3`, `-2`, `-1`) then
//! the current state in MUGEN order, gates each controller on its
//! `triggerall` (AND) and numbered trigger groups (OR, with the [CB6 contiguity
//! rule](executor)), honors `persistent`/`ignorehitpause`, performs state entry
//! and `ChangeState` transitions, applies the statedef `physics`
//! (friction/gravity), and advances the animation cursor from the AIR frame
//! durations. See the [`executor`] module.
//!
//! Out of scope here (later tasks): the full ~100-controller dispatch set (task
//! 5.4 — this task wires only `ChangeState`/`VelSet`/`VelAdd`/`CtrlSet`/`Null`),
//! `fp-app` integration (task 5.5), real get-hit state (Phase 6), and
//! multi-entity redirection (Phase 7).
//!
//! ## Trigger resolution model
//!
//! [`Character`] implements [`EvalContext`] by matching
//! trigger names **case-insensitively** and returning a
//! [`Value`]. Unknown triggers, out-of-range variable indices, and
//! unresolved redirections resolve to a safe default
//! ([`Value::DEFAULT`] / [`None`]) — never a panic — in
//! keeping with the engine-wide "never crash on bad content" rule.
//!
//! ### Letter-coded triggers (`StateType`, `MoveType`, `Physics`)
//!
//! MUGEN compares `StateType`, `MoveType`, and `Physics` against bare letter
//! tokens (`StateType = A`, `MoveType = I`). The expression parser sees the
//! right-hand letter as an ordinary identifier, so the comparison only succeeds
//! if *both* sides resolve to the same integer. [`Character`] therefore answers
//! both the category trigger (`StateType` → the current value's code) **and**
//! the bare letter idents (`A`, `S`, `C`, `L`, `I`, `H`, `N`, `U`) with a stable
//! per-category code, so `StateType = A` evaluates correctly end-to-end. See
//! [`StateType`], [`MoveType`], and [`Physics`].

#![warn(missing_docs)]

pub mod executor;
pub mod loader;

pub use executor::TickReport;
pub use loader::{
    CompiledController, CompiledExpr, CompiledState, CompiledTriggerGroup, LoadedCharacter,
};

use fp_core::Vec2;
use fp_vm::{EvalContext, Value};

/// Number of integer variables (`var(0)`..=`var(59)`) every player owns.
pub const NUM_VARS: usize = 60;
/// Number of float variables (`fvar(0)`..=`fvar(39)`) every player owns.
pub const NUM_FVARS: usize = 40;
/// Number of system integer variables (`sysvar(0)`..=`sysvar(4)`).
pub const NUM_SYSVARS: usize = 5;
/// Number of system float variables (`sysfvar(0)`..=`sysfvar(4)`).
pub const NUM_SYSFVARS: usize = 5;

/// Which way the character is facing.
///
/// MUGEN's `facing` is `1` (right) or `-1` (left); the engine multiplies
/// relative offsets by this sign. [`Facing::sign`] yields that multiplier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Facing {
    /// Facing right; relative-X offsets are applied as written (sign `+1`).
    #[default]
    Right,
    /// Facing left; relative-X offsets are mirrored (sign `-1`).
    Left,
}

impl Facing {
    /// Returns the MUGEN facing sign: `+1` when facing right, `-1` when left.
    #[must_use]
    pub const fn sign(self) -> i32 {
        match self {
            Facing::Right => 1,
            Facing::Left => -1,
        }
    }
}

/// The character's stance category (`Statedef` `type`): standing, crouching,
/// air, lying, or unchanged.
///
/// This is the value read by the `StateType` trigger. Each variant carries a
/// stable integer [`code`](StateType::code) so that `StateType = A` (where the
/// bare `A` resolves to [`StateType::code`] for [`StateType::Air`]) compares
/// equal. See the [crate-level letter-coded triggers note](crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StateType {
    /// `S` — standing.
    #[default]
    Standing,
    /// `C` — crouching.
    Crouching,
    /// `A` — in the air.
    Air,
    /// `L` — lying down (knocked down).
    Lying,
    /// `U` — unchanged (inherit the previous state's type).
    Unchanged,
}

impl StateType {
    /// The stable integer code this stance resolves to in trigger comparisons.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            StateType::Standing => CODE_S,
            StateType::Crouching => CODE_C,
            StateType::Air => CODE_A,
            StateType::Lying => CODE_L,
            StateType::Unchanged => CODE_U,
        }
    }

    /// Parses a `Statedef` `type` token (`S`/`C`/`A`/`L`/`U`), case-insensitively.
    ///
    /// Returns `None` for an unrecognized token; the caller keeps the previous
    /// value (MUGEN treats an absent/invalid `type` as "unchanged").
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        let t = token.trim();
        if t.eq_ignore_ascii_case("S") {
            Some(StateType::Standing)
        } else if t.eq_ignore_ascii_case("C") {
            Some(StateType::Crouching)
        } else if t.eq_ignore_ascii_case("A") {
            Some(StateType::Air)
        } else if t.eq_ignore_ascii_case("L") {
            Some(StateType::Lying)
        } else if t.eq_ignore_ascii_case("U") {
            Some(StateType::Unchanged)
        } else {
            None
        }
    }
}

/// The character's action category (`Statedef` `movetype`): attacking, idle,
/// being hit, or unchanged.
///
/// This is the value read by the `MoveType` trigger; see [`MoveType::code`] and
/// the [crate-level letter-coded triggers note](crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MoveType {
    /// `A` — attacking.
    Attack,
    /// `I` — idle (the default neutral move type).
    #[default]
    Idle,
    /// `H` — being hit.
    BeingHit,
    /// `U` — unchanged (inherit the previous state's move type).
    Unchanged,
}

impl MoveType {
    /// The stable integer code this move type resolves to in trigger
    /// comparisons.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            MoveType::Attack => CODE_A,
            MoveType::Idle => CODE_I,
            MoveType::BeingHit => CODE_H,
            MoveType::Unchanged => CODE_U,
        }
    }

    /// Parses a `Statedef` `movetype` token (`A`/`I`/`H`/`U`),
    /// case-insensitively. Returns `None` for an unrecognized token.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        let t = token.trim();
        if t.eq_ignore_ascii_case("A") {
            Some(MoveType::Attack)
        } else if t.eq_ignore_ascii_case("I") {
            Some(MoveType::Idle)
        } else if t.eq_ignore_ascii_case("H") {
            Some(MoveType::BeingHit)
        } else if t.eq_ignore_ascii_case("U") {
            Some(MoveType::Unchanged)
        } else {
            None
        }
    }
}

/// The character's physics mode (`Statedef` `physics`): stand friction, crouch
/// friction, air (gravity + landing), none, or unchanged.
///
/// This is the value read by the `Physics` trigger; see [`Physics::code`] and
/// the [crate-level letter-coded triggers note](crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Physics {
    /// `S` — standing friction.
    Stand,
    /// `C` — crouching friction.
    Crouch,
    /// `A` — air physics (gravity applied, landing detection).
    Air,
    /// `N` — none (no engine physics applied).
    #[default]
    None,
    /// `U` — unchanged (inherit the previous state's physics).
    Unchanged,
}

impl Physics {
    /// The stable integer code this physics mode resolves to in trigger
    /// comparisons.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Physics::Stand => CODE_S,
            Physics::Crouch => CODE_C,
            Physics::Air => CODE_A,
            Physics::None => CODE_N,
            Physics::Unchanged => CODE_U,
        }
    }

    /// Parses a `Statedef` `physics` token (`S`/`C`/`A`/`N`/`U`),
    /// case-insensitively. Returns `None` for an unrecognized token.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        let t = token.trim();
        if t.eq_ignore_ascii_case("S") {
            Some(Physics::Stand)
        } else if t.eq_ignore_ascii_case("C") {
            Some(Physics::Crouch)
        } else if t.eq_ignore_ascii_case("A") {
            Some(Physics::Air)
        } else if t.eq_ignore_ascii_case("N") {
            Some(Physics::None)
        } else if t.eq_ignore_ascii_case("U") {
            Some(Physics::Unchanged)
        } else {
            None
        }
    }
}

// ---- Letter-token integer codes --------------------------------------------
//
// MUGEN compares letter-coded triggers (`StateType`, `MoveType`, `Physics`)
// against bare letters parsed as identifiers. For `StateType = A` to be true,
// the `StateType` trigger and the bare `A` ident must resolve to the SAME
// integer. We assign each distinct letter a stable code (the exact integers are
// arbitrary, but they must be internally consistent and mutually distinct so
// distinct categories never alias). See `Character::letter_code`.

/// Code for the `S` token (standing / stand-friction).
const CODE_S: i32 = 0;
/// Code for the `C` token (crouching / crouch-friction).
const CODE_C: i32 = 1;
/// Code for the `A` token (air / attacking).
const CODE_A: i32 = 2;
/// Code for the `L` token (lying).
const CODE_L: i32 = 3;
/// Code for the `I` token (idle).
const CODE_I: i32 = 4;
/// Code for the `H` token (being hit).
const CODE_H: i32 = 5;
/// Code for the `N` token (no physics).
const CODE_N: i32 = 6;
/// Code for the `U` token (unchanged).
const CODE_U: i32 = 7;

/// Axis code passed by the evaluator for the `X` component of `Pos`/`Vel`.
const AXIS_X: i32 = 0;
/// Axis code passed by the evaluator for the `Y` component of `Pos`/`Vel`.
const AXIS_Y: i32 = 1;

/// Sentinel returned by `AnimElemTime(n)` when element `n` has **not yet been
/// reached**.
///
/// MUGEN reports the time-since-element as negative until the cursor reaches
/// that element; the VM's `AnimElem = N, op M` lowering treats `AnimElemTime(N)
/// >= 0` as the "reached" guard, so a future element must read negative to keep
/// the tail from spuriously firing. `-1` is the conventional MUGEN value.
const ANIM_ELEM_NOT_REACHED: i32 = -1;

/// Static, per-character constants read from the `.cns`
/// `[Data]`/`[Size]`/`[Velocity]`/`[Movement]` groups.
///
/// These are authored values loaded once from the character's `.cns`. Task 5.1
/// shipped only the `[Data]` maxima needed to initialize live state; task 5.3
/// expands this to the [`Size`](CharacterConstants::size),
/// [`Velocity`](CharacterConstants::velocity), and
/// [`Movement`](CharacterConstants::movement) groups the executor needs
/// (player widths, walk/jump velocities, gravity and friction). Every field has
/// a safe MUGEN-style default; unknown/unmodeled constants resolve to the safe
/// default rather than failing the load.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterConstants {
    /// Maximum life (`[Data] life`). Defaults to MUGEN's `1000`.
    pub life_max: i32,
    /// Maximum power (`[Data] power`). Defaults to MUGEN's `3000`.
    pub power_max: i32,
    /// Starting attack scaling (`[Data] attack`), as a percentage. Defaults to
    /// `100`.
    pub attack: i32,
    /// Starting defence scaling (`[Data] defence`), as a percentage. Defaults
    /// to `100`.
    pub defence: i32,
    /// `[Size]` group: player dimensions.
    pub size: SizeConstants,
    /// `[Velocity]` group: walk and jump velocities.
    pub velocity: VelocityConstants,
    /// `[Movement]` group: gravity and friction coefficients.
    pub movement: MovementConstants,
}

impl Default for CharacterConstants {
    fn default() -> Self {
        Self {
            life_max: 1000,
            power_max: 3000,
            attack: 100,
            defence: 100,
            size: SizeConstants::default(),
            velocity: VelocityConstants::default(),
            movement: MovementConstants::default(),
        }
    }
}

/// The `[Size]` constant group: the character's collision/positioning
/// dimensions.
///
/// Only the fields the executor and physics need are modeled here (player
/// widths and height); the remaining `[Size]` keys (`xscale`, `head.pos`, …)
/// are not read yet. Each defaults to KFM's value, MUGEN's de-facto baseline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeConstants {
    /// `ground.front` — player half-width forward, on the ground (pixels).
    pub ground_front: i32,
    /// `ground.back` — player half-width backward, on the ground (pixels).
    pub ground_back: i32,
    /// `height` — player height, used for jump-over checks (pixels).
    pub height: i32,
}

impl Default for SizeConstants {
    fn default() -> Self {
        // KFM's authored values (the MUGEN baseline character).
        Self {
            ground_front: 16,
            ground_back: 15,
            height: 60,
        }
    }
}

/// The `[Velocity]` constant group: authored walk and jump velocities, in
/// pixels/tick.
///
/// Velocities are stored as `(x, y)` pairs. Forward velocities assume facing
/// right; the executor mirrors them by [`Facing::sign`]. Only the fields needed
/// for basic locomotion are modeled (walk forward/back, run forward, neutral
/// and up jump); the air-recover velocities and run-jump pairs are not read yet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VelocityConstants {
    /// `walk.fwd` — forward walking velocity `(x, y)`. MUGEN authors this as a
    /// bare x value; `y` is `0`.
    pub walk_fwd: Vec2<f32>,
    /// `walk.back` — backward walking velocity `(x, y)` (x is negative).
    pub walk_back: Vec2<f32>,
    /// `run.fwd` — forward running velocity `(x, y)`.
    pub run_fwd: Vec2<f32>,
    /// `jump.neu` — neutral jump velocity `(x, y)` (y is negative = upward).
    pub jump_neu: Vec2<f32>,
    /// `jump.up` y-velocity — the upward jump speed. MUGEN derives jump y from
    /// `jump.neu.y`; when an explicit `jump.up` is absent this mirrors
    /// `jump_neu.y`.
    pub jump_up: f32,
}

impl Default for VelocityConstants {
    fn default() -> Self {
        // KFM's authored values.
        Self {
            walk_fwd: Vec2::new(2.4, 0.0),
            walk_back: Vec2::new(-2.2, 0.0),
            run_fwd: Vec2::new(4.6, 0.0),
            jump_neu: Vec2::new(0.0, -8.4),
            jump_up: -8.4,
        }
    }
}

/// The `[Movement]` constant group: gravity and friction.
///
/// `yaccel` is the per-tick downward acceleration applied by air physics
/// (`Physics::Air`). `stand.friction`/`crouch.friction` are the multiplicative
/// coefficients applied to x-velocity each tick by stand/crouch physics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MovementConstants {
    /// `yaccel` — downward acceleration in pixels/tick² (gravity).
    pub yaccel: f32,
    /// `stand.friction` — x-velocity multiplier per tick while standing.
    pub stand_friction: f32,
    /// `crouch.friction` — x-velocity multiplier per tick while crouching.
    pub crouch_friction: f32,
}

impl Default for MovementConstants {
    fn default() -> Self {
        // KFM's authored values.
        Self {
            yaccel: 0.44,
            stand_friction: 0.85,
            crouch_friction: 0.82,
        }
    }
}

/// A source of currently-active command names, queried by the `command = "name"`
/// trigger.
///
/// The state-machine executor (task 5.3) feeds this from `fp-input`'s command
/// recognizer each tick. Modeling it as a trait keeps `fp-character` free of an
/// `fp-input` dependency at this stage and lets tests inject a synthetic source.
/// Implementations match command names case-insensitively (MUGEN command labels
/// are case-insensitive) and must never panic.
pub trait CommandSource {
    /// Returns `true` if the named command fired on the current tick.
    fn is_active(&self, name: &str) -> bool;
}

/// A [`CommandSource`] for which no command is ever active.
///
/// Used as the default until the executor injects a real source; with it,
/// `command = "x"` evaluates to `0` (the command never fires) rather than
/// erroring.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCommands;

impl CommandSource for NoCommands {
    fn is_active(&self, _name: &str) -> bool {
        false
    }
}

/// A [`CommandSource`] backed by an owned list of active command names, matched
/// case-insensitively.
///
/// Convenient for the executor to rebuild each tick and for tests to inject a
/// known set of active commands.
#[derive(Debug, Clone, Default)]
pub struct ActiveCommands {
    names: Vec<String>,
}

impl ActiveCommands {
    /// Creates an empty active-command set.
    #[must_use]
    pub fn new() -> Self {
        Self { names: Vec::new() }
    }

    /// Builds an active-command set from any iterator of names.
    pub fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    /// Marks a command name active for this tick.
    pub fn insert(&mut self, name: impl Into<String>) {
        self.names.push(name.into());
    }

    /// Clears all active commands (called at the start of each tick).
    pub fn clear(&mut self) {
        self.names.clear();
    }
}

impl CommandSource for ActiveCommands {
    fn is_active(&self, name: &str) -> bool {
        self.names.iter().any(|n| n.eq_ignore_ascii_case(name))
    }
}

/// A live MUGEN character entity: the runtime state the engine mutates each tick
/// and that trigger expressions read through [`EvalContext`].
///
/// A `Character` is **struct-based** (not ECS): MUGEN entities have a fixed set
/// of properties and the expression VM needs direct field access. The fields
/// below mirror the per-player state MUGEN keeps; the
/// [`EvalContext`] impl exposes them to parsed trigger expressions.
///
/// The command seam ([`commands`](Character::commands)) is boxed behind
/// [`CommandSource`] so the executor can swap in `fp-input`'s recognizer later
/// without `fp-character` depending on it now. Construct one with
/// [`Character::new`] (MUGEN defaults) or [`Character::with_constants`] (seeded
/// from authored life/power maxima), then set fields directly.
pub struct Character {
    // ---- Kinematics --------------------------------------------------------
    /// World position in pixels (`Pos X`/`Pos Y`). MUGEN's origin is the
    /// character axis; Y increases downward.
    pub pos: Vec2<f32>,
    /// Velocity in pixels/tick (`Vel X`/`Vel Y`).
    pub vel: Vec2<f32>,
    /// Which way the character currently faces (`Facing` trigger sign).
    pub facing: Facing,

    // ---- Resources ---------------------------------------------------------
    /// Current life (`Life`). Clamped to `0..=life_max` by gameplay.
    pub life: i32,
    /// Maximum life (`LifeMax`).
    pub life_max: i32,
    /// Current power / super meter (`Power`).
    pub power: i32,
    /// Maximum power (`PowerMax`).
    pub power_max: i32,
    /// Whether the player currently has control (`Ctrl`).
    pub ctrl: bool,

    // ---- State categories --------------------------------------------------
    /// Stance category (`StateType`).
    pub state_type: StateType,
    /// Action category (`MoveType`).
    pub move_type: MoveType,
    /// Physics mode (`Physics`).
    pub physics: Physics,

    // ---- Animation cursor --------------------------------------------------
    /// Current animation (action) id (`Anim`).
    pub anim: i32,
    /// Zero-based index of the current animation element within the action.
    ///
    /// Note: the MUGEN `AnimElem` trigger is **one-based** (the first element is
    /// element 1). This field stores the zero-based cursor; the `AnimElemNo`
    /// trigger reports `anim_elem + 1`.
    pub anim_elem: i32,
    /// Ticks elapsed within the current animation element (`AnimElemTime`).
    pub anim_elem_time: i32,
    /// Ticks remaining until the current animation finishes (`AnimTime`).
    ///
    /// MUGEN reports `AnimTime` as `0` on the last tick of a finite animation
    /// and counts negatively past the end for looping animations; the loader and
    /// executor maintain this value. Stored directly so the trigger is a field
    /// read.
    pub anim_time: i32,

    // ---- State machine cursor ----------------------------------------------
    /// Current state number (`StateNo`).
    pub state_no: i32,
    /// Previous state number (`PrevStateNo`).
    pub prev_state_no: i32,
    /// Ticks elapsed in the current state (`Time` / `StateTime`).
    pub state_time: i32,

    // ---- Variable banks ----------------------------------------------------
    /// Integer variable bank, `var(0)`..=`var(59)`.
    pub vars: [i32; NUM_VARS],
    /// Float variable bank, `fvar(0)`..=`fvar(39)`.
    pub fvars: [f32; NUM_FVARS],
    /// System integer variable bank, `sysvar(0)`..=`sysvar(4)`.
    pub sysvars: [i32; NUM_SYSVARS],
    /// System float variable bank, `sysfvar(0)`..=`sysfvar(4)`.
    pub sysfvars: [f32; NUM_SYSFVARS],

    // ---- Static data -------------------------------------------------------
    /// Authored per-character constants loaded from the `.cns`.
    pub constants: CharacterConstants,

    // ---- Seams -------------------------------------------------------------
    /// Source of currently-active commands for the `command = "name"` trigger.
    ///
    /// Defaults to [`NoCommands`]; the executor swaps in `fp-input`'s recognizer.
    pub commands: Box<dyn CommandSource>,

    // ---- Executor bookkeeping ---------------------------------------------
    /// Per-state-entry firing counts used by the executor to enforce the
    /// `persistent` universal parameter, keyed by
    /// `(owning_state_number, controller_index)`.
    ///
    /// The key's first component is the controller's *owning* state number (the
    /// `N` in `[State N, label]`), **not** the live `state_no`: while a special
    /// state (`-3`/`-2`/`-1`) runs, `state_no` is still the current numbered
    /// state, so keying by it would make a special-state controller and a
    /// current-state controller that share an index collide on one count.
    ///
    /// Each entry counts how many times a controller has *qualified* (its gating
    /// passed) since the current state was entered. The executor consults and
    /// updates it in [`Character::tick`]; it is cleared on every state entry. It
    /// is part of the public struct only because the entity is struct-based (no
    /// hidden state), but callers other than the executor should not touch it.
    pub fire_counts: std::collections::HashMap<(i32, usize), i32>,
}

impl Default for Character {
    fn default() -> Self {
        let constants = CharacterConstants::default();
        Self {
            pos: Vec2::<f32>::ZERO,
            vel: Vec2::<f32>::ZERO,
            facing: Facing::default(),
            life: constants.life_max,
            life_max: constants.life_max,
            power: 0,
            power_max: constants.power_max,
            ctrl: false,
            state_type: StateType::default(),
            move_type: MoveType::default(),
            physics: Physics::default(),
            anim: 0,
            anim_elem: 0,
            anim_elem_time: 0,
            anim_time: 0,
            state_no: 0,
            prev_state_no: 0,
            state_time: 0,
            vars: [0; NUM_VARS],
            fvars: [0.0; NUM_FVARS],
            sysvars: [0; NUM_SYSVARS],
            sysfvars: [0.0; NUM_SYSFVARS],
            constants,
            commands: Box::new(NoCommands),
            fire_counts: std::collections::HashMap::new(),
        }
    }
}

impl Character {
    /// Creates a character in MUGEN's default initial state: standing, idle, no
    /// control, full life, zero power, all variables cleared, no commands.
    ///
    /// Life and power maxima are taken from the default [`CharacterConstants`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a character seeded from the given constants (life/power maxima),
    /// starting at full life and zero power.
    #[must_use]
    pub fn with_constants(constants: CharacterConstants) -> Self {
        Self {
            life: constants.life_max,
            life_max: constants.life_max,
            power_max: constants.power_max,
            constants,
            ..Self::default()
        }
    }

    /// Replaces the command source (called by the executor to inject
    /// `fp-input`'s recognizer, or by tests to inject a synthetic set).
    pub fn set_command_source(&mut self, source: Box<dyn CommandSource>) {
        self.commands = source;
    }

    /// Reads integer variable `index`, or `0` if the index is out of range.
    #[must_use]
    fn read_var(&self, index: i32) -> i32 {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.vars.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// Reads float variable `index`, or `0.0` if the index is out of range.
    #[must_use]
    fn read_fvar(&self, index: i32) -> f32 {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.fvars.get(i))
            .copied()
            .unwrap_or(0.0)
    }

    /// Reads system integer variable `index`, or `0` if out of range.
    #[must_use]
    fn read_sysvar(&self, index: i32) -> i32 {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.sysvars.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// Reads system float variable `index`, or `0.0` if out of range.
    #[must_use]
    fn read_sysfvar(&self, index: i32) -> f32 {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.sysfvars.get(i))
            .copied()
            .unwrap_or(0.0)
    }

    /// Maps a bare letter token (`S`, `A`, …) to its stable category code, or
    /// `None` if the name is not a single recognized letter token.
    ///
    /// This is what makes `StateType = A` work: the bare `A` on the right is
    /// looked up here and the `StateType` trigger on the left returns the same
    /// code via [`StateType::code`].
    fn letter_code(name: &str) -> Option<i32> {
        let code = if name.eq_ignore_ascii_case("S") {
            CODE_S
        } else if name.eq_ignore_ascii_case("C") {
            CODE_C
        } else if name.eq_ignore_ascii_case("A") {
            CODE_A
        } else if name.eq_ignore_ascii_case("L") {
            CODE_L
        } else if name.eq_ignore_ascii_case("I") {
            CODE_I
        } else if name.eq_ignore_ascii_case("H") {
            CODE_H
        } else if name.eq_ignore_ascii_case("N") {
            CODE_N
        } else if name.eq_ignore_ascii_case("U") {
            CODE_U
        } else {
            return None;
        };
        Some(code)
    }

    /// Resolves the `Pos`/`Vel` component for an axis-coded argument.
    ///
    /// The evaluator encodes the axis suffix as `X = 0`, `Y = 1`; any other code
    /// falls back to the X component's analogue of the safe default (the X
    /// value), matching the evaluator's "malformed axis → X" lowering.
    fn axis_component(vec: Vec2<f32>, args: &[Value]) -> f32 {
        match args.first().map(|v| v.to_int()) {
            Some(AXIS_Y) => vec.y,
            Some(AXIS_X) | None => vec.x,
            Some(_) => vec.x,
        }
    }
}

impl EvalContext for Character {
    fn trigger(&self, name: &str, args: &[Value]) -> Value {
        // Helper: first arg coerced to i32 (used by indexed/axis triggers).
        let first_int = || args.first().map(|v| v.to_int());

        // Bare letter tokens (right-hand side of `StateType = A`, etc.).
        if args.is_empty() {
            if let Some(code) = Self::letter_code(name) {
                return Value::Int(code);
            }
        }

        // Time / animation / state cursors.
        if name.eq_ignore_ascii_case("Time") || name.eq_ignore_ascii_case("StateTime") {
            return Value::Int(self.state_time);
        }
        if name.eq_ignore_ascii_case("AnimTime") {
            return Value::Int(self.anim_time);
        }
        if name.eq_ignore_ascii_case("Anim") {
            return Value::Int(self.anim);
        }
        if name.eq_ignore_ascii_case("AnimElem") {
            // Bare `AnimElem` reports the current one-based element number. The
            // `AnimElem = N` form is handled by the evaluator via AnimElemTime.
            return Value::Int(self.anim_elem + 1);
        }
        if name.eq_ignore_ascii_case("AnimElemNo") {
            return Value::Int(self.anim_elem + 1);
        }
        if name.eq_ignore_ascii_case("AnimElemTime") {
            // `AnimElemTime(n)` is the time since element `n` (one-based) was
            // reached. Task 5.1 models only "time in the *current* element": if
            // the requested element is the current one, return its elapsed time;
            // otherwise the element has **not yet been reached**, which MUGEN
            // reports as a NEGATIVE value. Returning the negative sentinel
            // [`ANIM_ELEM_NOT_REACHED`] (rather than the `0` safe default) is
            // load-bearing: the VM lowers `AnimElem = N, op M` to a "reached"
            // guard of `AnimElemTime(N) >= 0`, so a not-yet-reached element must
            // read negative or the tail would spuriously fire (5.1 follow-up a).
            return match first_int() {
                Some(n) if n == self.anim_elem + 1 => Value::Int(self.anim_elem_time),
                Some(_) => Value::Int(ANIM_ELEM_NOT_REACHED),
                None => Value::DEFAULT,
            };
        }
        if name.eq_ignore_ascii_case("StateNo") {
            return Value::Int(self.state_no);
        }
        if name.eq_ignore_ascii_case("PrevStateNo") {
            return Value::Int(self.prev_state_no);
        }

        // State category triggers (return the current value's letter code).
        if name.eq_ignore_ascii_case("StateType") {
            return Value::Int(self.state_type.code());
        }
        if name.eq_ignore_ascii_case("MoveType") {
            return Value::Int(self.move_type.code());
        }
        if name.eq_ignore_ascii_case("Physics") {
            return Value::Int(self.physics.code());
        }

        // Control / resources.
        if name.eq_ignore_ascii_case("Ctrl") {
            return Value::from(self.ctrl);
        }
        if name.eq_ignore_ascii_case("Life") {
            return Value::Int(self.life);
        }
        if name.eq_ignore_ascii_case("LifeMax") {
            return Value::Int(self.life_max);
        }
        if name.eq_ignore_ascii_case("Power") {
            return Value::Int(self.power);
        }
        if name.eq_ignore_ascii_case("PowerMax") {
            return Value::Int(self.power_max);
        }
        if name.eq_ignore_ascii_case("Facing") {
            return Value::Int(self.facing.sign());
        }

        // Position / velocity by axis (X = 0, Y = 1 per the evaluator's coding).
        if name.eq_ignore_ascii_case("Pos") {
            return Value::Float(Self::axis_component(self.pos, args));
        }
        if name.eq_ignore_ascii_case("Vel") {
            return Value::Float(Self::axis_component(self.vel, args));
        }

        // Variable banks (also reachable via the typed var/fvar/sysvar methods,
        // but supported here so the `trigger` path is self-contained).
        if name.eq_ignore_ascii_case("var") {
            return match first_int() {
                Some(i) => Value::Int(self.read_var(i)),
                None => Value::DEFAULT,
            };
        }
        if name.eq_ignore_ascii_case("fvar") {
            return match first_int() {
                Some(i) => Value::Float(self.read_fvar(i)),
                None => Value::DEFAULT,
            };
        }
        if name.eq_ignore_ascii_case("sysvar") {
            return match first_int() {
                Some(i) => Value::Int(self.read_sysvar(i)),
                None => Value::DEFAULT,
            };
        }
        if name.eq_ignore_ascii_case("sysfvar") {
            return match first_int() {
                Some(i) => Value::Float(self.read_sysfvar(i)),
                None => Value::DEFAULT,
            };
        }

        // Unknown trigger → safe default, never a panic.
        Value::DEFAULT
    }

    fn var(&self, index: i32) -> Value {
        Value::Int(self.read_var(index))
    }

    fn fvar(&self, index: i32) -> Value {
        Value::Float(self.read_fvar(index))
    }

    fn sysvar(&self, index: i32) -> Value {
        Value::Int(self.read_sysvar(index))
    }

    fn trigger_str(&self, name: &str, _key: &str) -> Value {
        // GetHitVar(member): real get-hit state is Phase 6. Until then every hit
        // field reports its safe default (0). Recognize the trigger name so the
        // intent is explicit, but still return the default for every member.
        if name.eq_ignore_ascii_case("GetHitVar") {
            return Value::DEFAULT;
        }
        Value::DEFAULT
    }

    fn command_active(&self, name: &str) -> bool {
        self.commands.is_active(name)
    }

    fn redirect(&self, _target: fp_vm::Redirect) -> Option<&dyn EvalContext> {
        // Single-entity for now; multi-entity redirection is Phase 7.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_vm::{eval, parse_str};

    /// Evaluates an expression string against a character, returning the
    /// resulting [`Value`]. Panics in test code only if the expression fails to
    /// parse (a test-author error, not a runtime path).
    fn ev(expr: &str, ch: &Character) -> Value {
        let ast = parse_str(expr).expect("test expression should parse");
        eval(&ast, ch as &dyn EvalContext)
    }

    /// Builds a character with representative synthetic state for trigger tests.
    fn sample() -> Character {
        let mut ch = Character::new();
        ch.pos = Vec2::new(40.0, -12.5);
        ch.vel = Vec2::new(2.5, -7.0);
        ch.facing = Facing::Left;
        ch.life = 100;
        ch.life_max = 1000;
        ch.power = 500;
        ch.power_max = 3000;
        ch.ctrl = true;
        ch.state_type = StateType::Air;
        ch.move_type = MoveType::Attack;
        ch.physics = Physics::Air;
        ch.anim = 200;
        ch.anim_elem = 2; // one-based element 3
        ch.anim_elem_time = 4;
        ch.anim_time = -3;
        ch.state_no = 200;
        ch.prev_state_no = 0;
        ch.state_time = 0;
        ch.vars[1] = 5;
        ch.fvars[0] = 1.5;
        ch.sysvars[2] = 9;
        ch
    }

    #[test]
    fn time_and_state_cursors() {
        let ch = sample();
        assert_eq!(ev("Time = 0", &ch), Value::Int(1));
        assert_eq!(ev("StateNo = 200", &ch), Value::Int(1));
        assert_eq!(ev("PrevStateNo = 0", &ch), Value::Int(1));
        assert_eq!(ev("StateNo = 199", &ch), Value::Int(0));
    }

    #[test]
    fn animation_triggers() {
        let ch = sample();
        assert_eq!(ev("Anim = 200", &ch), Value::Int(1));
        // anim_elem is zero-based 2 → one-based element 3.
        assert_eq!(ev("AnimElem = 3", &ch), Value::Int(1));
        assert_eq!(ev("AnimElemNo = 3", &ch), Value::Int(1));
        assert_eq!(ev("AnimTime = -3", &ch), Value::Int(1));
    }

    #[test]
    fn velocity_and_position_by_axis() {
        let ch = sample();
        assert_eq!(ev("Vel Y < 0", &ch), Value::Int(1));
        assert_eq!(ev("Vel X > 0", &ch), Value::Int(1));
        assert_eq!(ev("Pos Y < 0", &ch), Value::Int(1));
        // Direct float comparison threads through the float path.
        assert_eq!(ev("Vel X = 2.5", &ch), Value::Int(1));
    }

    #[test]
    fn life_power_and_ctrl() {
        let ch = sample();
        assert_eq!(ev("Life <= 100 && ctrl", &ch), Value::Int(1));
        assert_eq!(ev("LifeMax = 1000", &ch), Value::Int(1));
        assert_eq!(ev("Power = 500", &ch), Value::Int(1));
        assert_eq!(ev("PowerMax = 3000", &ch), Value::Int(1));
        // Ctrl is boolean → int 1.
        assert_eq!(ev("Ctrl", &ch), Value::Int(1));
    }

    #[test]
    fn state_categories_via_letter_tokens() {
        let ch = sample();
        // StateType = A succeeds because both sides resolve to CODE_A.
        assert_eq!(ev("StateType = A", &ch), Value::Int(1));
        assert_eq!(ev("StateType = S", &ch), Value::Int(0));
        assert_eq!(ev("MoveType = A", &ch), Value::Int(1));
        assert_eq!(ev("MoveType = I", &ch), Value::Int(0));
        assert_eq!(ev("Physics = A", &ch), Value::Int(1));
        // Distinct categories never alias: a standing character is not air.
        let mut standing = Character::new();
        standing.state_type = StateType::Standing;
        assert_eq!(ev("StateType = S", &standing), Value::Int(1));
        assert_eq!(ev("StateType = A", &standing), Value::Int(0));
    }

    #[test]
    fn variable_banks() {
        let ch = sample();
        assert_eq!(ev("var(1) = 5", &ch), Value::Int(1));
        assert_eq!(ev("var(0) = 0", &ch), Value::Int(1));
        assert_eq!(ev("fvar(0) = 1.5", &ch), Value::Int(1));
        assert_eq!(ev("sysvar(2) = 9", &ch), Value::Int(1));
        // Out-of-range index → safe default 0, never a panic.
        assert_eq!(ev("var(999) = 0", &ch), Value::Int(1));
        assert_eq!(ev("var(-1) = 0", &ch), Value::Int(1));
    }

    #[test]
    fn command_via_injectable_source() {
        let mut ch = Character::new();
        // No source → command never active.
        assert_eq!(ev("command = \"fwd\"", &ch), Value::Int(0));
        // Inject a synthetic active-command set.
        ch.set_command_source(Box::new(ActiveCommands::from_names(["fwd", "x"])));
        assert_eq!(ev("command = \"fwd\"", &ch), Value::Int(1));
        // Case-insensitive command matching.
        assert_eq!(ev("command = \"FWD\"", &ch), Value::Int(1));
        assert_eq!(ev("command = \"back\"", &ch), Value::Int(0));
    }

    #[test]
    fn gethitvar_returns_defaults_for_now() {
        let ch = sample();
        // Real get-hit state is Phase 6; every member reports 0 for now.
        assert_eq!(ev("GetHitVar(xveladd) = 0", &ch), Value::Int(1));
        assert_eq!(ev("GetHitVar(fall.yvel) = 0", &ch), Value::Int(1));
    }

    #[test]
    fn unknown_trigger_is_safe_default() {
        let ch = sample();
        // An unknown trigger resolves to 0, so a comparison against 0 is true and
        // the engine never panics.
        assert_eq!(ev("NoSuchTrigger = 0", &ch), Value::Int(1));
    }

    #[test]
    fn case_insensitive_trigger_names() {
        let ch = sample();
        assert_eq!(ev("time = 0", &ch), Value::Int(1));
        assert_eq!(ev("STATENO = 200", &ch), Value::Int(1));
        assert_eq!(ev("LiFe <= 100", &ch), Value::Int(1));
    }

    #[test]
    fn typed_var_methods_match_trigger_path() {
        let ch = sample();
        // The typed fast paths and the `trigger` path must agree.
        assert_eq!(ch.var(1), Value::Int(5));
        assert_eq!(ch.var(1), ch.trigger("var", &[Value::Int(1)]));
        assert_eq!(ch.fvar(0), Value::Float(1.5));
        assert_eq!(ch.sysvar(2), Value::Int(9));
        // Out-of-range → default.
        assert_eq!(ch.var(60), Value::Int(0));
        assert_eq!(ch.fvar(40), Value::Float(0.0));
    }

    #[test]
    fn redirect_is_none_for_single_entity() {
        let ch = Character::new();
        assert!(ch.redirect(fp_vm::Redirect::Parent).is_none());
        assert!(ch.redirect(fp_vm::Redirect::Root).is_none());
        assert!(ch.redirect(fp_vm::Redirect::Enemy).is_none());
    }

    #[test]
    fn complex_kfm_shape_evaluates() {
        // A realistic compound trigger mixing several reads.
        let ch = sample();
        let expr = "StateType = A && Vel Y < 0 && AnimElem = 3 && ctrl";
        assert_eq!(ev(expr, &ch), Value::Int(1));
    }

    #[test]
    fn with_constants_seeds_maxima() {
        let consts = CharacterConstants {
            life_max: 1200,
            power_max: 5000,
            attack: 110,
            defence: 90,
            ..CharacterConstants::default()
        };
        let ch = Character::with_constants(consts);
        assert_eq!(ev("LifeMax = 1200", &ch), Value::Int(1));
        assert_eq!(ev("Life = 1200", &ch), Value::Int(1)); // starts at full
        assert_eq!(ev("PowerMax = 5000", &ch), Value::Int(1));
        assert_eq!(ev("Power = 0", &ch), Value::Int(1));
    }

    // =====================================================================
    // Proctor (task 5.1): edge-case, error-path, and MUGEN-semantics coverage
    // layered on top of Forge's tests. Grouped by acceptance criterion so each
    // AC is demonstrably exercised end-to-end through fp_vm::parse_str +
    // fp_vm::eval against a live Character.
    // =====================================================================

    // ---- AC1: constructors / defaults produce a sane MUGEN initial state ----

    #[test]
    fn new_matches_mugen_defaults() {
        let ch = Character::new();
        // MUGEN defaults: standing, idle, no control, full life, zero power.
        assert_eq!(ev("StateType = S", &ch), Value::Int(1));
        assert_eq!(ev("MoveType = I", &ch), Value::Int(1));
        assert_eq!(ev("Ctrl", &ch), Value::Int(0));
        assert_eq!(ev("Life = 1000", &ch), Value::Int(1));
        assert_eq!(ev("LifeMax = 1000", &ch), Value::Int(1));
        assert_eq!(ev("Power = 0", &ch), Value::Int(1));
        assert_eq!(ev("PowerMax = 3000", &ch), Value::Int(1));
        assert_eq!(ev("Time = 0", &ch), Value::Int(1));
        assert_eq!(ev("Anim = 0", &ch), Value::Int(1));
        assert_eq!(ev("StateNo = 0", &ch), Value::Int(1));
        // Default facing is Right (sign +1); Physics defaults to None.
        assert_eq!(ev("Facing = 1", &ch), Value::Int(1));
        assert_eq!(ev("Physics = N", &ch), Value::Int(1));
    }

    #[test]
    fn default_trait_equals_new() {
        // `Character::default()` and `Character::new()` describe the same entity
        // (new() is documented to defer to default()). Compare a representative
        // spread of triggers rather than deriving PartialEq (the boxed command
        // source is not comparable).
        let a = Character::new();
        let b = Character::default();
        for expr in [
            "Life", "LifeMax", "Power", "PowerMax", "StateNo", "Anim", "Time", "Facing",
        ] {
            assert_eq!(ev(expr, &a), ev(expr, &b), "default vs new disagree on {expr}");
        }
    }

    #[test]
    fn facing_left_is_negative_sign() {
        let mut ch = Character::new();
        ch.facing = Facing::Left;
        assert_eq!(ev("Facing = -1", &ch), Value::Int(1));
        assert_eq!(Facing::Left.sign(), -1);
        assert_eq!(Facing::Right.sign(), 1);
    }

    // ---- AC2: every listed standard trigger resolves correctly ----

    #[test]
    fn physics_letter_tokens() {
        let mut ch = Character::new();
        ch.physics = Physics::Stand;
        assert_eq!(ev("Physics = S", &ch), Value::Int(1));
        ch.physics = Physics::Crouch;
        assert_eq!(ev("Physics = C", &ch), Value::Int(1));
        ch.physics = Physics::Air;
        assert_eq!(ev("Physics = A", &ch), Value::Int(1));
        ch.physics = Physics::None;
        assert_eq!(ev("Physics = N", &ch), Value::Int(1));
        // A distinct physics mode does not alias another's letter.
        assert_eq!(ev("Physics = S", &ch), Value::Int(0));
    }

    #[test]
    fn statetype_lying_and_unchanged_codes_are_distinct() {
        let mut ch = Character::new();
        ch.state_type = StateType::Lying;
        assert_eq!(ev("StateType = L", &ch), Value::Int(1));
        assert_eq!(ev("StateType = A", &ch), Value::Int(0));
        ch.state_type = StateType::Unchanged;
        assert_eq!(ev("StateType = U", &ch), Value::Int(1));
        assert_eq!(ev("StateType = L", &ch), Value::Int(0));
        // MoveType unchanged is the same U code, but reading MoveType vs
        // StateType is independent.
        ch.move_type = MoveType::Unchanged;
        assert_eq!(ev("MoveType = U", &ch), Value::Int(1));
        ch.move_type = MoveType::BeingHit;
        assert_eq!(ev("MoveType = H", &ch), Value::Int(1));
    }

    #[test]
    fn letter_codes_are_mutually_distinct() {
        // The eight letter codes the engine assigns must all differ, so distinct
        // categories never alias (e.g. StateType=A must not also satisfy =S).
        let codes = [
            CODE_S, CODE_C, CODE_A, CODE_L, CODE_I, CODE_H, CODE_N, CODE_U,
        ];
        let mut seen = std::collections::HashSet::new();
        for c in codes {
            assert!(seen.insert(c), "letter code {c} is not unique");
        }
        // The enum `code()` accessors agree with the bare-letter idents.
        assert_eq!(StateType::Air.code(), CODE_A);
        assert_eq!(MoveType::Attack.code(), CODE_A);
        assert_eq!(Physics::Stand.code(), CODE_S);
    }

    #[test]
    fn anim_cursor_fields_are_independent() {
        let mut ch = Character::new();
        ch.anim = 5;
        ch.anim_elem = 0; // one-based element 1
        ch.anim_elem_time = 7;
        ch.anim_time = 12;
        assert_eq!(ev("Anim = 5", &ch), Value::Int(1));
        assert_eq!(ev("AnimElem = 1", &ch), Value::Int(1));
        assert_eq!(ev("AnimElemNo = 1", &ch), Value::Int(1));
        assert_eq!(ev("AnimTime = 12", &ch), Value::Int(1));
        // AnimElemTime(current element) reports the time within that element.
        assert_eq!(ev("AnimElemTime(1) = 7", &ch), Value::Int(1));
    }

    #[test]
    fn prev_state_no_is_separate_from_state_no() {
        let mut ch = Character::new();
        ch.state_no = 1100;
        ch.prev_state_no = 200;
        assert_eq!(ev("StateNo = 1100", &ch), Value::Int(1));
        assert_eq!(ev("PrevStateNo = 200", &ch), Value::Int(1));
        assert_eq!(ev("StateNo != PrevStateNo", &ch), Value::Int(1));
    }

    #[test]
    fn time_and_statetime_are_the_same_field() {
        let mut ch = Character::new();
        ch.state_time = 42;
        assert_eq!(ev("Time = 42", &ch), Value::Int(1));
        assert_eq!(ev("StateTime = 42", &ch), Value::Int(1));
        assert_eq!(ev("Time = StateTime", &ch), Value::Int(1));
    }

    #[test]
    fn pos_and_vel_x_axis_reads_x_component() {
        let mut ch = Character::new();
        ch.pos = Vec2::new(123.0, 456.0);
        ch.vel = Vec2::new(-1.5, 2.0);
        // Bare `Pos`/`Vel` (no axis word) defaults to the X component.
        assert_eq!(ev("Pos X = 123.0", &ch), Value::Int(1));
        assert_eq!(ev("Pos Y = 456.0", &ch), Value::Int(1));
        assert_eq!(ev("Vel X = -1.5", &ch), Value::Int(1));
        assert_eq!(ev("Vel Y = 2.0", &ch), Value::Int(1));
    }

    #[test]
    fn pos_vel_are_float_typed() {
        // `Pos`/`Vel` resolve to Value::Float even for whole-number coordinates,
        // so float arithmetic through them stays in the float domain.
        let mut ch = Character::new();
        ch.pos = Vec2::new(10.0, 20.0);
        assert!(ch.trigger("Pos", &[Value::Int(AXIS_X)]).is_float());
        assert!(ch.trigger("Vel", &[Value::Int(AXIS_Y)]).is_float());
    }

    #[test]
    fn pos_vel_z_axis_does_not_panic() {
        // The evaluator encodes a `Z` axis as code 2 (MUGEN occasionally uses 3-D
        // component triggers). The 2-D Character has no Z; the impl must fold an
        // unknown axis to a safe component rather than panic. We only assert the
        // read is well-defined (no panic) and numeric.
        let mut ch = Character::new();
        ch.pos = Vec2::new(8.0, 9.0);
        let z = ch.trigger("Pos", &[Value::Int(2)]);
        assert!(z.is_float());
        // An out-of-band axis code likewise must not panic.
        let weird = ch.trigger("Vel", &[Value::Int(99)]);
        assert!(weird.is_float());
    }

    // ---- AC2: AnimElem comma-tail (element-time comparison) form ----

    #[test]
    fn animelem_tail_current_element_compares_elem_time() {
        // `AnimElem = N, op M` lowers to "element N reached AND AnimElemTime(N)
        // op M". For the *current* element the impl returns the real elapsed
        // time, so the secondary comparison is exercised faithfully.
        let mut ch = Character::new();
        ch.anim_elem = 2; // one-based element 3
        ch.anim_elem_time = 4;
        // element 3 reached, AnimElemTime(3)=4: 4 > 2 → true.
        assert_eq!(ev("AnimElem = 3, > 2", &ch), Value::Int(1));
        // 4 = 4 → true.
        assert_eq!(ev("AnimElem = 3, = 4", &ch), Value::Int(1));
        // 4 > 10 → false (but still reached, so the tail evaluates, not panics).
        assert_eq!(ev("AnimElem = 3, > 10", &ch), Value::Int(0));
    }

    #[test]
    fn animelem_time_future_element_is_negative_sentinel() {
        // 5.1 follow-up (a): a not-yet-reached element must read NEGATIVE so the
        // VM's `AnimElem = N, op M` reached-guard (`AnimElemTime(N) >= 0`) does
        // not spuriously fire. The current element is 1 (anim_elem 0); element 5
        // is in the future.
        let mut ch = Character::new();
        ch.anim_elem = 0; // one-based element 1 is current
        ch.anim_elem_time = 3;
        // Direct trigger read: future element reports a negative value.
        assert!(
            ch.trigger("AnimElemTime", &[Value::Int(5)]).to_int() < 0,
            "AnimElemTime for a future element must be negative"
        );
        // End-to-end through the VM: `AnimElem = 5, >= 0` must NOT fire because
        // element 5 has not been reached (the reached-guard sees a negative
        // element time). The tail evaluates to Value::Int(0), never panics.
        assert_eq!(ev("AnimElem = 5, >= 0", &ch), Value::Int(0));
        // The current element (1) IS reached, so `AnimElem = 1, >= 0` fires.
        assert_eq!(ev("AnimElem = 1, >= 0", &ch), Value::Int(1));
    }

    #[test]
    fn variable_banks_are_typed_correctly() {
        let mut ch = Character::new();
        ch.vars[7] = -3;
        ch.fvars[9] = -2.5;
        ch.sysvars[1] = 4;
        ch.sysfvars[3] = 6.25;
        // Int banks resolve to Int, float banks to Float.
        assert!(ch.trigger("var", &[Value::Int(7)]).is_int());
        assert!(ch.trigger("fvar", &[Value::Int(9)]).is_float());
        assert_eq!(ev("var(7) = -3", &ch), Value::Int(1));
        assert_eq!(ev("fvar(9) = -2.5", &ch), Value::Int(1));
        assert_eq!(ev("sysvar(1) = 4", &ch), Value::Int(1));
        assert_eq!(ev("sysfvar(3) = 6.25", &ch), Value::Int(1));
    }

    #[test]
    fn variable_bank_boundary_indices() {
        let mut ch = Character::new();
        // Highest valid indices in each bank.
        ch.vars[NUM_VARS - 1] = 11;
        ch.fvars[NUM_FVARS - 1] = 22.0;
        ch.sysvars[NUM_SYSVARS - 1] = 33;
        ch.sysfvars[NUM_SYSFVARS - 1] = 44.0;
        assert_eq!(ev("var(59) = 11", &ch), Value::Int(1));
        assert_eq!(ev("fvar(39) = 22.0", &ch), Value::Int(1));
        assert_eq!(ev("sysvar(4) = 33", &ch), Value::Int(1));
        assert_eq!(ev("sysfvar(4) = 44.0", &ch), Value::Int(1));
        // One past the end of each bank → safe default, never a panic.
        assert_eq!(ch.var(NUM_VARS as i32), Value::Int(0));
        assert_eq!(ch.fvar(NUM_FVARS as i32), Value::Float(0.0));
        assert_eq!(ch.sysvar(NUM_SYSVARS as i32), Value::Int(0));
        assert_eq!(
            ch.trigger("sysfvar", &[Value::Int(NUM_SYSFVARS as i32)]),
            Value::Float(0.0)
        );
    }

    // ---- AC3: case-insensitivity and safe defaults across the surface ----

    #[test]
    fn axis_words_are_case_insensitive() {
        let mut ch = Character::new();
        ch.vel = Vec2::new(3.0, -4.0);
        assert_eq!(ev("vel x = 3.0", &ch), Value::Int(1));
        assert_eq!(ev("VEL Y = -4.0", &ch), Value::Int(1));
        assert_eq!(ev("Vel y < 0", &ch), Value::Int(1));
    }

    #[test]
    fn letter_tokens_are_case_insensitive() {
        let mut ch = Character::new();
        ch.state_type = StateType::Crouching;
        assert_eq!(ev("StateType = c", &ch), Value::Int(1));
        assert_eq!(ev("statetype = C", &ch), Value::Int(1));
    }

    #[test]
    fn var_fvar_sysvar_trigger_names_case_insensitive() {
        let mut ch = Character::new();
        ch.vars[2] = 8;
        ch.fvars[2] = 8.0;
        ch.sysvars[0] = 8;
        assert_eq!(ev("VAR(2) = 8", &ch), Value::Int(1));
        assert_eq!(ev("FVar(2) = 8.0", &ch), Value::Int(1));
        assert_eq!(ev("SYSVAR(0) = 8", &ch), Value::Int(1));
    }

    #[test]
    fn unknown_trigger_with_args_is_safe_default() {
        let ch = sample();
        // An unknown *parameterized* trigger also resolves to the default, not a
        // panic — covers the args branch of the unknown path.
        assert_eq!(ev("BogusFn(3) = 0", &ch), Value::Int(1));
        // Unknown trigger is falsey on its own (default 0).
        assert_eq!(ev("BogusTrigger", &ch), Value::Int(0));
    }

    #[test]
    fn var_with_missing_argument_is_safe_default() {
        // A bare `var` with no argument (malformed content) must not panic; the
        // impl returns the safe default for the missing-arg case.
        let ch = sample();
        assert_eq!(ch.trigger("var", &[]), Value::DEFAULT);
        assert_eq!(ch.trigger("fvar", &[]), Value::DEFAULT);
        assert_eq!(ch.trigger("sysvar", &[]), Value::DEFAULT);
        assert_eq!(ch.trigger("sysfvar", &[]), Value::DEFAULT);
        assert_eq!(ch.trigger("AnimElemTime", &[]), Value::DEFAULT);
    }

    #[test]
    fn out_of_range_var_indices_never_panic() {
        let ch = sample();
        // Extreme and negative indices on every bank → 0, no panic.
        for idx in [i32::MIN, -1, 1000, i32::MAX] {
            assert_eq!(ch.var(idx), Value::Int(0));
            assert_eq!(ch.sysvar(idx), Value::Int(0));
            assert_eq!(ch.fvar(idx), Value::Float(0.0));
        }
    }

    // ---- AC2: GetHitVar routes through trigger_str, defaults for now ----

    #[test]
    fn gethitvar_members_all_default_via_trigger_str() {
        let ch = sample();
        // Real get-hit state is Phase 6; every named member reports 0 for now,
        // routed through the string-keyed seam (not the numeric path).
        for member in ["fall.yvel", "xveladd", "yveladd", "animtype", "fall", "ground.velocity"] {
            assert_eq!(
                ch.trigger_str("GetHitVar", member),
                Value::DEFAULT,
                "GetHitVar({member}) should default to 0"
            );
        }
        // Case-insensitive trigger name.
        assert_eq!(ch.trigger_str("gethitvar", "fall.yvel"), Value::DEFAULT);
        // An unrecognized member-keyed trigger name also defaults.
        assert_eq!(ch.trigger_str("NotAMemberTrigger", "x"), Value::DEFAULT);
        // End-to-end through the evaluator (which routes GetHitVar to trigger_str).
        assert_eq!(ev("GetHitVar(animtype) = 0", &ch), Value::Int(1));
    }

    // ---- AC2/AC4: command source seam ----

    #[test]
    fn no_commands_source_reports_nothing_active() {
        let src = NoCommands;
        assert!(!src.is_active("fwd"));
        assert!(!src.is_active(""));
        let ch = Character::new();
        // Default source is NoCommands.
        assert!(!ch.command_active("anything"));
        assert_eq!(ev("command = \"anything\"", &ch), Value::Int(0));
    }

    #[test]
    fn active_commands_builders_and_mutation() {
        // from_names + case-insensitive matching.
        let mut src = ActiveCommands::from_names(["Fwd", "QCF_x"]);
        assert!(src.is_active("fwd"));
        assert!(src.is_active("qcf_x"));
        assert!(!src.is_active("back"));
        // insert adds a command for the tick.
        src.insert("back");
        assert!(src.is_active("BACK"));
        // clear empties the set (start-of-tick reset).
        src.clear();
        assert!(!src.is_active("fwd"));
        assert!(!src.is_active("back"));
        // new() starts empty.
        let empty = ActiveCommands::new();
        assert!(!empty.is_active("fwd"));
    }

    #[test]
    fn command_negation_and_replacement() {
        let mut ch = Character::new();
        ch.set_command_source(Box::new(ActiveCommands::from_names(["fwd"])));
        assert_eq!(ev("command = \"fwd\"", &ch), Value::Int(1));
        // `!=` form: the command IS active, so `command != "fwd"` is false.
        assert_eq!(ev("command != \"fwd\"", &ch), Value::Int(0));
        assert_eq!(ev("command != \"back\"", &ch), Value::Int(1));
        // Swapping the source replaces the active set entirely.
        ch.set_command_source(Box::new(ActiveCommands::from_names(["back"])));
        assert_eq!(ev("command = \"fwd\"", &ch), Value::Int(0));
        assert_eq!(ev("command = \"back\"", &ch), Value::Int(1));
    }

    // ---- AC3: redirect returns None for every target (single entity) ----

    #[test]
    fn redirect_is_none_for_all_targets() {
        let ch = sample();
        for target in [
            fp_vm::Redirect::Parent,
            fp_vm::Redirect::Root,
            fp_vm::Redirect::Helper(1234),
            fp_vm::Redirect::Target(None),
            fp_vm::Redirect::Target(Some(2)),
            fp_vm::Redirect::Enemy,
            fp_vm::Redirect::EnemyNear(0),
            fp_vm::Redirect::EnemyNear(3),
            fp_vm::Redirect::Partner,
            fp_vm::Redirect::PlayerId(7),
        ] {
            assert!(ch.redirect(target).is_none(), "{target:?} should be None");
        }
    }

    #[test]
    fn redirected_expression_never_fires() {
        // A redirected trigger (e.g. `parent, life`) resolves to bottom → 0
        // because redirect() is None; it must never fire and never panic.
        let ch = sample();
        assert_eq!(ev("parent, Life", &ch), Value::Int(0));
        assert_eq!(ev("enemy, StateNo = 200", &ch), Value::Int(0));
        // A redirection binds looser than every operator, so `root, ...` retargets
        // the whole trailing compound; with no root entity it collapses to 0 even
        // though the same triggers are true locally.
        assert_eq!(ev("root, StateNo = 200 && ctrl", &ch), Value::Int(0));
    }

    // ---- AC4: MUGEN range-literal trigger forms against live state ----

    #[test]
    fn range_literal_triggers() {
        let ch = sample(); // life=100, var(1)=5, state_no=200
        // Inclusive range: 100 is within [1,1000].
        assert_eq!(ev("Life = [1,1000]", &ch), Value::Int(1));
        // Exclusive lower bound excludes the endpoint.
        assert_eq!(ev("var(1) = (5,10]", &ch), Value::Int(0));
        assert_eq!(ev("var(1) = [5,10]", &ch), Value::Int(1));
        // Out of range.
        assert_eq!(ev("StateNo = [0,100]", &ch), Value::Int(0));
        // `!=` range: StateNo (200) is NOT in [0,100], so the negated range fires.
        assert_eq!(ev("StateNo != [0,100]", &ch), Value::Int(1));
    }

    // ---- AC4: representative compound KFM-style trigger expressions ----

    #[test]
    fn realistic_kfm_guard_triggers() {
        // Shapes drawn from real KFM common-state trigger expressions.
        let mut ch = Character::new();
        ch.state_type = StateType::Standing;
        ch.move_type = MoveType::Idle;
        ch.ctrl = true;
        ch.state_no = 0;
        // Walk-forward guard: standing, idle, has control, command pressed.
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdfwd"])));
        let walk = "StateType = S && ctrl && command = \"holdfwd\"";
        assert_eq!(ev(walk, &ch), Value::Int(1));

        // Jump-land transition: in the air, falling, animation finished.
        let mut air = Character::new();
        air.state_type = StateType::Air;
        air.vel = Vec2::new(0.0, 5.0); // moving downward (Y down)
        air.pos = Vec2::new(0.0, 0.0); // at ground level
        air.anim_time = 0;
        let land = "StateType = A && Vel Y > 0 && Pos Y >= 0";
        assert_eq!(ev(land, &air), Value::Int(1));
    }

    #[test]
    fn negative_and_zero_resource_values() {
        // Life can be driven to 0 by gameplay; triggers must still read it.
        let mut ch = Character::new();
        ch.life = 0;
        ch.power = 0;
        assert_eq!(ev("Life = 0", &ch), Value::Int(1));
        assert_eq!(ev("Life <= 0", &ch), Value::Int(1));
        assert_eq!(ev("Power < 1", &ch), Value::Int(1));
    }

    #[test]
    fn typed_paths_and_trigger_paths_agree_for_all_banks() {
        let mut ch = Character::new();
        ch.vars[5] = 51;
        ch.fvars[5] = 5.5;
        ch.sysvars[3] = 31;
        // The typed EvalContext methods and the `trigger` string path must agree.
        assert_eq!(ch.var(5), ch.trigger("var", &[Value::Int(5)]));
        assert_eq!(ch.fvar(5), ch.trigger("fvar", &[Value::Int(5)]));
        assert_eq!(ch.sysvar(3), ch.trigger("sysvar", &[Value::Int(3)]));
        assert_eq!(ch.var(5), Value::Int(51));
        assert_eq!(ch.fvar(5), Value::Float(5.5));
        assert_eq!(ch.sysvar(3), Value::Int(31));
    }

    // ---- AC2: trigger names containing the bare-letter tokens are not shadowed
    // by the letter-ident handling when arguments are present ----

    #[test]
    fn bare_letter_idents_only_resolve_without_args() {
        // The letter-token shortcut only applies to argument-less idents. A call
        // like `A(1)` (nonsensical, but possible content) must not be mistaken
        // for the air code; it falls through to the unknown-trigger default.
        let ch = sample();
        assert_eq!(ch.trigger("A", &[]), Value::Int(CODE_A));
        assert_eq!(ch.trigger("A", &[Value::Int(1)]), Value::DEFAULT);
    }

    // ---- AC5: optional real-content fixture, gated to skip when absent ----
    //
    // Task 5.1 does not load files (the .cns/.air loader is task 5.2), so there
    // is no binary fixture to parse here. To still exercise "real KFM trigger
    // expressions end-to-end" against a file when one is provided, we read a
    // newline-separated list of trigger expressions from
    // `test-assets/kfm-triggers.txt` (each line: `<expr>` expected to be TRUE
    // against the `sample()` character) and evaluate each. The test SKIPS
    // cleanly (returns early) when the asset directory/file is absent, so the
    // suite stays green in a checkout without test-assets.

    #[test]
    fn real_fixture_trigger_expressions_when_present() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test-assets/kfm-triggers.txt");
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            // Asset absent → skip cleanly (not a failure).
            Err(_) => return,
        };
        let ch = sample();
        for (lineno, raw) in contents.lines().enumerate() {
            let expr = raw.trim();
            if expr.is_empty() || expr.starts_with('#') {
                continue; // blank / comment line
            }
            let value = ev(expr, &ch);
            assert_eq!(
                value,
                Value::Int(1),
                "fixture line {} `{}` should evaluate TRUE, got {:?}",
                lineno + 1,
                expr,
                value
            );
        }
    }
}
