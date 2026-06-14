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

pub mod combat;
pub mod executor;
pub mod loader;

pub use combat::{resolve_attack, AttackResolution};
pub use executor::{SoundRequest, TickReport};
pub use loader::{
    CompiledController, CompiledExpr, CompiledParam, CompiledState, CompiledTriggerGroup,
    LoadedCharacter,
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

/// The numeric hit-effect variables a **defender** reads about the last hit it
/// took, exposed to triggers via MUGEN's `GetHitVar(<member>)` redirection.
///
/// In MUGEN, when a `HitDef` connects, the engine copies the resolved hit
/// parameters onto the defender so its get-hit states (`5000`–`5xxx`) can read
/// them back with `GetHitVar(xvel)`, `GetHitVar(yvel)`, `GetHitVar(fall)`, etc.
/// Every field is numeric (MUGEN's `GetHitVar` only ever yields a number), so a
/// plain `i32`/`f32` record models it exactly.
///
/// This struct is **pure data**: it never runs game logic. It defaults to all
/// zeros (no hit taken yet); populating it from a connecting [`fp_combat::HitDef`]
/// is the job of hit *resolution* (task 6.3). [`GetHitVars::member`] resolves a
/// member name (case-insensitive) to the matching field, returning the safe
/// default ([`Value::DEFAULT`]) for any unknown member. The character exposes
/// this to triggers as `GetHitVar(<member>)`.
///
/// # Examples
///
/// ```
/// use fp_character::GetHitVars;
///
/// let mut g = GetHitVars::default();
/// assert_eq!(g.damage, 0); // nothing taken yet
/// g.damage = 30;
/// g.xvel = -4.0;
/// assert_eq!(g.damage, 30);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GetHitVars {
    /// `GetHitVar(xvel)` — X velocity imparted by the hit (pixels/tick).
    pub xvel: f32,
    /// `GetHitVar(yvel)` — Y velocity imparted by the hit (pixels/tick).
    pub yvel: f32,
    /// `GetHitVar(yaccel)` — Y acceleration applied while in hit-stun.
    pub yaccel: f32,
    /// `GetHitVar(type)` — ground hit-reaction type code (`0` = none).
    pub hit_type: i32,
    /// `GetHitVar(animtype)` — hit-reaction animation type code.
    pub animtype: i32,
    /// `GetHitVar(damage)` — damage dealt by the hit.
    pub damage: i32,
    /// `GetHitVar(hitcount)` — running hit count of the current combo.
    pub hitcount: i32,
    /// `GetHitVar(fall)` — non-zero if the hit knocked the defender into a fall.
    pub fall: i32,
    /// `GetHitVar(hitshaketime)` — remaining hit-shake (pause) ticks.
    pub hitshaketime: i32,
    /// `GetHitVar(hittime)` — remaining hit-stun ticks.
    pub hittime: i32,
    /// `GetHitVar(slidetime)` — remaining ground-slide ticks.
    pub slidetime: i32,
    /// `GetHitVar(ctrltime)` — remaining ticks before control returns.
    pub ctrltime: i32,
    /// `GetHitVar(isbound)` — non-zero while the defender is in a bound state.
    pub isbound: i32,
    /// `GetHitVar(guarded)` — non-zero if the hit was guarded (blocked).
    pub guarded: i32,
    /// `GetHitVar(chainid)` — the chain id of the hit (`-1` = none / any).
    pub chainid: i32,
}

impl Default for GetHitVars {
    /// All fields default to `0`, except [`chainid`](GetHitVars::chainid) which
    /// defaults to MUGEN's `-1` ("no chain") sentinel. A freshly-defaulted
    /// `GetHitVars` describes "no hit taken".
    fn default() -> Self {
        Self {
            xvel: 0.0,
            yvel: 0.0,
            yaccel: 0.0,
            hit_type: 0,
            animtype: 0,
            damage: 0,
            hitcount: 0,
            fall: 0,
            hitshaketime: 0,
            hittime: 0,
            slidetime: 0,
            ctrltime: 0,
            isbound: 0,
            guarded: 0,
            chainid: -1,
        }
    }
}

impl GetHitVars {
    /// Resolves a `GetHitVar` member name to its value as an [`fp_vm::Value`].
    ///
    /// `member` is matched **case-insensitively** against the MUGEN member names
    /// (`xvel`, `yvel`, `yaccel`, `type`, `animtype`, `damage`, `hitcount`,
    /// `fall`, `hitshaketime`, `hittime`, `slidetime`, `ctrltime`, `isbound`,
    /// `guarded`, `chainid`). Float-typed members yield a [`Value::Float`];
    /// integer-typed members yield a [`Value::Int`]. An **unknown** member
    /// resolves to [`Value::DEFAULT`] (`0`) — never a panic — matching MUGEN's
    /// tolerance of unmodeled redirections.
    #[must_use]
    pub fn member(&self, member: &str) -> Value {
        let m = member.trim();
        // Float-typed members.
        if m.eq_ignore_ascii_case("xvel") {
            Value::Float(self.xvel)
        } else if m.eq_ignore_ascii_case("yvel") {
            Value::Float(self.yvel)
        } else if m.eq_ignore_ascii_case("yaccel") {
            Value::Float(self.yaccel)
        // Integer-typed members.
        } else if m.eq_ignore_ascii_case("type") {
            Value::Int(self.hit_type)
        } else if m.eq_ignore_ascii_case("animtype") {
            Value::Int(self.animtype)
        } else if m.eq_ignore_ascii_case("damage") {
            Value::Int(self.damage)
        } else if m.eq_ignore_ascii_case("hitcount") {
            Value::Int(self.hitcount)
        } else if m.eq_ignore_ascii_case("fall") {
            Value::Int(self.fall)
        } else if m.eq_ignore_ascii_case("hitshaketime") {
            Value::Int(self.hitshaketime)
        } else if m.eq_ignore_ascii_case("hittime") {
            Value::Int(self.hittime)
        } else if m.eq_ignore_ascii_case("slidetime") {
            Value::Int(self.slidetime)
        } else if m.eq_ignore_ascii_case("ctrltime") {
            Value::Int(self.ctrltime)
        } else if m.eq_ignore_ascii_case("isbound") {
            Value::Int(self.isbound)
        } else if m.eq_ignore_ascii_case("guarded") {
            Value::Int(self.guarded)
        } else if m.eq_ignore_ascii_case("chainid") {
            Value::Int(self.chainid)
        } else {
            // Unknown member: safe default (0), debug-logged (not warn — many
            // GetHitVar members are unmodeled and that is not an error).
            tracing::debug!("GetHitVar: unknown member {member:?} -> 0");
            Value::DEFAULT
        }
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
    /// Whether this character is currently holding the "back" direction (away
    /// from the opponent), i.e. attempting to guard.
    ///
    /// Hit resolution ([`resolve_attack`]) reads this on the **defender** to
    /// build the [`fp_combat::DefenderState`] that decides hit-vs-guard. The
    /// executor sets it from `fp-input` once cross-entity facing is wired
    /// (Phase 7); until then it stays `false` (attacks land rather than block),
    /// matching the task's "else false for now" rule. Tests set it directly to
    /// drive the guard path.
    pub holding_back: bool,

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

    // ---- Combat (Phase 6) -------------------------------------------------
    /// The character's currently-active [`fp_combat::HitDef`], if any.
    ///
    /// The `HitDef` state controller (task 6.2) evaluates the character's
    /// expressions and stores the resolved attack description here; hit
    /// *resolution* (task 6.3) reads it to decide whether and how a contact
    /// damages the defender. `None` means the character is not currently
    /// offering a hit. MUGEN keeps a single active `HitDef` per player; a later
    /// `HitDef` controller overwrites an earlier one in the same tick.
    pub active_hitdef: Option<fp_combat::HitDef>,
    /// The hit-effect variables describing the **last hit this character took**,
    /// read by its get-hit states via `GetHitVar(<member>)`.
    ///
    /// Defaults to [`GetHitVars::default`] ("no hit taken"). Population from a
    /// connecting attack's [`fp_combat::HitDef`] happens in hit resolution
    /// (task 6.3); until then it stays at its default and every `GetHitVar`
    /// member reads back its default.
    pub get_hit_vars: GetHitVars,

    /// Remaining hit-pause / hit-stop ticks for **this** character.
    ///
    /// MUGEN freezes both participants for a few ticks when an attack connects
    /// (the "hit stop") so the impact reads. While `hitpause > 0` the executor
    /// skips normal state processing for this character and decrements the
    /// counter by one each tick (see [`Character::tick`]). Set on both attacker
    /// (from `pausetime.p1`) and defender (from `pausetime.p2`/`shaketime`) by
    /// hit resolution ([`resolve_attack`]). `0` means "not paused".
    pub hitpause: i32,

    /// Remaining hit-shake ticks for **this** character (the defender's visual
    /// shake during hit-pause), read by `GetHitVar(hitshaketime)` and the
    /// `HitShakeOver` trigger. Set from the connecting hit's `pausetime.p2`.
    pub shaketime: i32,

    /// Connection state of this character's currently-active move, for the
    /// `MoveContact` / `MoveHit` / `MoveGuarded` triggers and the `hitonce`
    /// (numhits = 1) rule.
    ///
    /// Updated by hit resolution ([`resolve_attack`]) on the **attacker** when
    /// its [`active_hitdef`](Character::active_hitdef) connects, and reset when a
    /// new move begins. See [`MoveConnect`].
    pub move_connect: MoveConnect,
}

/// Tracks whether the attacker's current move has connected, for the
/// `MoveContact` / `MoveHit` / `MoveGuarded` triggers and the `hitonce` rule.
///
/// MUGEN exposes three related triggers an attacker reads about its *own*
/// in-progress move:
///
/// - `MoveHit` — the move landed as a **clean hit** at least once.
/// - `MoveGuarded` — the move was **blocked** at least once.
/// - `MoveContact` — the move made contact at all (`MoveHit || MoveGuarded`).
///
/// Hit resolution ([`resolve_attack`]) sets [`hit`](MoveConnect::hit) /
/// [`guarded`](MoveConnect::guarded) when the attacker's
/// [`active_hitdef`](Character::active_hitdef) connects. The same `hit ||
/// guarded` flag also enforces `hitonce` (`numhits = 1`): once a move has
/// connected, [`resolve_attack`] will not let it connect again until the move is
/// reset with [`MoveConnect::reset`] (called on a fresh `HitDef`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MoveConnect {
    /// `true` once the current move landed a clean hit (`MoveHit`).
    pub hit: bool,
    /// `true` once the current move was guarded/blocked (`MoveGuarded`).
    pub guarded: bool,
}

impl MoveConnect {
    /// `MoveContact`: the move made contact at all (hit **or** guarded).
    #[must_use]
    pub const fn contact(self) -> bool {
        self.hit || self.guarded
    }

    /// Clears all connection flags — called when a new move (a fresh `HitDef`)
    /// begins, so `MoveContact`/`MoveHit`/`MoveGuarded` and `hitonce` start over.
    pub fn reset(&mut self) {
        self.hit = false;
        self.guarded = false;
    }
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
            holding_back: false,
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
            active_hitdef: None,
            get_hit_vars: GetHitVars::default(),
            hitpause: 0,
            shaketime: 0,
            move_connect: MoveConnect::default(),
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

    /// Resolves a MUGEN `const(<member>)` read against this character's loaded
    /// [`CharacterConstants`].
    ///
    /// `member` is the dotted member name exactly as written inside `const(...)`
    /// (e.g. `velocity.walk.fwd.x`, `size.ground.front`, `movement.yaccel`); the
    /// match is **case-insensitive on the full dotted name**, mirroring MUGEN's
    /// case-insensitive constant lookup. The members the KFM/common states read
    /// are mapped to the sub-structs added in task 5.3:
    ///
    /// - `velocity.walk.fwd.x` / `velocity.walk.back.x`
    /// - `velocity.run.fwd.x` / `velocity.run.fwd.y`
    /// - `velocity.jump.neu.x` / `velocity.jump.y`
    /// - `size.ground.front` / `size.ground.back` / `size.height`
    /// - `movement.yaccel` / `movement.stand.friction` / `movement.crouch.friction`
    /// - `data.life` / `data.power` / `data.attack` / `data.defence`
    ///
    /// An **unknown** member resolves to [`Value::DEFAULT`] (`0`) and is logged
    /// at `debug` level — never `warn`, since unmodeled `const` members are
    /// common in community content and not an error. This never panics.
    fn resolve_const(&self, member: &str) -> Value {
        let c = &self.constants;
        let m = member.trim();

        // Integer-typed members (`[Data]` and `[Size]`).
        if m.eq_ignore_ascii_case("data.life") {
            return Value::Int(c.life_max);
        }
        if m.eq_ignore_ascii_case("data.power") {
            return Value::Int(c.power_max);
        }
        if m.eq_ignore_ascii_case("data.attack") {
            return Value::Int(c.attack);
        }
        if m.eq_ignore_ascii_case("data.defence") {
            return Value::Int(c.defence);
        }
        if m.eq_ignore_ascii_case("size.ground.front") {
            return Value::Int(c.size.ground_front);
        }
        if m.eq_ignore_ascii_case("size.ground.back") {
            return Value::Int(c.size.ground_back);
        }
        if m.eq_ignore_ascii_case("size.height") {
            return Value::Int(c.size.height);
        }

        // Float-typed members (`[Velocity]` and `[Movement]`).
        if m.eq_ignore_ascii_case("velocity.walk.fwd.x") {
            return Value::Float(c.velocity.walk_fwd.x);
        }
        if m.eq_ignore_ascii_case("velocity.walk.back.x") {
            return Value::Float(c.velocity.walk_back.x);
        }
        if m.eq_ignore_ascii_case("velocity.run.fwd.x") {
            return Value::Float(c.velocity.run_fwd.x);
        }
        if m.eq_ignore_ascii_case("velocity.run.fwd.y") {
            return Value::Float(c.velocity.run_fwd.y);
        }
        if m.eq_ignore_ascii_case("velocity.jump.neu.x") {
            return Value::Float(c.velocity.jump_neu.x);
        }
        if m.eq_ignore_ascii_case("velocity.jump.y") {
            return Value::Float(c.velocity.jump_up);
        }
        if m.eq_ignore_ascii_case("movement.yaccel") {
            return Value::Float(c.movement.yaccel);
        }
        if m.eq_ignore_ascii_case("movement.stand.friction") {
            return Value::Float(c.movement.stand_friction);
        }
        if m.eq_ignore_ascii_case("movement.crouch.friction") {
            return Value::Float(c.movement.crouch_friction);
        }

        // Unknown member: safe default 0. `debug!` (not `warn!`) because unmodeled
        // const members are common and benign in community content.
        tracing::debug!("const({member}): unmodeled member, defaulting to 0");
        Value::DEFAULT
    }

    /// Resolves a MUGEN `GetHitVar(<member>)` read against this character's
    /// [`get_hit_vars`](Character::get_hit_vars).
    ///
    /// `member` is the name written inside `GetHitVar(...)` (case-insensitive).
    /// This delegates to [`GetHitVars::member`]: a known member returns its
    /// field's value (float- or int-typed), an unknown member returns
    /// [`Value::DEFAULT`] (`0`). Never panics.
    ///
    /// Populating the underlying [`GetHitVars`] is hit resolution's job
    /// (task 6.3); until then every member reads back its default.
    #[must_use]
    fn get_hit_var(&self, member: &str) -> Value {
        self.get_hit_vars.member(member)
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

        // Liveness. `alive` is true while the character has any life left. The
        // stock `common1.cns` stand state gates a `ChangeState` to the death
        // state (5050) on `trigger1 = !alive`; without this arm `alive` would
        // hit the unknown-trigger default of `0`, making `!alive` evaluate true
        // and dropping a full-life KFM into the death state on tick 1. This is
        // trivially correct from the `Life` we already model.
        if name.eq_ignore_ascii_case("alive") {
            return Value::from(self.life > 0);
        }

        // Move-connection triggers an attacker reads about its own in-progress
        // move. Populated by hit resolution (`resolve_attack`) on the attacker.
        // `MoveContact` is `MoveHit || MoveGuarded`.
        if name.eq_ignore_ascii_case("MoveHit") {
            return Value::from(self.move_connect.hit);
        }
        if name.eq_ignore_ascii_case("MoveGuarded") {
            return Value::from(self.move_connect.guarded);
        }
        if name.eq_ignore_ascii_case("MoveContact") {
            return Value::from(self.move_connect.contact());
        }
        // `HitShakeOver` is true once the defender's hit-shake has elapsed.
        if name.eq_ignore_ascii_case("HitShakeOver") {
            return Value::from(self.shaketime <= 0);
        }

        // ---- Deferred triggers (documented, not silently wrong) -------------
        //
        // The following standard triggers appear in the stock `kfm.cns` /
        // `common1.cns` but genuinely require engine context this crate does not
        // yet model. They are intentionally NOT special-cased here: each falls
        // through to the unknown-trigger default of `0` below, which is the same
        // value MUGEN would report when the corresponding state is absent, so
        // the common states do not actively misfire on them today. They are
        // listed here so the omission is explicit rather than accidental.
        //
        // * Get-hit state (Phase 6): `HitOver`, `HitFall`, `CanRecover`,
        //   `InGuardDist`. These read the active get-hit record this crate does
        //   not fully model yet. (`MoveContact`/`MoveHit`/`MoveGuarded` and
        //   `HitShakeOver` are now answered above from the fields hit resolution
        //   populates.) `GetHitVar(...)` is handled in `trigger_str`.
        // * Round / match state (engine, Phase 5+): `RoundState`, `RoundNo`,
        //   `RoundsExisted`, `MatchOver`, `GameTime`. These live on the round
        //   coordinator (`fp-engine`), not on a single `Character`.
        // * Cross-entity geometry (Phase 7 redirection): `P2BodyDist`,
        //   `P2Dist`, `FrontEdgeBodyDist`, `BackEdgeBodyDist`, `BackEdgeDist`.
        //   These need the opponent and stage, reached via `redirect` (which is
        //   currently single-entity / `None`).
        // * Animation table queries: `SelfAnimExist`. Needs the loaded `.air`
        //   action set, which the executor owns rather than `Character`.

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

    fn trigger_str(&self, name: &str, key: &str) -> Value {
        // const(member): read the character's authored constant by name. Since
        // task 5.6d the VM routes `const(<member>)` here with the dotted member
        // name verbatim in `key`. See [`Character::resolve_const`].
        if name.eq_ignore_ascii_case("const") {
            return self.resolve_const(key);
        }

        // GetHitVar(member): resolve the member against this character's
        // get-hit variables (task 6.2). Population of those variables from a
        // connecting hit is task 6.3; until then every field reads its default,
        // but an unknown member still resolves to the safe default (0).
        if name.eq_ignore_ascii_case("GetHitVar") {
            return self.get_hit_var(key);
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
    fn alive_tracks_life() {
        let mut ch = sample();

        // Full life: alive is 1 and `!alive` is 0, both via the typed path and
        // through a parsed expression (case-insensitive trigger name).
        ch.life = ch.life_max;
        assert_eq!(ch.trigger("alive", &[]), Value::Int(1));
        assert_eq!(ch.trigger("ALIVE", &[]), Value::Int(1));
        assert_eq!(ev("alive", &ch), Value::Int(1));
        assert_eq!(ev("!alive", &ch), Value::Int(0));

        // Any positive life still counts as alive.
        ch.life = 1;
        assert_eq!(ev("alive", &ch), Value::Int(1));
        assert_eq!(ev("!alive", &ch), Value::Int(0));

        // Zero life: alive flips to 0 and `!alive` (the common1.cns death gate)
        // becomes true.
        ch.life = 0;
        assert_eq!(ch.trigger("alive", &[]), Value::Int(0));
        assert_eq!(ev("alive", &ch), Value::Int(0));
        assert_eq!(ev("!alive", &ch), Value::Int(1));
    }

    #[test]
    fn deferred_triggers_stay_at_safe_default() {
        // Triggers documented as deferred (need get-hit / round / cross-entity
        // context not yet modeled) must still resolve to the safe default of 0,
        // never panic. This pins the documented behavior so a future
        // implementation is a deliberate change, not an accidental one.
        let ch = sample();
        for t in [
            "HitOver",
            "HitFall",
            "CanRecover",
            "InGuardDist",
            "RoundState",
            "GameTime",
            "MatchOver",
            "SelfAnimExist",
        ] {
            assert_eq!(
                ch.trigger(t, &[]),
                Value::DEFAULT,
                "deferred trigger {t} should default to 0"
            );
        }
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
    fn gethitvar_unknown_member_is_zero() {
        let ch = sample();
        // Unknown / unmodeled members report 0 (the safe default).
        assert_eq!(ev("GetHitVar(xveladd) = 0", &ch), Value::Int(1));
        assert_eq!(ev("GetHitVar(fall.yvel) = 0", &ch), Value::Int(1));
        // Resolved directly, too.
        assert_eq!(ch.trigger_str("GetHitVar", "nosuchmember"), Value::DEFAULT);
        assert_eq!(ch.trigger_str("GetHitVar", ""), Value::DEFAULT);
    }

    #[test]
    fn gethitvar_resolves_populated_fields() {
        // Populate the defender's get-hit variables and read them back through
        // the trigger surface. (Population is normally task 6.3; here we set the
        // fields directly to exercise the read path.)
        let mut ch = sample();
        ch.get_hit_vars = GetHitVars {
            xvel: -4.0,
            yvel: -7.5,
            yaccel: 0.7,
            hit_type: 1,
            animtype: 2,
            damage: 33,
            hitcount: 3,
            fall: 1,
            hitshaketime: 6,
            hittime: 14,
            slidetime: 8,
            ctrltime: 12,
            isbound: 1,
            guarded: 0,
            chainid: 5,
        };
        // Integer members.
        assert_eq!(ch.trigger_str("GetHitVar", "damage"), Value::Int(33));
        assert_eq!(ch.trigger_str("GetHitVar", "hittime"), Value::Int(14));
        assert_eq!(ch.trigger_str("GetHitVar", "chainid"), Value::Int(5));
        assert_eq!(ch.trigger_str("GetHitVar", "fall"), Value::Int(1));
        assert_eq!(ch.trigger_str("GetHitVar", "type"), Value::Int(1));
        // Float members.
        assert_eq!(ch.trigger_str("GetHitVar", "xvel"), Value::Float(-4.0));
        assert_eq!(ch.trigger_str("GetHitVar", "yvel"), Value::Float(-7.5));
        // Case-insensitive member matching.
        assert_eq!(ch.trigger_str("GetHitVar", "DAMAGE"), Value::Int(33));
        assert_eq!(ch.trigger_str("getHitVar", "XVel"), Value::Float(-4.0));
        // Whole-expression evaluation against a populated record.
        assert_eq!(ev("GetHitVar(damage) = 33", &ch), Value::Int(1));
        assert_eq!(ev("GetHitVar(xvel) < 0", &ch), Value::Int(1));
    }

    #[test]
    fn gethitvars_default_is_no_hit() {
        // Default record: all zero except chainid (-1 sentinel).
        let g = GetHitVars::default();
        assert_eq!(g.member("damage"), Value::Int(0));
        assert_eq!(g.member("xvel"), Value::Float(0.0));
        assert_eq!(g.member("chainid"), Value::Int(-1));
        assert_eq!(g.member("unknown"), Value::DEFAULT);
    }

    // =====================================================================
    // Proctor (task 6.2): exhaustive GetHitVars member coverage + edge cases.
    // =====================================================================

    /// A fully-populated `GetHitVars` with a distinct value per field, so a
    /// member-routing bug (returning the wrong field) is caught. Float fields use
    /// non-round values; int fields use distinct integers.
    fn populated_get_hit_vars() -> GetHitVars {
        GetHitVars {
            xvel: -4.25,
            yvel: -7.5,
            yaccel: 0.55,
            hit_type: 11,
            animtype: 12,
            damage: 13,
            hitcount: 14,
            fall: 15,
            hitshaketime: 16,
            hittime: 17,
            slidetime: 18,
            ctrltime: 19,
            isbound: 20,
            guarded: 21,
            chainid: 22,
        }
    }

    #[test]
    fn gethitvar_every_member_routes_to_its_own_field() {
        // AC2: each member name must resolve to its OWN field. A typo in the
        // member()-dispatch (e.g. hittime/hitshaketime swapped) would surface here.
        let g = populated_get_hit_vars();
        // Float-typed members.
        assert_eq!(g.member("xvel"), Value::Float(-4.25));
        assert_eq!(g.member("yvel"), Value::Float(-7.5));
        assert_eq!(g.member("yaccel"), Value::Float(0.55));
        // Integer-typed members.
        assert_eq!(g.member("type"), Value::Int(11));
        assert_eq!(g.member("animtype"), Value::Int(12));
        assert_eq!(g.member("damage"), Value::Int(13));
        assert_eq!(g.member("hitcount"), Value::Int(14));
        assert_eq!(g.member("fall"), Value::Int(15));
        assert_eq!(g.member("hitshaketime"), Value::Int(16));
        assert_eq!(g.member("hittime"), Value::Int(17));
        assert_eq!(g.member("slidetime"), Value::Int(18));
        assert_eq!(g.member("ctrltime"), Value::Int(19));
        assert_eq!(g.member("isbound"), Value::Int(20));
        assert_eq!(g.member("guarded"), Value::Int(21));
        assert_eq!(g.member("chainid"), Value::Int(22));
    }

    #[test]
    fn gethitvar_member_type_is_int_not_float() {
        // The float vs. int distinction is load-bearing: GetHitVar(xvel) must be a
        // Float while GetHitVar(damage) must be an Int, so downstream arithmetic
        // and comparisons behave like MUGEN's.
        let g = populated_get_hit_vars();
        assert!(matches!(g.member("xvel"), Value::Float(_)));
        assert!(matches!(g.member("yvel"), Value::Float(_)));
        assert!(matches!(g.member("yaccel"), Value::Float(_)));
        assert!(matches!(g.member("damage"), Value::Int(_)));
        assert!(matches!(g.member("hittime"), Value::Int(_)));
        assert!(matches!(g.member("chainid"), Value::Int(_)));
    }

    #[test]
    fn gethitvar_member_trims_and_ignores_case() {
        // MUGEN member names are case-insensitive; the resolver also trims
        // surrounding whitespace (a redirection key can carry stray spaces).
        let g = populated_get_hit_vars();
        assert_eq!(g.member("  damage  "), Value::Int(13));
        assert_eq!(g.member("\tXVEL\t"), Value::Float(-4.25));
        assert_eq!(g.member("HitTime"), Value::Int(17));
        assert_eq!(g.member("ChainID"), Value::Int(22));
    }

    #[test]
    fn gethitvar_unknown_members_are_default_zero() {
        // Members MUGEN defines but this struct does not model yet, plus pure
        // garbage and empty, all resolve to the safe default (0) — never a panic.
        let g = populated_get_hit_vars();
        for unknown in [
            "fall.yvel",     // modeled on HitDef but not GetHitVars
            "fall.damage",
            "xveladd",
            "yveladd",
            "groundtype",
            "recovertime",
            "",
            "   ",
            "xvelxvel",
        ] {
            assert_eq!(
                g.member(unknown),
                Value::DEFAULT,
                "unknown GetHitVar member {unknown:?} must default to 0"
            );
        }
    }

    #[test]
    fn gethitvar_populated_resolves_through_character_trigger_str() {
        // End-to-end: a populated record read back through the Character's
        // trigger_str("GetHitVar", member) seam AND through a parsed expression.
        let mut ch = Character::new();
        ch.get_hit_vars = populated_get_hit_vars();
        // Direct seam.
        assert_eq!(ch.trigger_str("GetHitVar", "slidetime"), Value::Int(18));
        assert_eq!(ch.trigger_str("GetHitVar", "guarded"), Value::Int(21));
        assert_eq!(ch.trigger_str("GetHitVar", "yaccel"), Value::Float(0.55));
        // Through the evaluator.
        assert_eq!(ev("GetHitVar(hitcount) = 14", &ch), Value::Int(1));
        assert_eq!(ev("GetHitVar(isbound) != 0", &ch), Value::Int(1));
        assert_eq!(ev("GetHitVar(yvel) < 0", &ch), Value::Int(1));
        // Unknown member through the evaluator → 0.
        assert_eq!(ev("GetHitVar(nope) = 0", &ch), Value::Int(1));
    }

    #[test]
    fn gethitvars_is_copy_and_debug() {
        // GetHitVars is a public Copy/Debug struct; the executor copies it and
        // diagnostics format it. Confirm those derives hold and survive a copy.
        let g = populated_get_hit_vars();
        let copied = g; // Copy, not move.
        assert_eq!(g.member("damage"), copied.member("damage"));
        let dbg = format!("{g:?}");
        assert!(dbg.contains("damage"), "Debug must mention fields");
        // Original still usable after the copy (proves Copy, not move).
        assert_eq!(g.damage, 13);
    }

    #[test]
    fn character_default_has_no_active_hitdef_and_default_get_hit_vars() {
        // AC1/AC2: a freshly-built Character offers no hit and has the default
        // (no-hit) get-hit record — the starting point before any HitDef fires or
        // any hit lands.
        let ch = Character::new();
        assert!(ch.active_hitdef.is_none(), "no active HitDef on a fresh character");
        assert_eq!(ch.get_hit_vars, GetHitVars::default());
        // Every GetHitVar member reads its no-hit default through the seam.
        assert_eq!(ch.trigger_str("GetHitVar", "damage"), Value::Int(0));
        assert_eq!(ch.trigger_str("GetHitVar", "chainid"), Value::Int(-1));
    }

    // ---- const(<member>) resolver (task 5.6e) ------------------------------

    /// Builds a character whose constants are distinct from every default so a
    /// resolver bug that returns a hardcoded/default value is caught.
    fn const_sample() -> Character {
        let consts = CharacterConstants {
            life_max: 1234,
            power_max: 4321,
            attack: 111,
            defence: 222,
            size: SizeConstants {
                ground_front: 17,
                ground_back: 19,
                height: 63,
            },
            velocity: VelocityConstants {
                walk_fwd: Vec2::new(2.7, 0.0),
                walk_back: Vec2::new(-2.1, 0.0),
                run_fwd: Vec2::new(4.9, -1.5),
                jump_neu: Vec2::new(0.3, -8.1),
                jump_up: -8.6,
            },
            movement: MovementConstants {
                yaccel: 0.5,
                stand_friction: 0.83,
                crouch_friction: 0.81,
            },
        };
        Character::with_constants(consts)
    }

    #[test]
    fn const_resolves_velocity_members() {
        let ch = const_sample();
        // Float members thread through the float path via direct equality.
        assert_eq!(ch.trigger_str("const", "velocity.walk.fwd.x"), Value::Float(2.7));
        assert_eq!(ch.trigger_str("const", "velocity.walk.back.x"), Value::Float(-2.1));
        assert_eq!(ch.trigger_str("const", "velocity.run.fwd.x"), Value::Float(4.9));
        assert_eq!(ch.trigger_str("const", "velocity.run.fwd.y"), Value::Float(-1.5));
        assert_eq!(ch.trigger_str("const", "velocity.jump.neu.x"), Value::Float(0.3));
        assert_eq!(ch.trigger_str("const", "velocity.jump.y"), Value::Float(-8.6));
    }

    #[test]
    fn const_resolves_size_members() {
        let ch = const_sample();
        assert_eq!(ch.trigger_str("const", "size.ground.front"), Value::Int(17));
        assert_eq!(ch.trigger_str("const", "size.ground.back"), Value::Int(19));
        assert_eq!(ch.trigger_str("const", "size.height"), Value::Int(63));
    }

    #[test]
    fn const_resolves_movement_members() {
        let ch = const_sample();
        assert_eq!(ch.trigger_str("const", "movement.yaccel"), Value::Float(0.5));
        assert_eq!(ch.trigger_str("const", "movement.stand.friction"), Value::Float(0.83));
        assert_eq!(ch.trigger_str("const", "movement.crouch.friction"), Value::Float(0.81));
    }

    #[test]
    fn const_resolves_data_members() {
        let ch = const_sample();
        assert_eq!(ch.trigger_str("const", "data.life"), Value::Int(1234));
        assert_eq!(ch.trigger_str("const", "data.power"), Value::Int(4321));
        assert_eq!(ch.trigger_str("const", "data.attack"), Value::Int(111));
        assert_eq!(ch.trigger_str("const", "data.defence"), Value::Int(222));
    }

    #[test]
    fn const_member_match_is_case_insensitive() {
        let ch = const_sample();
        // Mixed/upper case on the full dotted name resolves the same value.
        assert_eq!(ch.trigger_str("const", "Velocity.Walk.Fwd.X"), Value::Float(2.7));
        assert_eq!(ch.trigger_str("const", "SIZE.GROUND.FRONT"), Value::Int(17));
        assert_eq!(ch.trigger_str("const", "Movement.YAccel"), Value::Float(0.5));
        // The trigger name itself is also case-insensitive.
        assert_eq!(ch.trigger_str("CONST", "data.life"), Value::Int(1234));
    }

    #[test]
    fn const_unknown_member_is_safe_default() {
        let ch = const_sample();
        // Unknown member → Value::DEFAULT (0), never a panic.
        assert_eq!(ch.trigger_str("const", "no.such.member"), Value::DEFAULT);
        assert_eq!(ch.trigger_str("const", ""), Value::DEFAULT);
    }

    #[test]
    fn const_routes_through_parse_and_eval() {
        // End-to-end through fp_vm::parse_str + eval: `const(member)` routes via
        // trigger_str and compares against the loaded value.
        let ch = const_sample();
        assert_eq!(ev("const(velocity.walk.fwd.x) = 2.7", &ch), Value::Int(1));
        assert_eq!(ev("const(size.ground.front) = 17", &ch), Value::Int(1));
        assert_eq!(ev("const(movement.yaccel) > 0", &ch), Value::Int(1));
        assert_eq!(ev("const(data.life) = 1234", &ch), Value::Int(1));
        // Unknown member compares equal to 0 (and never panics).
        assert_eq!(ev("const(no.such.member) = 0", &ch), Value::Int(1));
    }

    #[test]
    fn const_uses_default_values_for_default_character() {
        // A character built from CharacterConstants::default() reads the KFM
        // baseline values the defaults encode.
        let ch = Character::new();
        assert_eq!(ev("const(velocity.walk.fwd.x) = 2.4", &ch), Value::Int(1));
        assert_eq!(ev("const(size.ground.front) = 16", &ch), Value::Int(1));
        assert_eq!(ev("const(movement.yaccel) = 0.44", &ch), Value::Int(1));
    }

    #[test]
    fn const_real_kfm_walk_fwd_x_is_2_4() {
        // Gated real-fixture test: load KFM, build a Character from its loaded
        // constants, and assert `const(velocity.walk.fwd.x)` evaluates to KFM's
        // authored 2.4 via fp_vm::parse_str + eval. Skips when the fixture is
        // absent (matching the loader's gated real-KFM tests).
        let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let loaded = LoadedCharacter::load(&def).expect("kfm.def should load");
        let ch = Character::with_constants(loaded.constants);
        assert_eq!(ev("const(velocity.walk.fwd.x) = 2.4", &ch), Value::Int(1));
    }

    // =====================================================================
    // Proctor (task 5.6e): edge-case / error-path / MUGEN-semantics coverage
    // for the `const(<member>)` resolver, layered on top of Forge's tests
    // above. Grouped by acceptance criterion. All synthetic data is distinct
    // from every CharacterConstants default so a resolver that returns a
    // hardcoded/default value is caught; the gated real-KFM end-to-end test is
    // skip-if-absent.
    // =====================================================================

    // ---- AC1: every modeled member maps to the CORRECT field (no aliasing) --

    #[test]
    fn const_members_do_not_alias_each_other() {
        // A swapped/transposed mapping bug (e.g. walk.fwd.x reading walk.back.x,
        // or jump.neu.x reading jump.y) would pass a "non-default" smoke test but
        // be wrong. Pin each member to its OWN field value via const_sample(),
        // whose every constant is distinct.
        let ch = const_sample();
        // The set of (member, expected) pairs is exhaustive over the modeled
        // surface; if two members returned the same wrong field, at least one of
        // these would fail because the source values are all unique.
        let int_pairs = [
            ("data.life", 1234),
            ("data.power", 4321),
            ("data.attack", 111),
            ("data.defence", 222),
            ("size.ground.front", 17),
            ("size.ground.back", 19),
            ("size.height", 63),
        ];
        for (m, want) in int_pairs {
            assert_eq!(ch.trigger_str("const", m), Value::Int(want), "member `{m}`");
        }
        let float_pairs = [
            ("velocity.walk.fwd.x", 2.7f32),
            ("velocity.walk.back.x", -2.1),
            ("velocity.run.fwd.x", 4.9),
            ("velocity.run.fwd.y", -1.5),
            ("velocity.jump.neu.x", 0.3),
            ("velocity.jump.y", -8.6),
            ("movement.yaccel", 0.5),
            ("movement.stand.friction", 0.83),
            ("movement.crouch.friction", 0.81),
        ];
        for (m, want) in float_pairs {
            assert_eq!(ch.trigger_str("const", m), Value::Float(want), "member `{m}`");
        }
    }

    #[test]
    fn const_member_value_types_match_mugen_groups() {
        // [Data] and [Size] are integer-typed; [Velocity] and [Movement] are
        // float-typed. The resolver must preserve those types so downstream
        // arithmetic stays in the right domain (a float const compared against an
        // int literal must not silently truncate the const).
        let ch = const_sample();
        for m in [
            "data.life", "data.power", "data.attack", "data.defence",
            "size.ground.front", "size.ground.back", "size.height",
        ] {
            assert!(ch.trigger_str("const", m).is_int(), "`{m}` must be int-typed");
        }
        for m in [
            "velocity.walk.fwd.x", "velocity.walk.back.x", "velocity.run.fwd.x",
            "velocity.run.fwd.y", "velocity.jump.neu.x", "velocity.jump.y",
            "movement.yaccel", "movement.stand.friction", "movement.crouch.friction",
        ] {
            assert!(ch.trigger_str("const", m).is_float(), "`{m}` must be float-typed");
        }
    }

    #[test]
    fn const_run_fwd_x_and_y_are_independent_components() {
        // `velocity.run.fwd` is the only modeled member with BOTH an x and a y
        // component read separately. Confirm they thread to the matching Vec2
        // axis (a bug returning x for both would pass a single-axis test).
        let ch = const_sample(); // run_fwd = (4.9, -1.5)
        assert_eq!(ch.trigger_str("const", "velocity.run.fwd.x"), Value::Float(4.9));
        assert_eq!(ch.trigger_str("const", "velocity.run.fwd.y"), Value::Float(-1.5));
        assert_ne!(
            ch.trigger_str("const", "velocity.run.fwd.x"),
            ch.trigger_str("const", "velocity.run.fwd.y"),
            "run.fwd.x and run.fwd.y must read distinct components"
        );
    }

    #[test]
    fn const_jump_neu_x_and_jump_y_are_distinct_fields() {
        // `jump.neu.x` reads VelocityConstants::jump_neu.x; `jump.y` reads the
        // separate jump_up field. const_sample() makes them distinct (0.3 vs
        // -8.6) so a mapping that confuses the two is caught.
        let ch = const_sample();
        assert_eq!(ch.trigger_str("const", "velocity.jump.neu.x"), Value::Float(0.3));
        assert_eq!(ch.trigger_str("const", "velocity.jump.y"), Value::Float(-8.6));
    }

    // ---- AC1: case-insensitive matching on the FULL dotted name --------------

    #[test]
    fn const_member_match_handles_arbitrary_casing_per_segment() {
        // MUGEN folds case across the whole dotted name; segments may be mixed.
        // const_sample() keeps every value unique so a case slip that lands on a
        // different member would surface.
        let ch = const_sample();
        assert_eq!(ch.trigger_str("const", "VELOCITY.WALK.FWD.X"), Value::Float(2.7));
        assert_eq!(ch.trigger_str("const", "velocity.WALK.fwd.X"), Value::Float(2.7));
        assert_eq!(ch.trigger_str("const", "Size.Ground.Back"), Value::Int(19));
        assert_eq!(ch.trigger_str("const", "Movement.Crouch.Friction"), Value::Float(0.81));
        assert_eq!(ch.trigger_str("const", "DATA.DEFENCE"), Value::Int(222));
    }

    #[test]
    fn const_member_leading_trailing_whitespace_is_tolerated() {
        // The resolver trims the member before matching, so a member arriving with
        // incidental surrounding whitespace still resolves rather than defaulting.
        // (Defends the never-panic / be-lenient posture on messy content.)
        let ch = const_sample();
        assert_eq!(ch.trigger_str("const", "  velocity.walk.fwd.x  "), Value::Float(2.7));
        assert_eq!(ch.trigger_str("const", "\tsize.height\t"), Value::Int(63));
    }

    // ---- AC1: unknown / malformed members → Value::DEFAULT, never panic ------

    #[test]
    fn const_partial_and_prefix_members_default() {
        // Members that are PREFIXES of a real dotted name, or a real name with an
        // extra suffix, are NOT matches: the match is on the exact full dotted
        // name. They must default to 0 (debug-logged), never partially resolve.
        let ch = const_sample();
        for m in [
            "data",                       // group only
            "velocity",                   // group only
            "velocity.walk",              // partial
            "velocity.walk.fwd",          // missing axis
            "size.ground",                // partial
            "movement",                   // group only
            "velocity.walk.fwd.x.extra",  // trailing junk
            "size.ground.front.x",        // bogus axis on an int member
            "velocity.walk.fwd.z",        // unmodeled z axis
            "data.life.max",              // over-qualified
        ] {
            assert_eq!(
                ch.trigger_str("const", m),
                Value::DEFAULT,
                "non-exact member `{m}` must default to 0"
            );
        }
    }

    #[test]
    fn const_adversarial_member_strings_never_panic() {
        // The resolver must survive arbitrary, hostile member strings (a core
        // "never crash on bad content" invariant). None should panic; all unknown
        // strings resolve to the safe default.
        let ch = const_sample();
        let junk = [
            "",
            ".",
            "...",
            "   ",
            "velocity..walk..fwd..x",
            "VELOCITY .WALK. FWD .X", // internal spaces are not stripped → no match
            "🥋.combo",               // non-ASCII
            &"a".repeat(4096),        // very long
        ];
        for m in junk {
            assert_eq!(
                ch.trigger_str("const", m),
                Value::DEFAULT,
                "adversarial member `{m}` must default to 0 without panicking"
            );
        }
    }

    #[test]
    fn const_internal_whitespace_is_not_a_match() {
        // Only leading/trailing whitespace is trimmed; internal whitespace within
        // the dotted name is significant, so a spaced-out member does NOT alias a
        // valid one. (Pins that `trim()`, not a whitespace-stripping match, is the
        // semantics.)
        let ch = const_sample();
        assert_eq!(ch.trigger_str("const", "velocity. walk.fwd.x"), Value::DEFAULT);
        assert_eq!(ch.trigger_str("const", "size .height"), Value::DEFAULT);
    }

    // ---- AC1: the GetHitVar branch of trigger_str is unchanged ---------------

    #[test]
    fn const_and_gethitvar_branches_are_independent() {
        // A const member queried under the GetHitVar trigger name must NOT resolve
        // to the constant (it is not a hit field) — it defaults. Symmetrically, a
        // GetHitVar member under `const` is unknown and defaults. This pins that
        // the two member-keyed branches do not bleed into each other and that the
        // GetHitVar branch still defaults (Phase 6 deferral) unchanged.
        let ch = const_sample();
        // const member name routed under GetHitVar → default, not 2.7.
        assert_eq!(
            ch.trigger_str("GetHitVar", "velocity.walk.fwd.x"),
            Value::DEFAULT
        );
        // GetHitVar member name routed under const → unknown const member → default.
        assert_eq!(ch.trigger_str("const", "fall.yvel"), Value::DEFAULT);
        assert_eq!(ch.trigger_str("const", "xveladd"), Value::DEFAULT);
        // The GetHitVar branch itself still defaults for its own members.
        assert_eq!(ch.trigger_str("GetHitVar", "fall.yvel"), Value::DEFAULT);
    }

    #[test]
    fn trigger_str_unknown_function_name_defaults() {
        // A string-keyed trigger name that is neither `const` nor `GetHitVar`
        // falls through to the safe default rather than mis-routing to either
        // branch.
        let ch = const_sample();
        assert_eq!(ch.trigger_str("NotARealStrTrigger", "velocity.walk.fwd.x"), Value::DEFAULT);
        assert_eq!(ch.trigger_str("", "data.life"), Value::DEFAULT);
    }

    // ---- AC2: end-to-end through parse_str + eval, including MUGEN compare ---

    #[test]
    fn const_int_member_compares_against_float_literal_end_to_end() {
        // MUGEN promotes to float when either side is float. An int-typed const
        // compared against a float literal must compare by numeric value, so
        // `const(size.ground.front) = 17.0` is true for ground_front == 17.
        let ch = const_sample();
        assert_eq!(ev("const(size.ground.front) = 17.0", &ch), Value::Int(1));
        assert_eq!(ev("const(data.life) >= 1000.5", &ch), Value::Int(1)); // 1234 >= 1000.5
        assert_eq!(ev("const(size.height) < 63.5", &ch), Value::Int(1)); // 63 < 63.5
    }

    #[test]
    fn const_mixed_case_member_resolves_through_parser() {
        // The lexer preserves identifier case verbatim, so a mixed-case member
        // reaches trigger_str unfolded; this exercises resolve_const's OWN
        // case-insensitive matching end-to-end (not lexer folding).
        let ch = const_sample();
        assert_eq!(ev("const(Velocity.Walk.Fwd.X) = 2.7", &ch), Value::Int(1));
        assert_eq!(ev("const(SIZE.GROUND.FRONT) = 17", &ch), Value::Int(1));
        assert_eq!(ev("const(Movement.YAccel) > 0", &ch), Value::Int(1));
    }

    #[test]
    fn const_used_in_compound_kfm_style_trigger() {
        // A realistic shape: gate on velocity being authored positive AND a size
        // bound — the kind of compound expression common states build from const.
        let ch = const_sample();
        let expr = "const(velocity.walk.fwd.x) > 0 && const(size.ground.front) > 0";
        assert_eq!(ev(expr, &ch), Value::Int(1));
        // Negative authored back-walk velocity flows through arithmetic correctly.
        assert_eq!(ev("const(velocity.walk.back.x) < 0", &ch), Value::Int(1));
    }

    #[test]
    fn const_unknown_member_is_falsey_end_to_end() {
        // An unknown member resolves to 0, so bare `const(bogus)` is falsey and a
        // comparison against 0 is true — and never panics through the VM.
        let ch = const_sample();
        assert_eq!(ev("const(no.such.member)", &ch), Value::Int(0));
        assert_eq!(ev("const(no.such.member) = 0", &ch), Value::Int(1));
        // An unknown member never accidentally equals a real authored value.
        assert_eq!(ev("const(no.such.member) = 2.7", &ch), Value::Int(0));
    }

    #[test]
    fn const_default_character_reads_kfm_baseline_all_members() {
        // CharacterConstants::default() encodes KFM's authored baseline. Pin the
        // full modeled surface against those documented defaults so the default
        // table and the resolver mapping stay in lockstep.
        let ch = Character::new();
        assert_eq!(ev("const(velocity.walk.fwd.x) = 2.4", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.walk.back.x) = -2.2", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.run.fwd.x) = 4.6", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.run.fwd.y) = 0", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.jump.neu.x) = 0", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.jump.y) = -8.4", &ch), Value::Int(1));
        assert_eq!(ev("const(size.ground.front) = 16", &ch), Value::Int(1));
        assert_eq!(ev("const(size.ground.back) = 15", &ch), Value::Int(1));
        assert_eq!(ev("const(size.height) = 60", &ch), Value::Int(1));
        assert_eq!(ev("const(movement.yaccel) = 0.44", &ch), Value::Int(1));
        assert_eq!(ev("const(movement.stand.friction) = 0.85", &ch), Value::Int(1));
        assert_eq!(ev("const(movement.crouch.friction) = 0.82", &ch), Value::Int(1));
        assert_eq!(ev("const(data.life) = 1000", &ch), Value::Int(1));
        assert_eq!(ev("const(data.power) = 3000", &ch), Value::Int(1));
        assert_eq!(ev("const(data.attack) = 100", &ch), Value::Int(1));
        assert_eq!(ev("const(data.defence) = 100", &ch), Value::Int(1));
    }

    // ---- AC2: gated real-KFM fixture, broadened beyond walk.fwd.x ------------

    #[test]
    fn const_real_kfm_members_match_authored_values() {
        // Gated real-fixture test: load KFM and assert a SPREAD of const members
        // evaluate to KFM's authored values via fp_vm::parse_str + eval. Skips
        // cleanly when test-assets/ is absent (matching the loader's gated tests).
        // This is the broadened companion to const_real_kfm_walk_fwd_x_is_2_4:
        // it exercises every constant group against real content, so a per-group
        // mapping regression is caught against authored data, not just synthetic.
        let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let loaded = LoadedCharacter::load(&def).expect("kfm.def should load");
        let ch = Character::with_constants(loaded.constants);

        // Walk velocities (the task's headline value plus its mirror).
        assert_eq!(ev("const(velocity.walk.fwd.x) = 2.4", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.walk.back.x) = -2.2", &ch), Value::Int(1));
        // Run + jump (signs/components).
        assert_eq!(ev("const(velocity.run.fwd.x) > 0", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.jump.y) < 0", &ch), Value::Int(1));
        // Size group is integer-typed and positive.
        assert_eq!(ev("const(size.ground.front) > 0", &ch), Value::Int(1));
        assert_eq!(ev("const(size.height) > 0", &ch), Value::Int(1));
        // Movement group: gravity positive, frictions in (0,1].
        assert_eq!(ev("const(movement.yaccel) > 0", &ch), Value::Int(1));
        assert_eq!(ev("const(movement.stand.friction) > 0", &ch), Value::Int(1));
        // Data group: life/power are the authored maxima (KFM ships 1000/3000).
        assert_eq!(ev("const(data.life) > 0", &ch), Value::Int(1));
        assert_eq!(ev("const(data.power) > 0", &ch), Value::Int(1));
        // Case-insensitive end-to-end against real content.
        assert_eq!(ev("const(VELOCITY.WALK.FWD.X) = 2.4", &ch), Value::Int(1));
        // An unknown member against a real character still defaults to 0.
        assert_eq!(ev("const(no.such.member) = 0", &ch), Value::Int(1));
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

    // =====================================================================
    // Proctor (task 5.6b): `alive` trigger + common-state trigger audit.
    // These layer MUGEN-semantics edge cases and the real-content death-gate
    // scenario on top of Forge's `alive_tracks_life` /
    // `deferred_triggers_stay_at_safe_default` tests. Grouped by the task's
    // acceptance criteria.
    // =====================================================================

    // ---- AC1: `alive` resolves to Life>0, placed before the unknown fallthrough.

    #[test]
    fn alive_is_zero_for_overkill_negative_life() {
        // MUGEN can drive Life below zero on an overkill hit before clamping.
        // `alive` is `Life > 0`, so any non-positive Life (including negative)
        // must read 0 — never the unknown-trigger default leaking through as a
        // surprising 1, and never a panic on the signed value.
        let mut ch = sample();
        ch.life = -250;
        assert_eq!(ch.trigger("alive", &[]), Value::Int(0));
        assert_eq!(ev("alive", &ch), Value::Int(0));
        assert_eq!(ev("!alive", &ch), Value::Int(1));
        // The exact boundary: Life == 0 is dead, Life == 1 is alive.
        ch.life = 0;
        assert_eq!(ev("alive", &ch), Value::Int(0));
        ch.life = 1;
        assert_eq!(ev("alive", &ch), Value::Int(1));
        // Extreme negative value still reads dead, no overflow/panic.
        ch.life = i32::MIN;
        assert_eq!(ev("alive", &ch), Value::Int(0));
    }

    #[test]
    fn alive_is_int_typed_and_case_insensitive() {
        // `alive` is a boolean-coded trigger → the int 1/0 variant (never float),
        // so it threads through `!`, `&&`, and integer comparison cleanly.
        let mut ch = sample();
        ch.life = ch.life_max;
        for spelling in ["alive", "Alive", "ALIVE", "aLiVe"] {
            assert_eq!(
                ch.trigger(spelling, &[]),
                Value::Int(1),
                "`{spelling}` should resolve case-insensitively to 1"
            );
            assert!(
                ch.trigger(spelling, &[]).is_int(),
                "`{spelling}` must be int-typed, not float"
            );
        }
        // Through the parser/evaluator with varied casing.
        assert_eq!(ev("Alive", &ch), Value::Int(1));
        assert_eq!(ev("!ALIVE", &ch), Value::Int(0));
    }

    #[test]
    fn alive_ignores_spurious_arguments() {
        // `alive` is argument-less in MUGEN. The arm matches on name alone, so a
        // (malformed) parenthesized call still resolves from Life rather than
        // panicking — defends the "never panic" invariant on odd content.
        let mut ch = sample();
        ch.life = 500;
        assert_eq!(ch.trigger("alive", &[Value::Int(7)]), Value::Int(1));
        ch.life = 0;
        assert_eq!(ch.trigger("alive", &[Value::Int(7)]), Value::Int(0));
    }

    #[test]
    fn alive_unaffected_by_lifemax() {
        // Liveness is about *current* Life crossing zero, independent of LifeMax.
        // A character with a tiny max but positive life is alive; draining to 0
        // makes it dead regardless of max.
        let mut ch = Character::new();
        ch.life_max = 1;
        ch.life = 1;
        assert_eq!(ev("alive", &ch), Value::Int(1));
        ch.life = 0;
        assert_eq!(ev("alive", &ch), Value::Int(0));
        // Large max, full life: still alive.
        ch.life_max = 9999;
        ch.life = 9999;
        assert_eq!(ev("alive", &ch), Value::Int(1));
    }

    // ---- AC1: the real common1.cns death-gate scenario (the task's motivation).

    #[test]
    fn common1_death_gate_does_not_fire_at_full_life() {
        // common1.cns `[State 0, 4] ;Are you dead?` is a ChangeState to the death
        // state (5050) gated on `trigger1 = !alive`. Before 5.6b, `alive` hit the
        // unknown-trigger default 0, so `!alive` was always TRUE and a full-life
        // KFM dropped into the death state on tick 1. With `alive` implemented the
        // gate must be FALSE at full life and only TRUE once Life reaches 0.
        let mut ch = Character::new(); // new() => full life, alive
        ch.state_no = 0;
        ch.state_time = 1;
        // The death-gate trigger expression, verbatim from the stock state.
        let death_gate = "!alive";
        assert_eq!(
            ev(death_gate, &ch),
            Value::Int(0),
            "full-life KFM must NOT satisfy the !alive death gate"
        );
        // Drive life to zero: now (and only now) the gate fires → ChangeState 5050.
        ch.life = 0;
        assert_eq!(
            ev(death_gate, &ch),
            Value::Int(1),
            "a KO'd KFM must satisfy the !alive death gate"
        );
    }

    #[test]
    fn common1_alive_guard_forms_evaluate_both_ways() {
        // common1.cns also gates *recovery* states on `triggerall = alive` and
        // `trigger1 = alive`. The positive form must mirror the negative one: true
        // at positive life, false at zero. (Pins both polarities used in stock
        // content so a regression in either direction is caught.)
        let mut ch = Character::new();
        ch.life = 300;
        assert_eq!(ev("alive", &ch), Value::Int(1)); // recovery allowed
        assert_eq!(ev("!alive", &ch), Value::Int(0)); // death gate closed
        ch.life = 0;
        assert_eq!(ev("alive", &ch), Value::Int(0)); // recovery blocked
        assert_eq!(ev("!alive", &ch), Value::Int(1)); // death gate open
    }

    // ---- AC2: the deferred-trigger audit is pinned against the *actual* names
    // that appear in test-assets/kfm/{kfm,common1}.cns. Each must still default
    // to 0 (so common states do not misfire on them) and never panic. This makes
    // a future implementation of any of them a deliberate, test-breaking change.

    #[test]
    fn audited_deferred_triggers_all_default_to_zero() {
        let ch = sample();
        // Names harvested from the trigger lines of the stock CNS files that the
        // EvalContext deliberately does not resolve yet (get-hit / round / match
        // / cross-entity geometry / anim-table queries). Includes case variants
        // to confirm the default holds regardless of spelling.
        let deferred = [
            // Get-hit state (Phase 6). `HitShakeOver`/`MoveContact`/`MoveHit`/
            // `MoveGuarded` are no longer here: task 6.3b answers them from the
            // fields hit resolution populates (see `move_connect_triggers`).
            "HitOver", "hitover", "HitFall", "CanRecover", "InGuardDist",
            // Round / match state (engine, Phase 5+).
            "RoundState", "roundstate", "RoundNo", "RoundsExisted", "MatchOver",
            "GameTime",
            // Cross-entity geometry (Phase 7 redirection).
            "P2BodyDist", "P2Dist", "FrontEdgeBodyDist", "BackEdgeBodyDist",
            "BackEdgeDist",
            // Animation-table query (executor owns the .air set).
            "SelfAnimExist",
        ];
        for t in deferred {
            let v = ch.trigger(t, &[]);
            assert_eq!(v, Value::DEFAULT, "deferred trigger `{t}` must default to 0");
            // Value::DEFAULT is documented as Value::Int(0); pin that contract so
            // a comparison against literal 0 in stock content still holds.
            assert_eq!(v, Value::Int(0), "deferred trigger `{t}` default must be int 0");
        }
        // With representative args (these are functions in real content, e.g.
        // SelfAnimExist(44), P2BodyDist), the deferred path must still default,
        // not panic.
        assert_eq!(ch.trigger("SelfAnimExist", &[Value::Int(44)]), Value::Int(0));
        assert_eq!(ch.trigger("P2BodyDist", &[Value::Int(0)]), Value::Int(0));
    }

    #[test]
    fn move_connect_triggers_track_the_move_connect_field() {
        // MoveHit / MoveGuarded / MoveContact read `move_connect`, populated by
        // hit resolution (task 6.3b). A fresh move reads all false.
        let mut ch = Character::new();
        assert_eq!(ev("MoveHit", &ch), Value::Int(0));
        assert_eq!(ev("MoveGuarded", &ch), Value::Int(0));
        assert_eq!(ev("MoveContact", &ch), Value::Int(0));

        // A clean hit sets MoveHit (and so MoveContact), not MoveGuarded.
        ch.move_connect.hit = true;
        assert_eq!(ev("MoveHit", &ch), Value::Int(1));
        assert_eq!(ev("MoveGuarded", &ch), Value::Int(0));
        assert_eq!(ev("MoveContact", &ch), Value::Int(1));

        // A guard sets MoveGuarded (and so MoveContact), not MoveHit.
        ch.move_connect.reset();
        ch.move_connect.guarded = true;
        assert_eq!(ev("MoveHit", &ch), Value::Int(0));
        assert_eq!(ev("MoveGuarded", &ch), Value::Int(1));
        assert_eq!(ev("MoveContact", &ch), Value::Int(1));
    }

    #[test]
    fn hitshakeover_tracks_shaketime() {
        // HitShakeOver is true once the defender's shake timer has elapsed.
        let mut ch = Character::new();
        assert_eq!(ev("HitShakeOver", &ch), Value::Int(1)); // no shake -> over
        ch.shaketime = 3;
        assert_eq!(ev("HitShakeOver", &ch), Value::Int(0)); // still shaking
        ch.shaketime = 0;
        assert_eq!(ev("HitShakeOver", &ch), Value::Int(1));
    }

    #[test]
    fn alive_is_not_in_the_deferred_set() {
        // Guard against a regression that would re-defer `alive`: unlike the
        // audited deferred names, `alive` must NOT collapse to the unknown
        // default at full life. (If someone removes the `alive` arm, this fails
        // even though the generic default still returns Value::Int(0).)
        let ch = Character::new(); // full life
        assert_eq!(ch.trigger("alive", &[]), Value::Int(1));
        assert_ne!(
            ch.trigger("alive", &[]),
            ch.trigger("ThisTriggerDoesNotExist", &[]),
            "alive must resolve to a real value, not the unknown-trigger default"
        );
    }

    // ---- AC3: already-supported triggers are unchanged; nothing panics.

    #[test]
    fn alive_arm_does_not_shadow_other_triggers() {
        // Adding the `alive` arm must not perturb neighbouring resolutions. Spot
        // check a spread of previously-supported triggers still answer correctly
        // on the same character whose `alive` we also read.
        let mut ch = sample();
        ch.life = 0; // dead, to make alive=0 distinctly observable
        assert_eq!(ev("alive", &ch), Value::Int(0));
        // Unrelated triggers keep their values regardless of liveness.
        assert_eq!(ev("StateNo = 200", &ch), Value::Int(1));
        assert_eq!(ev("Anim = 200", &ch), Value::Int(1));
        assert_eq!(ev("Vel X = 2.5", &ch), Value::Int(1));
        assert_eq!(ev("LifeMax = 1000", &ch), Value::Int(1));
        assert_eq!(ev("Power = 500", &ch), Value::Int(1));
    }

    // ---- AC4 / AC5: real-content fixture for the `!alive` death gate, gated to
    // skip cleanly when test-assets/ is absent.

    #[test]
    fn real_common1_death_gate_trigger_when_present() {
        // Reads the stock common1.cns and extracts the `!alive` death-gate trigger
        // expression(s) verbatim, then asserts the implemented `alive` makes the
        // gate behave correctly: closed (0) for a full-life KFM, open (1) when KO.
        // SKIPS cleanly when the asset is absent so the suite stays green in a
        // checkout without test-assets.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-assets/kfm/common1.cns");
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return, // asset absent → skip, not a failure
        };

        // Collect the right-hand side of every `trigger... = ...alive...` line.
        let alive_exprs: Vec<String> = contents
            .lines()
            .map(str::trim)
            .filter(|l| {
                let lower = l.to_ascii_lowercase();
                lower.starts_with("trigger") && lower.contains("alive")
            })
            .filter_map(|l| l.split_once('=').map(|(_, rhs)| rhs.trim().to_string()))
            .collect();

        // The stock file is known to gate on `alive` (recovery) and `!alive`
        // (death). If neither appears the fixture is unexpectedly shaped; assert
        // we found at least one so the test cannot silently pass on a stripped
        // file that still exists.
        assert!(
            !alive_exprs.is_empty(),
            "common1.cns present but contains no `alive`-gated triggers"
        );

        let alive = Character::new(); // full life
        let mut dead = Character::new();
        dead.life = 0;

        let mut saw_negated = false;
        let mut saw_plain = false;
        for raw in &alive_exprs {
            // Only evaluate expressions that are *exactly* an alive guard
            // (`alive` or `!alive`), case-insensitively. Other lines that merely
            // contain the substring (none expected in stock KFM, but be safe) are
            // skipped so the assertions stay meaningful.
            let norm = raw.to_ascii_lowercase();
            let norm = norm.split(';').next().unwrap_or("").trim(); // strip comments
            match norm {
                "!alive" => {
                    saw_negated = true;
                    assert_eq!(ev(raw, &alive), Value::Int(0), "`{raw}` false at full life");
                    assert_eq!(ev(raw, &dead), Value::Int(1), "`{raw}` true when KO");
                }
                "alive" => {
                    saw_plain = true;
                    assert_eq!(ev(raw, &alive), Value::Int(1), "`{raw}` true at full life");
                    assert_eq!(ev(raw, &dead), Value::Int(0), "`{raw}` false when KO");
                }
                _ => {}
            }
        }
        // Stock common1.cns has both polarities; require we exercised the death
        // gate at minimum (the task's core scenario).
        assert!(saw_negated, "expected a `!alive` death gate in common1.cns");
        assert!(saw_plain, "expected an `alive` recovery guard in common1.cns");
    }
}
