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
pub mod framedata;
pub mod identity;
pub mod invuln;
pub mod ir_cache;
pub mod loader;
pub mod snapshot;

pub use combat::{resolve_attack, AttackResolution};
pub use executor::{
    ExplodOp, ExplodPosType, ExplodSpawn, FreezeKind, FreezeRequest, HelperPosType, HelperSpawn,
    ProjectileSpawn, SoundRequest, TargetOp, TickReport,
};
pub use framedata::{frame_advantage, MoveFrameData};
pub use identity::CharacterFingerprint;
pub use invuln::{AttackAttrSet, InvulnMask, InvulnMode, InvulnSlot};
pub use snapshot::CharacterSnapshot;
// Re-export the combat sound reference so downstream crates (e.g. fp-engine) can
// name the type of [`AttackResolution::hit_sound`] without taking a direct
// dependency on fp-combat.
pub use fp_combat::SoundId;
pub use loader::{
    CompiledController, CompiledExpr, CompiledParam, CompiledState, CompiledTriggerGroup,
    LoadedCharacter, LoadedPalette,
};

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use fp_core::Vec2;
use fp_formats::air::AnimAction;
use fp_vm::{AssignBank, EvalContext, Redirect, Rng, Value};
use serde::{Deserialize, Serialize};

/// The fixed default seed for a fresh character's `random` RNG stream.
///
/// A [`Character`] is seeded to this value so the MUGEN `random` trigger is
/// **deterministic out of the box** — every fresh character draws the same
/// sequence on every run and every machine, which is what frame-perfect netplay
/// rollback / replay (#38) requires. It is a fixed constant on purpose: seeding
/// from the wall clock or an OS RNG would make replays diverge. To give a match
/// its own reproducible stream (e.g. one derived from a shared match seed), call
/// [`Character::seed_rng`] after construction.
pub const DEFAULT_RNG_SEED: i32 = 1;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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

/// Clamps a possibly-out-of-range signed element index into `0..len`, returning
/// `0` when `len` is `0`. Used to look up the current element safely in the
/// per-element start-offset table without panicking on stale/external mutation.
fn clamp_usize(index: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if index < 0 {
        0
    } else {
        (index as usize).min(len - 1)
    }
}

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
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// Air-juggle point allowance (`[Data] airjuggle`). Defaults to MUGEN's
    /// `15` (KFM's value).
    ///
    /// This is the per-defender pool of juggle points refilled whenever the
    /// character returns to the ground (or a fresh combo begins). Each hit a move
    /// lands on an airborne / falling defender costs that move's `[Statedef]
    /// juggle` points; when the pool can no longer pay the cost the hit is
    /// dropped (the juggle limit). See [`Character::juggle_points`] and
    /// [`combat::resolve_attack`](crate::combat::resolve_attack).
    pub airjuggle: i32,
    /// `[Size]` group: player dimensions.
    pub size: SizeConstants,
    /// `[Velocity]` group: walk and jump velocities.
    pub velocity: VelocityConstants,
    /// `[Movement]` group: gravity and friction coefficients.
    pub movement: MovementConstants,
    /// `[Info] localcoord` — the character's authoring coordinate space
    /// `(width, height)` in pixels. Defaults to MUGEN's `(320, 240)`.
    ///
    /// This mirrors [`LoadedCharacter::localcoord`](crate::LoadedCharacter) onto
    /// the constants so the [`EvalContext`] (which reaches the character only via
    /// `me.constants`) can read it. It is the divisor source for the
    /// coordinate-scaling triggers `Const720p` and `Const1280p`, which scale a
    /// value authored in a high-resolution space down to this character's space
    /// by the **width ratio** (`localcoord.0 / reference_width`, where the
    /// reference width is `1280` for `Const720p` and `2560` for `Const1280p`).
    pub localcoord: (i32, i32),
}

impl Default for CharacterConstants {
    fn default() -> Self {
        Self {
            life_max: 1000,
            power_max: 3000,
            attack: 100,
            defence: 100,
            // MUGEN's de-facto baseline (KFM's `[Data] airjuggle = 15`); a
            // character that omits the key gets this allowance.
            airjuggle: 15,
            size: SizeConstants::default(),
            velocity: VelocityConstants::default(),
            movement: MovementConstants::default(),
            // MUGEN's de-facto baseline authoring space (KFM and most stock
            // content). A character that omits `[Info] localcoord` is treated as
            // authored in this space, so coordinate-scaling triggers
            // (`Const720p`/`Const1280p`) downscale relative to (320, 240).
            localcoord: (320, 240),
        }
    }
}

/// The `[Size]` constant group: the character's collision/positioning
/// dimensions.
///
/// Only the fields the executor and physics need are modeled here (player
/// widths and height); the remaining `[Size]` keys (`xscale`, `head.pos`, …)
/// are not read yet. Each defaults to KFM's value, MUGEN's de-facto baseline.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
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
/// right; the executor mirrors them by [`Facing::sign`]. The fields needed for
/// locomotion and jumping are modeled: walk forward/back, run forward/back,
/// neutral / forward / back ground jumps, running jumps, and air jumps. Several
/// of these are read by `common1.cns` via `const(velocity.*)` to seed the
/// `velset` of jump/run states; before they were modeled those reads resolved
/// to `0`, so forward/back/run/air jumps all rose straight up.
///
/// MUGEN authors most x-axis jump speeds as bare scalars (e.g. `jump.fwd = 2.5`)
/// whose stored `y` is `0`; the y component of a jump comes from `jump.neu.y`
/// (mirrored into [`jump_up`](Self::jump_up)) or, for air jumps, from
/// `airjump.neu.y` (mirrored into [`airjump_y`](Self::airjump_y)).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VelocityConstants {
    /// `walk.fwd` — forward walking velocity `(x, y)`. MUGEN authors this as a
    /// bare x value; `y` is `0`.
    pub walk_fwd: Vec2<f32>,
    /// `walk.back` — backward walking velocity `(x, y)` (x is negative).
    pub walk_back: Vec2<f32>,
    /// `run.fwd` — forward running velocity `(x, y)`.
    pub run_fwd: Vec2<f32>,
    /// `run.back` — backward running (hop) velocity `(x, y)` (x is negative; KFM
    /// gives it a negative `y`, i.e. an upward hop).
    pub run_back: Vec2<f32>,
    /// `jump.neu` — neutral jump velocity `(x, y)` (y is negative = upward).
    pub jump_neu: Vec2<f32>,
    /// `jump.fwd` — forward ground-jump velocity `(x, y)`. MUGEN authors this as
    /// a bare x value; the jump's y comes from [`jump_up`](Self::jump_up).
    pub jump_fwd: Vec2<f32>,
    /// `jump.back` — backward ground-jump velocity `(x, y)` (x is negative). The
    /// jump's y comes from [`jump_up`](Self::jump_up).
    pub jump_back: Vec2<f32>,
    /// `runjump.fwd` — forward running-jump velocity `(x, y)`.
    pub runjump_fwd: Vec2<f32>,
    /// `runjump.back` — backward running-jump velocity `(x, y)` (x is negative).
    pub runjump_back: Vec2<f32>,
    /// `airjump.neu` — neutral air-jump velocity `(x, y)` (y is negative =
    /// upward).
    pub airjump_neu: Vec2<f32>,
    /// `airjump.fwd` — forward air-jump velocity `(x, y)`. MUGEN authors this as
    /// a bare x value; the air-jump y comes from [`airjump_y`](Self::airjump_y).
    pub airjump_fwd: Vec2<f32>,
    /// `airjump.back` — backward air-jump velocity `(x, y)` (x is negative). The
    /// air-jump y comes from [`airjump_y`](Self::airjump_y).
    pub airjump_back: Vec2<f32>,
    /// `jump.up` y-velocity — the upward jump speed. MUGEN derives jump y from
    /// `jump.neu.y`; when an explicit `jump.up` is absent this mirrors
    /// `jump_neu.y`.
    pub jump_up: f32,
    /// `airjump.y` y-velocity — the upward air-jump speed. MUGEN derives air-jump
    /// y from `airjump.neu.y`; when an explicit `airjump.y` is absent this
    /// mirrors `airjump_neu.y`. Read by `common1` as `const(velocity.airjump.y)`.
    pub airjump_y: f32,
}

impl Default for VelocityConstants {
    fn default() -> Self {
        // KFM's authored values (kfm.cns [Velocity]).
        Self {
            walk_fwd: Vec2::new(2.4, 0.0),
            walk_back: Vec2::new(-2.2, 0.0),
            run_fwd: Vec2::new(4.6, 0.0),
            run_back: Vec2::new(-4.5, -3.8),
            jump_neu: Vec2::new(0.0, -8.4),
            jump_fwd: Vec2::new(2.5, 0.0),
            jump_back: Vec2::new(-2.55, 0.0),
            runjump_fwd: Vec2::new(4.0, -8.1),
            runjump_back: Vec2::new(-2.55, -8.1),
            airjump_neu: Vec2::new(0.0, -8.1),
            airjump_fwd: Vec2::new(2.5, 0.0),
            airjump_back: Vec2::new(-2.55, 0.0),
            jump_up: -8.4,
            airjump_y: -8.1,
        }
    }
}

/// The `[Movement]` constant group: gravity and friction.
///
/// `yaccel` is the per-tick downward acceleration applied by air physics
/// (`Physics::Air`). `stand.friction`/`crouch.friction` are the multiplicative
/// coefficients applied to x-velocity each tick by stand/crouch physics.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MovementConstants {
    /// `yaccel` — downward acceleration in pixels/tick² (gravity).
    pub yaccel: f32,
    /// `stand.friction` — x-velocity multiplier per tick while standing.
    pub stand_friction: f32,
    /// `crouch.friction` — x-velocity multiplier per tick while crouching.
    pub crouch_friction: f32,
    /// `stand.friction.threshold` — speed below which a standing player snaps to a
    /// stop. common1 gates its idle-stop (`VelSet x=0`) and return-to-idle on
    /// `abs(vel x) < Const(movement.stand.friction.threshold)`.
    pub stand_friction_threshold: f32,
    /// `crouch.friction.threshold` — stop threshold while crouching.
    pub crouch_friction_threshold: f32,
    /// `down.friction.threshold` — stop threshold while lying down.
    pub down_friction_threshold: f32,
    /// `airjump.num` — how many **air jumps** (double/multi jumps) the character
    /// may perform before touching the ground again.
    ///
    /// MUGEN's `[Movement] airjump.num`. `0` (the default when the key is absent)
    /// means the character has **no** air jump: the air-jump engine built-in is
    /// gated off entirely. KFM authors `airjump.num = 1` (a single double jump).
    /// The engine resets the per-character air-jump counter to `0` whenever the
    /// character is grounded, so a fresh ground jump restores the full allowance.
    pub airjump_num: i32,
    /// `airjump.height` — the minimum height **above the floor** (in pixels) the
    /// character must have risen before an air jump is permitted.
    ///
    /// MUGEN's `[Movement] airjump.height`. Because the world floor is `Y = 0`
    /// and up is the negative-Y direction, the engine permits an air jump only
    /// when `pos.y <= -airjump_height` (i.e. the character is at least this many
    /// pixels off the ground). `0` (the default when the key is absent) imposes no
    /// minimum height. KFM authors `airjump.height = 35`.
    pub airjump_height: f32,
}

impl Default for MovementConstants {
    fn default() -> Self {
        // KFM's authored [Movement] values, except the air-jump fields which
        // default to MUGEN's "no air jump" baseline (`0`) when a character omits
        // them — KFM's own `airjump.num = 1` / `airjump.height = 35` are supplied
        // by the loader from `[Movement]`.
        Self {
            yaccel: 0.44,
            stand_friction: 0.85,
            crouch_friction: 0.82,
            stand_friction_threshold: 2.0,
            crouch_friction_threshold: 0.05,
            down_friction_threshold: 0.05,
            airjump_num: 0,
            airjump_height: 0.0,
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

/// The per-tick `AssertSpecial` flag set (faithfulness audit #13).
///
/// MUGEN's `AssertSpecial` controller asserts a named engine flag *for the
/// current tick only*: the assertion holds while the state that asserts it is
/// running and is **cleared at the start of every tick**, so a flag must be
/// re-asserted each tick to stay set. The executor clears this set at the top of
/// [`Character::tick`] and the `AssertSpecial` dispatch arm sets the named flags
/// during the tick; consumers (built-in walk locomotion, auto-turn / face-opponent
/// logic, intro gating) read it back the same tick.
///
/// Only the flags Fighters Paradise currently consumes have named fields
/// (`NoWalk`, `NoAutoTurn`, `Intro`). Any other asserted flag name (MUGEN has
/// ~30, e.g. `NoBarDisplay`, `Invisible`, `NoShadow`) is stored verbatim in
/// [`others`](Self::others) so [`is_asserted`](Self::is_asserted) can report it
/// without the engine needing a field per flag — an unmodeled flag is recorded,
/// never dropped, and simply has no consumer yet (a safe no-op).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertedFlags {
    /// `NoWalk` — suppress the engine's built-in stand↔walk locomotion this tick
    /// (common1 run state 100 asserts this so a run does not fall back to a walk).
    pub no_walk: bool,
    /// `NoAutoTurn` — suppress automatic turning to face the opponent this tick
    /// (common1 run state 100 asserts this; a turning run would look wrong).
    pub no_auto_turn: bool,
    /// `Intro` — the character is in its intro pose (kfm.cns asserts this during
    /// its intro state). Consumers may gate intro-only behavior on it.
    pub intro: bool,
    /// Any other asserted flag name (lower-cased), stored verbatim so it is not
    /// silently dropped even though no subsystem consumes it yet.
    pub others: Vec<String>,
}

impl AssertedFlags {
    /// Clears every asserted flag — called at the **start** of each tick so an
    /// assertion only holds for the tick that (re-)asserts it.
    pub fn clear(&mut self) {
        self.no_walk = false;
        self.no_auto_turn = false;
        self.intro = false;
        self.others.clear();
    }

    /// Records an asserted flag by its MUGEN name (case-insensitive).
    ///
    /// Known flags (`NoWalk`, `NoAutoTurn`, `Intro`) set their dedicated field;
    /// any other name is appended to [`others`](Self::others) (de-duplicated)
    /// rather than dropped. Never panics.
    pub fn assert(&mut self, flag: &str) {
        let f = flag.trim();
        if f.eq_ignore_ascii_case("NoWalk") {
            self.no_walk = true;
        } else if f.eq_ignore_ascii_case("NoAutoTurn") {
            self.no_auto_turn = true;
        } else if f.eq_ignore_ascii_case("Intro") {
            self.intro = true;
        } else {
            let lower = f.to_ascii_lowercase();
            if !self.others.iter().any(|o| o == &lower) {
                self.others.push(lower);
            }
        }
    }

    /// Returns `true` if the named flag is currently asserted (case-insensitive),
    /// checking both the dedicated fields and the [`others`](Self::others) catch-all.
    #[must_use]
    pub fn is_asserted(&self, flag: &str) -> bool {
        let f = flag.trim();
        if f.eq_ignore_ascii_case("NoWalk") {
            self.no_walk
        } else if f.eq_ignore_ascii_case("NoAutoTurn") {
            self.no_auto_turn
        } else if f.eq_ignore_ascii_case("Intro") {
            self.intro
        } else {
            let lower = f.to_ascii_lowercase();
            self.others.iter().any(|o| o == &lower)
        }
    }
}

/// A per-state push/collision **width override** set by the MUGEN `Width`
/// controller (faithfulness audit #10).
///
/// MUGEN's `Width` controller overrides the player-push half-widths for the
/// current state (e.g. a crouch or an attack that should push differently, or a
/// throw-bind state that pins the victim). Like other per-tick controllers it is
/// transient: the executor **clears** it at the start of each tick, so a state
/// that wants a sustained override must re-assert `Width` every tick (which
/// MUGEN content does, gating it on the state being active).
///
/// `front`/`back` are facing-relative half-widths (front = toward the direction
/// the character faces). When [`active`](Self::active) is `false` the engine
/// falls back to the static `[Size] ground.front`/`ground.back` constants.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct WidthOverride {
    /// Whether a `Width` override is in effect this tick. When `false`, push/bounds
    /// use the static `[Size]` width.
    pub active: bool,
    /// Front (facing-forward) push half-width, in pixels.
    pub front: f32,
    /// Back (facing-backward) push half-width, in pixels.
    pub back: f32,
}

impl WidthOverride {
    /// Clears the override — called at the start of each tick so a `Width` only
    /// holds for the tick that (re-)asserts it.
    pub fn clear(&mut self) {
        self.active = false;
        self.front = 0.0;
        self.back = 0.0;
    }

    /// Sets the override to the given facing-relative half-widths and marks it
    /// active for this tick.
    pub fn set(&mut self, front: f32, back: f32) {
        self.active = true;
        self.front = front;
        self.back = back;
    }
}

/// A live MUGEN-style color tint the character is currently displaying (the
/// `PalFX` controller's effect; faithfulness audit #33, full modulation set
/// T008).
///
/// MUGEN's `PalFX` recolors the character for a number of ticks with the full
/// modulation set: a per-channel signed `add`, a per-channel `mul`tiply, a
/// grayscale-blend `color` (`256` = full color, `0` = grayscale), a per-channel
/// **sinusoidal** add `sinadd` that oscillates over a `sinadd_period`, and an
/// `invertall` channel inversion. The static values are normalized here to the
/// renderer's float scale — `add`/`sinadd` are signed fractions (`±1.0` = ±255
/// MUGEN units), `mul` a plain multiplier (`1.0` = unchanged), and `color` a
/// `0.0..=1.0` color-retention fraction.
///
/// The static `add`/`mul`/`color`/`invertall` are constant for the effect's
/// lifetime, but `sinadd` makes the **effective** add change every tick — that
/// per-frame coefficient is computed by [`effective`](Self::effective) from the
/// [`elapsed`](Self::elapsed) tick counter, and [`Character::palfx`] returns
/// that resolved value (no phase reaches the renderer).
///
/// [`remaining`](Self::remaining) is the countdown in ticks; while `> 0` the
/// effect is active and ticks down each (non-hit-paused) frame, with
/// [`elapsed`](Self::elapsed) counting up in lock-step to drive the `sinadd`
/// phase. A [`Default`]/[`IDENTITY`](Self::IDENTITY) effect (`remaining = 0`) is
/// a no-op: [`Character::palfx`] returns the identity tint, so the sprite
/// renders unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CurPalFx {
    /// Signed per-channel static add, as a fraction of full scale (`±1.0` =
    /// ±255). The `sinadd` oscillation is added on top of this per tick.
    pub add: [f32; 3],
    /// Per-channel multiply (`1.0` = unchanged).
    pub mul: [f32; 3],
    /// Color-retention fraction in `0.0..=1.0` (`1.0` = full color, `0.0` =
    /// grayscale). Mirrors MUGEN's `PalFX color = 0..256`.
    pub color: f32,
    /// Per-channel sinusoidal add **amplitude**, as a fraction of full scale
    /// (`±1.0` = ±255). At tick `t` the add contribution is `sinadd[c] *
    /// sin(2π · t / sinadd_period)`. `[0.0; 3]` (the default) oscillates nothing.
    pub sinadd: [f32; 3],
    /// Period of the `sinadd` oscillation in ticks (MUGEN `sinadd = r,g,b,period`).
    /// `0` (or the absence of `sinadd`) disables the oscillation. A negative
    /// period inverts the sine (MUGEN convention).
    pub sinadd_period: i32,
    /// MUGEN `PalFX invertall`: when `true`, each channel is inverted
    /// (`1.0 - channel`) after the grayscale blend and before the multiply/add.
    pub invertall: bool,
    /// Remaining active duration in ticks. `0` (or less) = inactive / no-op.
    pub remaining: i32,
    /// Ticks elapsed since the effect was armed, driving the `sinadd` phase.
    /// Counts up each active tick (alongside [`remaining`](Self::remaining)
    /// counting down). Starts at `0`, so the first frame's `sinadd` phase is
    /// `sin(0) = 0`.
    pub elapsed: i32,
}

impl CurPalFx {
    /// The identity (no-op) tint: full color, unit multiply, zero add, no
    /// oscillation, no inversion, not active. [`Character::palfx`] returns this
    /// when nothing is active.
    pub const IDENTITY: Self = Self {
        add: [0.0, 0.0, 0.0],
        mul: [1.0, 1.0, 1.0],
        color: 1.0,
        sinadd: [0.0, 0.0, 0.0],
        sinadd_period: 0,
        invertall: false,
        remaining: 0,
        elapsed: 0,
    };

    /// Returns `true` while the effect is still running ([`remaining`](Self::remaining)
    /// `> 0`).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.remaining > 0
    }

    /// The per-tick **effective** signed add for the current
    /// [`elapsed`](Self::elapsed) frame: the static [`add`](Self::add) plus the
    /// `sinadd` oscillation `sinadd[c] · sin(2π · elapsed / sinadd_period)`.
    ///
    /// When `sinadd_period` is `0` the oscillation is disabled and this returns
    /// the static add unchanged (so it is a no-op for a plain `PalFX`). Never
    /// panics (division is guarded; non-finite results fall back to the static
    /// add).
    #[must_use]
    pub fn effective_add(&self) -> [f32; 3] {
        if self.sinadd_period == 0 {
            return self.add;
        }
        let phase = std::f32::consts::TAU * (self.elapsed as f32) / (self.sinadd_period as f32);
        let s = phase.sin();
        let mut out = self.add;
        for (slot, &amp) in out.iter_mut().zip(self.sinadd.iter()) {
            let v = *slot + amp * s;
            if v.is_finite() {
                *slot = v;
            }
        }
        out
    }

    /// The effect resolved for the current tick: a copy with the `sinadd`
    /// oscillation folded into [`add`](Self::add) (so [`effective_add`] has
    /// already been applied) and the oscillation parameters cleared. This is the
    /// value handed to the renderer, which only ever sees a resolved per-frame
    /// add — never a phase.
    ///
    /// [`effective_add`]: Self::effective_add
    #[must_use]
    pub fn effective(&self) -> Self {
        Self {
            add: self.effective_add(),
            sinadd: [0.0; 3],
            sinadd_period: 0,
            ..*self
        }
    }

    /// Counts the effect down by one tick (and the `sinadd` phase up by one),
    /// expiring it (resetting to the identity tint) when [`remaining`](Self::remaining)
    /// reaches zero. Called once per non-hit-paused tick.
    pub fn tick(&mut self) {
        if self.remaining > 0 {
            self.remaining -= 1;
            self.elapsed = self.elapsed.saturating_add(1);
            if self.remaining <= 0 {
                *self = Self::IDENTITY;
            }
        }
    }
}

impl Default for CurPalFx {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// The blend mode applied to every `AfterImage` ghost (MUGEN `AfterImage trans`).
///
/// MUGEN's `AfterImage trans` selects how the whole trail is composited over the
/// background; the renderer (`fp-app`) maps each variant onto its
/// `fp_render::BlendMode`. `None` (the MUGEN default for `AfterImage`) draws the
/// ghosts with ordinary alpha blending; `Add`/`Add1`/`Sub` request additive /
/// half-additive / subtractive compositing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TrailBlend {
    /// `trans = none` — ordinary alpha blending (the MUGEN `AfterImage` default).
    #[default]
    None,
    /// `trans = add` (or `addalpha`) — additive blending.
    Add,
    /// `trans = add1` — half-strength additive blending.
    Add1,
    /// `trans = sub` — subtractive blending.
    Sub,
}

impl TrailBlend {
    /// Parses a MUGEN `AfterImage trans` token (case-insensitive), falling back to
    /// [`TrailBlend::None`] for an empty or unrecognised value (never panics).
    #[must_use]
    pub fn parse(token: &str) -> Self {
        let t = token.trim();
        if t.eq_ignore_ascii_case("add") || t.eq_ignore_ascii_case("addalpha") {
            Self::Add
        } else if t.eq_ignore_ascii_case("add1") {
            Self::Add1
        } else if t.eq_ignore_ascii_case("sub") {
            Self::Sub
        } else {
            Self::None
        }
    }
}

/// A single captured frame in an [`AfterImageState`]'s history ring (T007).
///
/// Records exactly what the renderer needs to redraw a *past* frame of the
/// character: which sprite was showing ([`anim`](Self::anim) /
/// [`anim_elem`](Self::anim_elem)), where the axis was ([`pos`](Self::pos)), and
/// which way it faced ([`facing`](Self::facing)). The per-ghost color modulation
/// (`PalBright`/`PalContrast`/`PalAdd`/`PalMul`) is **not** stored per frame — it
/// is a function of the ghost's *index* in the trail (older = more modulated),
/// computed by the renderer, so the ring only holds the geometry/sprite identity.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AfterImageFrame {
    /// The animation (action) id that was current when this frame was captured.
    pub anim: i32,
    /// The zero-based animation element index that was current when captured.
    pub anim_elem: i32,
    /// The character axis (world position, pixels) at capture time.
    pub pos: Vec2<f32>,
    /// Which way the character faced at capture time.
    pub facing: Facing,
}

/// The live `AfterImage` trail effect a character is displaying (faithfulness
/// audit #33; true frame-history ring, T007).
///
/// MUGEN's `AfterImage` draws a fading trail of the character's recent frames
/// behind the sprite for [`time`](Self::time) ticks; `AfterImageTime` re-arms or
/// cancels the duration. This models the real behaviour: a **frame-history ring**
/// ([`frames`](Self::frames)) holds up to [`length`](Self::length) past frame
/// snapshots, a new one captured every [`timegap`](Self::timegap) ticks; the
/// renderer draws every [`framegap`](Self::framegap)-th retained frame as a
/// ghost, composited with [`trans`](Self::trans) and progressively modulated by
/// the trail's [`palfx`](Self::palfx) base tint plus the per-ghost
/// [`palbright`](Self::palbright) / [`palcontrast`](Self::palcontrast) ramps.
/// While [`time`](Self::time) `> 0` the trail is active and counts down each
/// (non-hit-paused) tick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AfterImageState {
    /// Remaining trail duration in ticks. `0` (or less) = inactive.
    pub time: i32,
    /// Maximum number of past frames retained in [`frames`](Self::frames) (MUGEN
    /// `length`, default `20`, clamped to a sane small cap by the controller).
    pub length: i32,
    /// How many ticks elapse between successive frame captures (MUGEN `timegap`,
    /// default `1` = capture every tick). Clamped to `>= 1` when used.
    pub timegap: i32,
    /// How many retained frames the renderer steps between drawn ghosts (MUGEN
    /// `framegap`, default `4`). Clamped to `>= 1` when used.
    pub framegap: i32,
    /// The blend mode the renderer composites the ghosts with (MUGEN `trans`).
    pub trans: TrailBlend,
    /// The base color tint applied to the (newest) trail ghost — the controller's
    /// `PalAdd`/`PalMul`, folded into a single [`CurPalFx`].
    pub palfx: CurPalFx,
    /// Per-ghost additive brightness ramp (MUGEN `PalBright = r,g,b`, signed
    /// `0..255` normalized to a `±1.0` fraction). Added once per ghost step, so
    /// the `n`-th ghost gets `n ×` this offset on top of [`palfx`](Self::palfx).
    pub palbright: [f32; 3],
    /// Per-ghost multiplicative contrast ramp (MUGEN `PalContrast = r,g,b`, a
    /// `0..255` value normalized so `255` → `1.0`). Multiplied once per ghost
    /// step, so the `n`-th ghost gets [`palfx`](Self::palfx)`.mul × contrast^n`.
    pub palcontrast: [f32; 3],
    /// Internal capture-cadence counter: counts up each active tick, and a frame
    /// is pushed into [`frames`](Self::frames) when it reaches
    /// [`timegap`](Self::timegap). Not part of the public draw contract.
    pub timegap_counter: i32,
    /// The frame-history ring, newest-first: `frames[0]` is the most recently
    /// captured frame. Bounded to [`length`](Self::length) entries.
    pub frames: Vec<AfterImageFrame>,
}

impl AfterImageState {
    /// The inactive default: no trail, empty history.
    #[must_use]
    pub fn inactive() -> Self {
        Self {
            time: 0,
            length: 0,
            timegap: 1,
            framegap: 4,
            trans: TrailBlend::None,
            palfx: CurPalFx::IDENTITY,
            palbright: [0.0; 3],
            palcontrast: [1.0; 3],
            timegap_counter: 0,
            frames: Vec::new(),
        }
    }

    /// Returns `true` while the trail is still running ([`time`](Self::time) `> 0`).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.time > 0
    }

    /// Pushes a freshly captured frame onto the front of the history ring,
    /// dropping the oldest frame(s) so the ring never exceeds
    /// [`length`](Self::length). A non-positive `length` retains nothing.
    pub fn push_frame(&mut self, frame: AfterImageFrame) {
        if self.length <= 0 {
            self.frames.clear();
            return;
        }
        self.frames.insert(0, frame);
        let cap = self.length as usize;
        if self.frames.len() > cap {
            self.frames.truncate(cap);
        }
    }

    /// Captures the given frame into the ring **iff** this tick lands on the
    /// configured [`timegap`](Self::timegap) cadence, advancing the cadence
    /// counter. Called once per active, non-hit-paused tick. A frame is recorded
    /// every `timegap` ticks (counter reaching `timegap`); intermediate ticks only
    /// advance the counter, so the ring grows one frame per `timegap` ticks. A
    /// no-op when the trail is inactive.
    pub fn capture_on_cadence(&mut self, frame: AfterImageFrame) {
        if !self.is_active() {
            return;
        }
        let gap = self.timegap.max(1);
        self.timegap_counter += 1;
        if self.timegap_counter >= gap {
            self.timegap_counter = 0;
            self.push_frame(frame);
        }
    }

    /// The ghost frames the renderer should actually draw, newest-first: starts at
    /// the newest retained frame and steps [`framegap`](Self::framegap) frames
    /// between successive ghosts (MUGEN draws every `framegap`-th history frame).
    /// Returns at most [`length`](Self::length) entries; empty when no history is
    /// retained yet.
    #[must_use]
    pub fn ghost_frames(&self) -> Vec<AfterImageFrame> {
        let step = self.framegap.max(1) as usize;
        self.frames.iter().step_by(step).copied().collect()
    }

    /// Counts the trail down by one tick, clearing it (and its history) when it
    /// expires. Called once per non-hit-paused tick.
    pub fn tick(&mut self) {
        if self.time > 0 {
            self.time -= 1;
            if self.time <= 0 {
                *self = Self::inactive();
            }
        }
    }
}

impl Default for AfterImageState {
    fn default() -> Self {
        Self::inactive()
    }
}

/// A live screen-shake effect armed by the `EnvShake` state controller (T015).
///
/// MUGEN's `EnvShake` shakes the camera for [`time`](Self::time) ticks at a
/// vertical [`ampl`](Self::ampl)itude (pixels) and [`freq`](Self::freq)uency,
/// optionally [`phase`](Self::phase)-offset. The screen shake itself is a
/// camera-level effect owned by the renderer (`fp-app`), so this models the
/// controllable parameters and counts the duration down each tick; the renderer
/// reads [`Character::env_shake`] and offsets the camera while it is active.
/// Inactive ([`time`](Self::time) `<= 0`) by default.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EnvShake {
    /// Remaining shake duration in ticks. `0` (or less) = inactive.
    pub time: i32,
    /// Shake frequency, MUGEN units `0..180` (degrees of phase advance per tick).
    pub freq: f32,
    /// Vertical shake amplitude in pixels (MUGEN default `-4`).
    pub ampl: f32,
    /// Phase offset in degrees (MUGEN default `0`).
    pub phase: f32,
}

impl EnvShake {
    /// The inactive default: no shake.
    pub const INACTIVE: Self = Self {
        time: 0,
        freq: 60.0,
        ampl: -4.0,
        phase: 0.0,
    };

    /// Returns `true` while the shake is still running ([`time`](Self::time) `> 0`).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.time > 0
    }

    /// Counts the shake down by one tick, clearing it when it expires. Called
    /// once per non-hit-paused tick.
    pub fn tick(&mut self) {
        if self.time > 0 {
            self.time -= 1;
            if self.time <= 0 {
                *self = Self::INACTIVE;
            }
        }
    }
}

impl Default for EnvShake {
    fn default() -> Self {
        Self::INACTIVE
    }
}

/// A live full-screen color flash armed by the `EnvColor` state controller (T015).
///
/// MUGEN's `EnvColor` fills the screen with a solid [`col`](Self::col)or for
/// [`time`](Self::time) ticks (the classic "super flash" white-out). `under`
/// selects whether the fill is drawn *under* the characters (`true`) or over
/// everything (`false`, the default). This is a screen-level effect owned by the
/// renderer (`fp-app`); this models the parameters and counts the duration down.
/// Inactive ([`time`](Self::time) `== 0`) by default.
///
/// MUGEN's `EnvColor time = -1` ("persist until cleared") is represented
/// **faithfully**: [`time`](Self::time) is held at the sentinel
/// [`PERSISTENT`](Self::PERSISTENT) (`-1`) and never counts down — only an
/// explicit clear (a `time = 0` `EnvColor`, written by the controller as
/// [`INACTIVE`](Self::INACTIVE)) ends it, exactly as in MUGEN.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvColor {
    /// Remaining fill duration in ticks. `0` = inactive; a positive value counts
    /// down each tick; [`PERSISTENT`](Self::PERSISTENT) (`-1`) means "fill until
    /// explicitly cleared" and never decrements (MUGEN's `time = -1`).
    pub time: i32,
    /// The fill color as a `[r, g, b]` triple in `0..=255` (MUGEN default white).
    pub col: [u8; 3],
    /// Draw the fill *under* the characters (`true`) or over everything
    /// (`false`, MUGEN's default).
    pub under: bool,
}

impl EnvColor {
    /// The inactive default: no fill.
    pub const INACTIVE: Self = Self {
        time: 0,
        col: [255, 255, 255],
        under: false,
    };

    /// The [`time`](Self::time) sentinel for MUGEN's `time = -1`: the fill
    /// persists until explicitly cleared and never counts down.
    pub const PERSISTENT: i32 = -1;

    /// Returns `true` while the fill is showing — either still counting down
    /// ([`time`](Self::time) `> 0`) or persistent ([`PERSISTENT`](Self::PERSISTENT)).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.time > 0 || self.time == Self::PERSISTENT
    }

    /// Counts the fill down by one tick, clearing it when it expires. Called once
    /// per non-hit-paused tick. A [`PERSISTENT`](Self::PERSISTENT) fill is left
    /// untouched — only an explicit clear ends it (MUGEN's `time = -1`).
    pub fn tick(&mut self) {
        if self.time > 0 {
            self.time -= 1;
            if self.time <= 0 {
                *self = Self::INACTIVE;
            }
        }
    }
}

impl Default for EnvColor {
    fn default() -> Self {
        Self::INACTIVE
    }
}

/// A palette-remap selection armed by the `RemapPal` state controller (T015).
///
/// MUGEN's `RemapPal` swaps the character's palette to a different `(group, item)`
/// in the palette table (`source = sg, si` → `dest = dg, di`), used for alternate
/// costume colors mid-match. We model the selection so the renderer can pick the
/// matching loaded palette; a `dest` of `(-1, -1)` (or a [`None`]) restores the
/// default palette. The actual palette pixels live on the static
/// [`LoadedCharacter`](crate::LoadedCharacter), so this stores only the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RemapPal {
    /// The source palette `(group, item)`, or `None` for the sprite default.
    pub source: Option<(i32, i32)>,
    /// The destination palette `(group, item)`, or `None` to restore the default.
    pub dest: Option<(i32, i32)>,
}

impl RemapPal {
    /// Returns `true` while a non-default remap is selected.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.dest.is_some()
    }
}

/// The number of `HitOverride` slots a character carries (MUGEN's `slot = 0..7`).
pub const NUM_HIT_OVERRIDE_SLOTS: usize = 8;

/// One armed `HitOverride` slot (faithfulness audit #9b).
///
/// MUGEN's `HitOverride` controller arms one of a character's 8 slots so that, for
/// a number of ticks (`time`), an incoming hit whose attacker `attr` matches the
/// slot's attribute set redirects the **defender** to a custom `stateno` instead
/// of running the normal get-hit reaction. This is how characters implement armor,
/// dodges, and counters ("if hit by a normal during this window, go to my counter
/// state").
///
/// A slot is **active** while [`time_remaining`](Self::time_remaining) `> 0` (or
/// `< 0`, MUGEN's "until cleared / forever" sentinel — modeled as "always active";
/// see [`is_active`](Self::is_active)). When an active slot's
/// [`attrs`](Self::attrs) match the attacker's
/// [`AttackAttr`](fp_combat::AttackAttr), hit resolution
/// ([`combat::resolve_attack`](crate::combat::resolve_attack)) sends the defender
/// to [`stateno`](Self::stateno) and consumes the slot, *bypassing* the normal
/// get-hit path (no damage/knockback/get-hit-state is applied — MUGEN treats the
/// override state as fully taking over the reaction).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HitOverrideSlot {
    /// The parsed attack-attribute set that arms this slot (the `attr` param).
    /// Reuses the `NotHitBy`/`HitBy` grammar via [`AttackAttrSet`].
    pub attrs: AttackAttrSet,
    /// The state number the defender is sent to when this slot matches.
    pub stateno: i32,
    /// Remaining ticks this slot stays armed. `0` = inactive; `> 0` counts down;
    /// `< 0` is MUGEN's "stay armed until consumed/replaced" sentinel
    /// (always active).
    pub time_remaining: i32,
}

impl HitOverrideSlot {
    /// Returns `true` while this slot is armed — `time_remaining != 0` (a positive
    /// countdown or the negative "forever" sentinel). A `0` slot is inactive.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.time_remaining != 0
    }

    /// Decrements a positive countdown by one tick (saturating at `0`). A `< 0`
    /// "forever" slot and an already-`0` slot are left untouched. Never panics.
    pub fn decrement(&mut self) {
        if self.time_remaining > 0 {
            self.time_remaining -= 1;
        }
    }
}

/// A character's 8-slot `HitOverride` table (faithfulness audit #9b).
///
/// Slot indices `0..8` map to MUGEN's `HitOverride slot = N`. The executor's
/// `HitOverride` arm sets a slot; the executor decrements active slots each tick
/// (respecting hit-pause is unnecessary — MUGEN counts these down normally — but
/// they are ticked alongside the other per-tick timers); and hit resolution
/// consults the slots **before** the normal get-hit, redirecting and consuming the
/// first matching active slot.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HitOverrides {
    /// The 8 override slots, indexed by MUGEN `slot` number.
    pub slots: [HitOverrideSlot; NUM_HIT_OVERRIDE_SLOTS],
}

impl HitOverrides {
    /// Arms slot `slot` with the given attribute set, destination state, and
    /// duration. An out-of-range `slot` index is a safe no-op (debug-logged),
    /// matching MUGEN's tolerance — it never panics.
    pub fn arm(&mut self, slot: usize, attrs: AttackAttrSet, stateno: i32, time: i32) {
        match self.slots.get_mut(slot) {
            Some(s) => {
                s.attrs = attrs;
                s.stateno = stateno;
                s.time_remaining = time;
            }
            None => {
                tracing::debug!("HitOverride: slot index {slot} out of range 0..8; ignored");
            }
        }
    }

    /// Finds the first **active** slot whose attribute set matches `attr`, returning
    /// its index and destination `stateno`, or `None` when no slot overrides this
    /// hit. Lower slot indices take priority (scanned in order).
    #[must_use]
    pub fn matching(&self, attr: &fp_combat::AttackAttr) -> Option<(usize, i32)> {
        self.slots
            .iter()
            .enumerate()
            .find_map(|(i, s)| (s.is_active() && s.attrs.matches(attr)).then_some((i, s.stateno)))
    }

    /// Consumes (disarms) the slot at `index` after a successful override match —
    /// MUGEN clears a `HitOverride` once it fires. Out-of-range is a no-op.
    pub fn consume(&mut self, index: usize) {
        if let Some(s) = self.slots.get_mut(index) {
            s.time_remaining = 0;
        }
    }

    /// Advances all positive-countdown slots by one tick. The `< 0` "forever"
    /// slots and `0` inactive slots are untouched. Never panics.
    pub fn tick(&mut self) {
        for s in &mut self.slots {
            s.decrement();
        }
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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
    /// `GetHitVar(fall.xvel)` — the X fall velocity the hit imparts, read by the
    /// `HitFallVel` controller (audit #23).
    pub fall_xvel: f32,
    /// `GetHitVar(fall.yvel)` — the Y fall velocity the hit imparts (negative =
    /// upward), read by the `HitFallVel` controller (audit #23). Populated from the
    /// HitDef's `fall.yvelocity` on a falling hit.
    pub fall_yvel: f32,
    /// `GetHitVar(fall.damage)` — extra damage applied when the defender lands from
    /// the fall, applied by the `HitFallDamage` controller (audit #23).
    pub fall_damage: i32,
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
            fall_xvel: 0.0,
            fall_yvel: 0.0,
            fall_damage: 0,
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
        } else if m.eq_ignore_ascii_case("fall.xvel") {
            Value::Float(self.fall_xvel)
        } else if m.eq_ignore_ascii_case("fall.yvel") {
            Value::Float(self.fall_yvel)
        } else if m.eq_ignore_ascii_case("fall.damage") {
            Value::Int(self.fall_damage)
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

    /// Per-element cumulative start-offset table for the **current** animation
    /// action, used to answer `AnimElemTime(n)` for *any* element of the action
    /// (not just the current one).
    ///
    /// `anim_elem_start_offsets[i]` is the number of ticks, measured from the
    /// start of the current loop iteration of the action, at which element `i`
    /// (zero-based) begins: element `0` starts at `0`, element `i` at the sum of
    /// the `ticks` durations of elements `0..i`. The executor (re)builds this
    /// from the AIR action's frame durations in
    /// [`advance_animation`](executor) whenever the active action number
    /// changes, and never touches it for an action with no frames (the vector is
    /// then empty and `AnimElemTime` falls back to the legacy scalar).
    ///
    /// Combined with [`anim_elem`](Character::anim_elem) and
    /// [`anim_elem_time`](Character::anim_elem_time) this lets `AnimElemTime(n)`
    /// compute *time-since-element-n began* for the current loop iteration:
    /// `elapsed_in_action = start_offset[anim_elem] + anim_elem_time`, then
    /// `AnimElemTime(n) = elapsed_in_action - start_offset[n-1]` (one-based `n`).
    /// For the current element this is exactly `anim_elem_time`; for a
    /// not-yet-reached future element it is negative, matching MUGEN.
    ///
    /// Public only because the entity is struct-based (no hidden state); callers
    /// other than the executor and the `AnimElemTime` trigger should not touch
    /// it — mutating it directly desynchronizes it from the live AIR data.
    pub anim_elem_start_offsets: Vec<i32>,
    /// The action number that [`anim_elem_start_offsets`](Character::anim_elem_start_offsets)
    /// was built for, or `None` before any table has been built.
    ///
    /// The executor compares this against the live [`anim`](Character::anim) in
    /// [`advance_animation`](executor) to decide whether the offset table must
    /// be rebuilt (the action changed) or can be reused (same action, advancing
    /// within it). Treated as opaque bookkeeping; see
    /// [`anim_elem_start_offsets`](Character::anim_elem_start_offsets).
    pub anim_table_action: Option<i32>,

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

    /// Pending in-expression assignments (`var(n) := e`, T036) buffered during a
    /// single expression evaluation, before they are flushed into the real banks.
    ///
    /// MUGEN's `:=` writes a variable in the *middle* of evaluating a trigger or
    /// controller-parameter expression. The VM evaluates against a shared `&self`
    /// (the [`EvalContext`] interface is read-only by design), so the write cannot
    /// land directly on the bank arrays during eval; instead the
    /// [`assign`](EvalContext::assign) hook records it here through interior
    /// mutability. The executor calls [`flush_var_assignments`](Character::flush_var_assignments)
    /// at the top of each tick to drain this log into the real banks. Reads
    /// ([`read_var`](Character::read_var) etc.) consult this overlay *first* so a
    /// later read in the **same** expression already sees an assignment from
    /// earlier in that expression (matching MUGEN's left-to-right evaluation).
    ///
    /// It is **not** serialized: it is always drained to empty between ticks, so a
    /// snapshot taken at a tick boundary captures the flushed bank arrays and
    /// nothing is lost. Each entry is `(bank, index, value)`; last write to a slot
    /// wins (the log is scanned in reverse on read and applied in order on flush).
    var_assignments: RefCell<Vec<(AssignBank, i32, Value)>>,

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

    /// How many **air jumps** (double/multi jumps) this character has already
    /// performed since it last left the ground — the engine's air-jump counter
    /// for the MUGEN air-jump built-in (faithfulness audit P14).
    ///
    /// The air-jump built-in (in the executor) permits an air jump only while
    /// `air_jump_count < constants.movement.airjump_num`, incrementing this on
    /// each air jump it grants. It is reset to `0` whenever the character is
    /// grounded (any non-[`StateType::Air`] state at tick start), so a fresh
    /// ground jump restores the full air-jump allowance. Public only because the
    /// entity is struct-based; callers other than the executor should not touch
    /// it.
    pub air_jump_count: i32,

    /// Whether the **up** direction was active (held) on the *previous* tick,
    /// used by the air-jump built-in to detect a **fresh up-press** (the rising
    /// edge of `holdup`).
    ///
    /// MUGEN's air jump fires only on a *new* up press, never while up is merely
    /// held — otherwise a single held up would burn every air jump on consecutive
    /// frames. The executor computes the rising edge each tick as
    /// `holdup_active && !up_held_prev`, then stores the current `holdup` active
    /// state here for the next tick's comparison. Public only because the entity
    /// is struct-based; callers other than the executor should not touch it.
    pub up_held_prev: bool,

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

    /// Remaining hit-pause / hit-stop ticks for **this** character (MUGEN's
    /// `hitpause_time`); `0` (the default) means "not paused".
    ///
    /// MUGEN freezes both participants for a few ticks when an attack connects
    /// (the "hit stop" / impact freeze) so the impact reads. While `hitpause > 0`
    /// the executor freezes this character for the tick: it does **not** advance
    /// the animation, the state `Time` counter, or physics (velocity/position),
    /// and the only controllers that run are those flagged `ignorehitpause`. The
    /// counter is decremented by one each frozen tick, so a freshly-set
    /// `hitpause = N` lasts exactly `N` ticks; normal advancement resumes on the
    /// tick it reaches `0` (see [`Character::tick`] and the
    /// [`hitpause_time`](Character::hitpause_time) accessor). Set on the attacker
    /// (from `pausetime.p1`) and the defender (from `pausetime.p2`) by hit
    /// resolution ([`resolve_attack`]); a miss pauses neither.
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

    /// Whether this character has a hit-established **target** (the opponent in
    /// 1-v-1) that its `Target*` controllers act on.
    ///
    /// MUGEN's `Target*` controllers (`TargetState`, `TargetBind`,
    /// `TargetLifeAdd`, `TargetFacing`, `TargetVelSet`, `TargetVelAdd`,
    /// `TargetPowerAdd`) operate on the set of players this character has hit;
    /// throws (KFM state 810) use them to drive the victim. Hit resolution
    /// ([`resolve_attack`]) sets this `true` on the **attacker** when its
    /// [`active_hitdef`](Character::active_hitdef) connects — the defender it just
    /// hit becomes the target. While `true`, the `Target*` controllers emit
    /// [`TargetOp`]s onto [`TickReport::target_ops`]; while `false` they are safe
    /// no-ops.
    ///
    /// Lifecycle simplification: in this flat 1-v-1 model the target *is* the
    /// opponent, so once set it stays set. MUGEN's target release (on move end,
    /// or an explicit `TargetState`/`Target` redirect change) is **deferred** —
    /// there is no per-target tracking here, only this single boolean.
    pub has_target: bool,

    /// Per-projectile-id contact / hit / guard timing, the persistent state
    /// behind the `ProjContact<id>` / `ProjHit<id>` / `ProjGuarded<id>` /
    /// `ProjContactTime<id>` / `ProjHitTime<id>` / `ProjGuardedTime<id>` trigger
    /// family (T026).
    ///
    /// Keyed by `projid`; an absent key reports "never" (`-1` time / `0`
    /// boolean). Advanced each tick by [`tick_proj_events`](Self::tick_proj_events)
    /// (so every active counter ages one frame) and updated when one of this
    /// owner's projectiles connects via
    /// [`record_proj_event`](Self::record_proj_event) — the round coordinator
    /// (`fp-engine`) is the writer, since the live projectile slot-map lives with
    /// the [`Player`](../fp_engine/struct.Player.html), not on the self-only
    /// `Character`. The trigger reads (the `EvalContext::trigger` impl) consult
    /// it. Cleared on a round reset alongside the other combat bookkeeping.
    pub proj_events: std::collections::HashMap<i32, ProjContactTracker>,

    /// Runtime attack multiplier (MUGEN `AttackMulSet`). Damage this character
    /// *deals* is scaled by this; default `1.0`. Persists until changed or the
    /// round resets.
    pub attack_mul: f32,

    /// Runtime defence multiplier (MUGEN `DefenceMulSet`). Damage this character
    /// *receives* is scaled by this; default `1.0` (`<1` = armor, `>1` = takes
    /// more). Persists until changed or the round resets.
    pub defence_mul: f32,

    /// Current sprite-draw priority (MUGEN `[Statedef] sprpriority` and the
    /// `SprPriority` controller; faithfulness audit #16).
    ///
    /// Higher values draw **in front of** lower ones; equal priorities keep their
    /// player order. Set from the destination statedef's `sprpriority` header on
    /// every state entry (when present) and overwritten mid-state by the
    /// `SprPriority` controller. The renderer (`fp-app`) orders the two fighters
    /// by this field each frame so a thrower / attacker can pull in front of (or
    /// behind) the opponent. Defaults to `0`. KFM drives it dynamically
    /// (common1's [Statedef 5120] etc.).
    pub cur_sprpriority: i32,

    /// Remaining air-juggle points for hits **this character takes** while
    /// airborne (MUGEN's juggle system; faithfulness audit #16).
    ///
    /// Seeded from [`CharacterConstants::airjuggle`] (KFM's `15`) and refilled
    /// whenever the character is grounded. Each attack that connects on this
    /// character while it is airborne / falling spends the attacker's move
    /// `[Statedef] juggle` cost from this pool; once the pool can no longer pay,
    /// the hit is dropped (the juggle limit prevents an infinite air combo). A
    /// grounded defender is never gated on juggle. See
    /// [`combat::resolve_attack`](crate::combat::resolve_attack).
    pub juggle_points: i32,

    /// The air-juggle points the character's **current move** costs when it lands
    /// on an airborne defender (MUGEN `[Statedef] juggle`; faithfulness audit #16).
    ///
    /// Set from the destination statedef's `juggle` header on every state entry
    /// (`0` when the header is absent — the state is not a juggle-costing move).
    /// Hit resolution ([`combat::resolve_attack`](crate::combat::resolve_attack))
    /// reads it as the cost to charge the **defender's**
    /// [`juggle_points`](Self::juggle_points) pool on an airborne hit. Kept on the
    /// attacker (rather than passed as an argument) so the cross-crate
    /// `resolve_attack` signature stays stable. Public only because the entity is
    /// struct-based; callers other than the executor should not touch it.
    pub cur_juggle_cost: i32,

    /// Whether the `HitDef` controller fired this tick (executor bookkeeping for
    /// `hitdefpersist`, faithfulness audit #16).
    ///
    /// Reset to `false` at the start of each [`Character::tick`] and set `true`
    /// when the `HitDef` controller runs. A same-tick `ChangeState` consults this
    /// so it does **not** clear a HitDef that was just established this frame —
    /// preserving MUGEN's "the HitDef is live for this frame's detection" rule —
    /// while a HitDef carried over from a *prior* tick is still cleared on a state
    /// change (unless `hitdefpersist`). Public only because the entity is
    /// struct-based; callers other than the executor should not touch it.
    pub hitdef_set_this_tick: bool,

    /// The character's attack-attribute invulnerability mask — the `NotHitBy` /
    /// `HitBy` windows (faithfulness audit P9).
    ///
    /// Two independent slots (`value` → slot 1, `value2` → slot 2), each holding
    /// a parsed attack-attribute set, an exclude/include [`mode`](crate::invuln::InvulnSlot::mode),
    /// and a `time_remaining` countdown. The `NotHitBy`/`HitBy` controllers set
    /// the slots; the executor decrements them each tick (respecting hit-pause /
    /// `ignorehitpause`); and hit resolution ([`resolve_attack`]) consults the
    /// **defender's** active slots against the **attacker's**
    /// [`HitDef.attr`](fp_combat::HitDef::attr) before applying a hit, dropping
    /// the hit (it passes through, like MUGEN) when any active slot blocks it.
    /// Defaults to an all-inactive mask (`time_remaining = 0`), which blocks
    /// nothing. See [`crate::invuln`].
    pub invuln: InvulnMask,

    /// The per-tick `AssertSpecial` flag set (faithfulness audit #13).
    ///
    /// Cleared at the **start** of each [`Character::tick`] and re-populated by the
    /// `AssertSpecial` controller during the tick, so an assertion holds only for
    /// the tick that asserts it. Consumed the same tick by the built-in walk
    /// locomotion (`NoWalk`), the auto-turn / face-opponent logic (`NoAutoTurn`),
    /// and intro gating (`Intro`). See [`AssertedFlags`].
    pub asserted: AssertedFlags,

    /// The current-tick player-push **width override** set by the `Width`
    /// controller (faithfulness audit #10).
    ///
    /// Cleared at the start of each tick and set by `Width` during it; when
    /// inactive, player-push / stage-bound clamping fall back to the static
    /// `[Size] ground.front`/`ground.back` constants. See [`WidthOverride`].
    pub cur_width: WidthOverride,

    /// The character's live `PalFX` color tint (faithfulness audit #33).
    ///
    /// Unlike [`cur_width`](Self::cur_width) this is **not** per-tick: the
    /// `PalFX` controller arms it for a `time` window, and the executor counts it
    /// down each non-hit-paused tick. The renderer (`fp-app`) reads
    /// [`Character::palfx`] and passes the active tint into the player's
    /// `fp_render::SpriteDrawParams`; when inactive, `palfx()` returns the
    /// identity (no-op) tint so the sprite renders unchanged. See [`CurPalFx`].
    pub cur_palfx: CurPalFx,

    /// The character's live `AfterImage` trail (faithfulness audit #33).
    ///
    /// Set by the `AfterImage` controller and re-armed/cancelled by
    /// `AfterImageTime`; counts down each non-hit-paused tick. The renderer
    /// (`fp-app`) reads [`Character::afterimage`] and, when active, draws a
    /// fading trail of recent frames behind the sprite. Inactive by default. See
    /// [`AfterImageState`].
    pub afterimage: AfterImageState,

    /// The 8-slot `HitOverride` table (faithfulness audit #9b).
    ///
    /// Unlike [`asserted`](Self::asserted) and [`cur_width`](Self::cur_width),
    /// these slots are **not** per-tick: they stay armed for their `time` window
    /// (counted down each tick), and hit resolution
    /// ([`combat::resolve_attack`](crate::combat::resolve_attack)) consults them
    /// **before** the normal get-hit, redirecting the defender to the slot's
    /// `stateno` on an attribute match and consuming the slot. See [`HitOverrides`].
    pub hit_overrides: HitOverrides,

    /// `true` while this character's **current** get-hit reaction is one that a
    /// [`HitOverride`](HitOverrides) slot redirected — the state behind the
    /// `HitOverridden` trigger (T061).
    ///
    /// Set by hit resolution
    /// ([`combat::resolve_attack`](crate::combat::resolve_attack)) on the tick an
    /// armed slot matches and the defender is sent to the slot's `stateno` instead
    /// of the normal get-hit. It stays `true` for the duration of that overridden
    /// reaction and is cleared when a **normal** (non-overridden) get-hit lands —
    /// the next hit replaces the reaction, so `HitOverridden` reverts to `0`. A
    /// freshly-built character (no hit taken) reports `false`.
    pub hit_overridden: bool,

    /// The raw Park–Miller RNG state backing the MUGEN `random` trigger
    /// (faithfulness audit #28).
    ///
    /// MUGEN's `random` is **not** OS randomness: it is the Park–Miller
    /// "minimal standard" LCG, advanced purely deterministically from a
    /// match-start seed. Keeping the state here — as a plain `i32` rather than an
    /// opaque generator object — means it is part of the entity's normal struct
    /// state and so trivially **serializable for frame-perfect netplay rollback /
    /// replay** (#38): a saved/rolled-back `Character` carries its RNG position
    /// with it, and two peers seeded identically draw an identical sequence.
    ///
    /// Wrapped in a [`Cell`] because the [`EvalContext::random`] seam is
    /// `&self` (triggers evaluate against an immutable context), yet each draw
    /// must advance the state. The cell holds the raw seed in the generator's
    /// valid range `1..=2^31-2`; [`Character::random`] reconstructs an
    /// [`Rng`] from it (a lossless round-trip for any reachable state, since
    /// [`Rng::new`] only normalizes the degenerate `0`), advances it one step,
    /// and stores the new seed back. It is seeded deterministically to
    /// [`DEFAULT_RNG_SEED`] (a *fixed* default — never wall-clock or OS RNG,
    /// which would break determinism); set it via [`Character::seed_rng`] to give
    /// a match its own reproducible stream.
    ///
    /// Public only because the entity is struct-based (no hidden state); prefer
    /// [`Character::seed_rng`] / the `random` trigger over poking it directly.
    pub rng_seed: Cell<i32>,

    /// The engine-global round / match clock this character's `RoundState`,
    /// `GameTime`, `MatchOver`, `RoundNo`, and `RoundsExisted` triggers read
    /// (faithfulness audits #21 and T016).
    ///
    /// These values are owned by the round coordinator (`fp-engine`'s `Match`),
    /// not by a single fighter, so the coordinator pushes the live view onto each
    /// character via [`Character::set_round_view`] **before** ticking it. A bare /
    /// freshly-constructed `Character` carries the [`RoundView::default`]
    /// (`RoundState 0`, `GameTime 0`, match not over, `RoundNo 0`,
    /// `RoundsExisted 0`), so the triggers read their safe defaults when there is
    /// no coordinator in view. See [`RoundView`].
    pub round_view: RoundView,

    /// The engine-assigned AI difficulty level this character runs at, `0` for a
    /// human-controlled fighter and `1..=8` for a CPU opponent (the value the
    /// MUGEN `AILevel` trigger reads — higher = stronger CPU, T052).
    ///
    /// Like [`round_view`](Self::round_view), this is **owned by the round
    /// coordinator** (`fp-engine`'s `Match`/`TeamMatch`), which sets it once at
    /// match construction from the player's input driver (keyboard / gamepad → `0`;
    /// a [`fp_input::CpuAi`] → its [`fp_input::AiDifficulty`] mapped to `1..=8`).
    /// The character never assigns it itself — it is a read-only identity bit the
    /// CNS reads to gate cheap-AI-only behaviour. A bare /
    /// freshly-[`new`](Self::new)'d `Character` (no coordinator) defaults to `0`,
    /// so a human is never mistaken for the CPU. Set it via
    /// [`Character::set_ai_level`] and read it via [`Character::ai_level`].
    pub ai_level: u8,

    /// The selected external `.act` palette override, as a 0-based index into the
    /// owning [`LoadedCharacter::palettes`](crate::LoadedCharacter::palettes), or
    /// `None` to render with the SFF-embedded palette (the default).
    ///
    /// MUGEN characters may ship up to twelve alternate costume palettes (`.act`
    /// files referenced by `pal1`..`pal12`); this selects which one the renderer
    /// uploads instead of the sprite's embedded palette. `None` (the default)
    /// preserves today's behaviour exactly — the SFF-embedded palette is used and
    /// the GPU upload is byte-identical. Set it with
    /// [`set_active_palette`](Character::set_active_palette); resolve it to RGBA
    /// bytes with
    /// [`LoadedCharacter::override_palette`](crate::LoadedCharacter::override_palette).
    ///
    /// Stored as a bare selection index (not the RGBA) because the loaded
    /// palettes live on the static [`LoadedCharacter`](crate::LoadedCharacter),
    /// not on this runtime entity — keeping the runtime struct cheap to clone /
    /// snapshot. An index past the end of the loaded palettes resolves to `None`
    /// (the SFF-embedded palette), never a panic.
    pub active_palette: Option<usize>,

    /// The character's live `EnvShake` camera-shake effect (T015).
    ///
    /// Set by the `EnvShake` controller and counted down each non-hit-paused
    /// tick. The renderer (`fp-app`) reads [`Character::env_shake`] and offsets
    /// the camera while it is active. Inactive by default. See [`EnvShake`].
    pub env_shake: EnvShake,

    /// The character's live `EnvColor` full-screen color flash (T015).
    ///
    /// Set by the `EnvColor` controller and counted down each non-hit-paused
    /// tick. The renderer (`fp-app`) reads [`Character::env_color`] and fills the
    /// screen while it is active. Inactive by default. See [`EnvColor`].
    pub env_color: EnvColor,

    /// The character's live `RemapPal` palette-remap selection (T015).
    ///
    /// Set by the `RemapPal` controller; persists until changed or reset. The
    /// renderer reads [`Character::remap_pal`] to pick the matching loaded
    /// palette. Inactive (`dest = None`) by default. See [`RemapPal`].
    pub remap_pal: RemapPal,

    /// The character's debug clipboard text (MUGEN `DisplayToClipboard` /
    /// `AppendToClipboard` / `ClearClipboard`; T015).
    ///
    /// MUGEN renders this string in the debug overlay. We store the most recent
    /// formatted text so a host can surface it; `DisplayToClipboard` replaces it,
    /// `AppendToClipboard` appends, and `ClearClipboard` empties it. Empty by
    /// default.
    pub clipboard: String,

    /// The most recent `VictoryQuote` selection index (MUGEN `VictoryQuote`;
    /// T015), or `None` if none has been requested.
    ///
    /// `value = n` selects victory quote `n`; `value = -1` (MUGEN's "random")
    /// is stored verbatim for a host to interpret. The win-screen presenter
    /// reads this to pick which quote to show. `None` by default.
    pub victory_quote: Option<i32>,

    /// Per-tick draw-angle override set by the `AngleDraw` / `AngleSet` /
    /// `AngleAdd` / `AngleMul` controllers (T015).
    ///
    /// MUGEN rotates the sprite by this angle (degrees) for the tick in which
    /// `AngleDraw` fires. `AngleSet`/`AngleAdd`/`AngleMul` mutate the stored
    /// angle; `AngleDraw` arms the draw for this tick (and may override the angle
    /// with its own `value`). Like [`cur_width`](Self::cur_width) the *arm* flag
    /// is transient (cleared at the top of each tick); the angle itself persists.
    /// See [`DrawAngle`].
    pub draw_angle: DrawAngle,

    /// Per-tick position-freeze flag set by the `PosFreeze` controller (T015).
    ///
    /// MUGEN's `PosFreeze` holds the character's position for one tick (skips
    /// position integration) — used by hit-flash / charge effects. Cleared at the
    /// top of each tick and set by `PosFreeze` during it; the executor skips
    /// [`integrate_position`](crate::executor) while it is set.
    pub pos_frozen: bool,

    /// Per-tick transparency override set by the `Trans` controller (T015).
    ///
    /// MUGEN's `Trans` selects a blend mode (`none`/`add`/`add1`/`sub`/`addalpha`)
    /// for the tick in which it fires. Cleared at the top of each tick. The
    /// renderer reads [`Character::cur_trans`] to pick the sprite blend. See
    /// [`TransMode`].
    pub cur_trans: Option<TransMode>,
}

/// A sprite-draw rotation set by the `AngleDraw` family of controllers (T015).
///
/// `angle` is the rotation in degrees; `active` is the per-tick arm flag set by
/// `AngleDraw` (the only controller that actually requests a rotated draw). The
/// renderer applies `angle` while `active` is `true`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct DrawAngle {
    /// The current draw angle in degrees (mutated by `AngleSet`/`AngleAdd`/`AngleMul`).
    pub angle: f32,
    /// Whether a rotated draw is armed for this tick (set by `AngleDraw`).
    pub active: bool,
}

/// The resolved per-frame animation transform (scale + rotation) the renderer
/// applies to a character's sprite this tick (T009).
///
/// MUGEN's extended AIR lets each animation frame carry an optional
/// `xscale, yscale` scale pair and an `angle` (degrees), and lets
/// `Interpolate Scale` / `Interpolate Angle` lines request that the transform
/// blend smoothly from the *previous* frame into the current one across the
/// current element's duration. [`Character::anim_transform`] resolves all of that
/// — filling in defaults and applying the interpolation against the character's
/// own [`anim_elem`](Character::anim_elem) / [`anim_elem_time`](Character::anim_elem_time)
/// cursor — into this struct, which the renderer maps onto its sprite-draw
/// parameters (`scale_x`/`scale_y`, and `angle` converted to radians).
///
/// [`IDENTITY`](Self::IDENTITY) (also [`Default`]) is the no-op transform — unit
/// scale, zero rotation — so a character whose AIR carries no scale/angle columns
/// renders byte-identically to before this feature.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AnimTransform {
    /// Per-axis scale factor (`1.0` = original size). Defaults to `(1.0, 1.0)`.
    pub scale: Vec2<f32>,
    /// Rotation in **degrees** (MUGEN's AIR `angle` unit). `0.0` = no rotation;
    /// convert to radians before handing to the renderer.
    pub angle_deg: f32,
}

impl AnimTransform {
    /// The no-op transform: unit scale, zero rotation. A sprite drawn with it is
    /// positioned exactly as before per-frame scale/angle existed.
    pub const IDENTITY: Self = Self {
        scale: Vec2 { x: 1.0, y: 1.0 },
        angle_deg: 0.0,
    };

    /// Rotation in radians, for handing straight to the renderer's sprite
    /// rotation (which is radians, clockwise, about the sprite center).
    #[must_use]
    pub fn angle_rad(&self) -> f32 {
        self.angle_deg.to_radians()
    }
}

impl Default for AnimTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// The blend mode selected by the `Trans` state controller (T015).
///
/// Mirrors MUGEN's `Trans` `trans` parameter. `AddAlpha` carries the source/dest
/// alpha pair the controller specified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransMode {
    /// Opaque (no blending).
    None,
    /// Additive blending.
    Add,
    /// MUGEN's `add1` (additive at half source intensity).
    Add1,
    /// Subtractive blending.
    Sub,
    /// Alpha blending with explicit source/dest alpha (`0..=256` MUGEN scale).
    AddAlpha {
        /// Source alpha (`0..=256`).
        src: i32,
        /// Destination alpha (`0..=256`).
        dst: i32,
    },
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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

/// The "ticks since the last contact / hit / guard" counters for a single
/// projectile id, the persistent state behind the `ProjContact<id>` /
/// `ProjHit<id>` / `ProjGuarded<id>` / `ProjContactTime<id>` / `ProjHitTime<id>` /
/// `ProjGuardedTime<id>` trigger family (T026).
///
/// MUGEN tracks, per projectile id (`projid`), how many ticks have elapsed since
/// a projectile of that id last made contact (touched the opponent at all),
/// landed a clean hit, or was guarded. The `*Time` triggers read these directly;
/// the boolean `ProjContact<id>` / `ProjHit<id>` / `ProjGuarded<id>` triggers are
/// "did it happen this tick" (time `== 0`) and the comma-tail form
/// `ProjContact<id> = 1, op t` compares the stored time against `t`.
///
/// Each counter holds [`NEVER`](Self::NEVER) (`-1`) until the corresponding event
/// first occurs, then counts **up** from `0` (the tick of the event) each frame.
/// The owning [`Character`] advances the counters every tick via
/// [`Character::tick_proj_events`]; the round coordinator (`fp-engine`) records a
/// fresh event via [`Character::record_proj_event`] when one of this owner's
/// projectiles connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjContactTracker {
    /// Ticks since this id last made contact (hit **or** guarded), or
    /// [`NEVER`](Self::NEVER) if it never has.
    contact_time: i32,
    /// Ticks since this id last landed a clean (unblocked) hit, or
    /// [`NEVER`](Self::NEVER).
    hit_time: i32,
    /// Ticks since this id was last guarded/blocked, or [`NEVER`](Self::NEVER).
    guarded_time: i32,
}

impl Default for ProjContactTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjContactTracker {
    /// The "no such event yet" sentinel returned by the `Proj*Time<id>` triggers
    /// and stored in each counter before its event first occurs (`-1`, matching
    /// MUGEN's never-contacted time).
    pub const NEVER: i32 = -1;

    /// A fresh tracker for an id that has never contacted, hit, or been guarded:
    /// every counter is [`NEVER`](Self::NEVER).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            contact_time: Self::NEVER,
            hit_time: Self::NEVER,
            guarded_time: Self::NEVER,
        }
    }

    /// Ticks since this id last made contact (hit or guarded), `-1` if never
    /// (`ProjContactTime<id>`).
    #[must_use]
    pub const fn contact_time(self) -> i32 {
        self.contact_time
    }

    /// Ticks since this id last landed a clean hit, `-1` if never
    /// (`ProjHitTime<id>`).
    #[must_use]
    pub const fn hit_time(self) -> i32 {
        self.hit_time
    }

    /// Ticks since this id was last guarded, `-1` if never (`ProjGuardedTime<id>`).
    #[must_use]
    pub const fn guarded_time(self) -> i32 {
        self.guarded_time
    }

    /// Advances each *active* counter by one tick (a counter still at
    /// [`NEVER`](Self::NEVER) stays there). Saturating so a long-lived id can
    /// never overflow `i32`.
    fn tick(&mut self) {
        for t in [
            &mut self.contact_time,
            &mut self.hit_time,
            &mut self.guarded_time,
        ] {
            if *t >= 0 {
                *t = t.saturating_add(1);
            }
        }
    }

    /// Records a connection this tick: contact is always reset to `0`, and the
    /// clean-hit or guard counter is reset to `0` depending on `guarded`.
    fn record(&mut self, guarded: bool) {
        self.contact_time = 0;
        if guarded {
            self.guarded_time = 0;
        } else {
            self.hit_time = 0;
        }
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
            anim_elem_start_offsets: Vec::new(),
            anim_table_action: None,
            state_no: 0,
            prev_state_no: 0,
            state_time: 0,
            vars: [0; NUM_VARS],
            fvars: [0.0; NUM_FVARS],
            sysvars: [0; NUM_SYSVARS],
            sysfvars: [0.0; NUM_SYSFVARS],
            var_assignments: RefCell::new(Vec::new()),
            constants,
            commands: Box::new(NoCommands),
            fire_counts: std::collections::HashMap::new(),
            air_jump_count: 0,
            up_held_prev: false,
            active_hitdef: None,
            get_hit_vars: GetHitVars::default(),
            hitpause: 0,
            shaketime: 0,
            move_connect: MoveConnect::default(),
            has_target: false,
            proj_events: std::collections::HashMap::new(),
            invuln: InvulnMask::default(),
            asserted: AssertedFlags::default(),
            cur_width: WidthOverride::default(),
            cur_palfx: CurPalFx::default(),
            afterimage: AfterImageState::default(),
            hit_overrides: HitOverrides::default(),
            hit_overridden: false,
            attack_mul: 1.0,
            defence_mul: 1.0,
            cur_sprpriority: 0,
            // Seed the juggle pool from the character's `[Data] airjuggle`
            // allowance; refilled whenever the character touches the ground.
            juggle_points: constants.airjuggle,
            cur_juggle_cost: 0,
            hitdef_set_this_tick: false,
            // Deterministic fixed seed (never wall-clock); the cell is kept in
            // the generator's valid range via `Rng::new`. See `DEFAULT_RNG_SEED`.
            rng_seed: Cell::new(Rng::new(DEFAULT_RNG_SEED).seed()),
            // Safe default round clock (intro / time 0 / not over) until a
            // coordinator pushes the live view in via `set_round_view`.
            round_view: RoundView::default(),
            // Human by default (level 0); the coordinator raises it for a CPU
            // opponent via `set_ai_level`. A bare character is never the CPU.
            ai_level: 0,
            // No external palette override by default: render with the
            // SFF-embedded palette, byte-identical to today.
            active_palette: None,
            // T015 screen / palette / draw effects, all inactive by default.
            env_shake: EnvShake::INACTIVE,
            env_color: EnvColor::INACTIVE,
            remap_pal: RemapPal::default(),
            clipboard: String::new(),
            victory_quote: None,
            draw_angle: DrawAngle::default(),
            pos_frozen: false,
            cur_trans: None,
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
            // Seed the juggle pool from THIS character's `[Data] airjuggle`
            // (not the default), so each fighter starts with its own allowance.
            juggle_points: constants.airjuggle,
            constants,
            ..Self::default()
        }
    }

    /// Reseeds this character's `random` RNG stream to a deterministic seed.
    ///
    /// Use this to give a match its own reproducible randomness — for example
    /// seeding each player from a single shared match seed so both peers draw
    /// the same sequence (frame-perfect netplay / replay, #38). The seed is
    /// normalized into the Park–Miller generator's valid range `1..=2^31-2`
    /// (any `i32` is accepted, including `0` and negatives); the stream then
    /// advances purely deterministically from there. Never seed from the wall
    /// clock or OS RNG, which would break determinism — pass a fixed/derived
    /// value.
    pub fn seed_rng(&mut self, seed: i32) {
        // Round-trip through `Rng::new` so the stored cell is always a valid,
        // in-range Park–Miller state regardless of the caller's seed.
        self.rng_seed.set(Rng::new(seed).seed());
    }

    /// The engine-assigned AI difficulty level (`0` = human, `1..=8` = CPU) this
    /// character runs at — the value the MUGEN `AILevel` trigger reads (T052).
    ///
    /// A freshly-[`new`](Self::new)'d character (no coordinator) returns `0`, so a
    /// human is never mistaken for the CPU. See [`Character::ai_level`](Self::ai_level)
    /// (the field) and [`Character::set_ai_level`].
    #[must_use]
    pub fn ai_level(&self) -> u8 {
        self.ai_level
    }

    /// Sets the engine-assigned AI difficulty level (`0` = human, `1..=8` = CPU).
    ///
    /// Owned by the round coordinator (`fp-engine`'s `Match`/`TeamMatch`), which
    /// calls this once at match construction from the player's input driver. The
    /// character never assigns it itself — it is a read-only identity bit the CNS
    /// reads (the `AILevel` trigger). Read it back with
    /// [`Character::ai_level`](Self::ai_level).
    pub fn set_ai_level(&mut self, level: u8) {
        self.ai_level = level;
    }

    /// Returns the selected external `.act` palette override as a 0-based index
    /// into the owning [`LoadedCharacter::palettes`](crate::LoadedCharacter::palettes),
    /// or `None` to render with the SFF-embedded palette.
    ///
    /// See [`active_palette`](Character::active_palette) for the full contract.
    /// Resolve the index to RGBA bytes with
    /// [`LoadedCharacter::override_palette`](crate::LoadedCharacter::override_palette).
    #[must_use]
    pub fn active_palette(&self) -> Option<usize> {
        self.active_palette
    }

    /// Selects an external `.act` palette override by 0-based index into the
    /// owning [`LoadedCharacter::palettes`](crate::LoadedCharacter::palettes).
    ///
    /// Pass `None` (or call [`clear_active_palette`](Character::clear_active_palette))
    /// to revert to the SFF-embedded palette, the default. The index is **not**
    /// validated here — an index past the loaded palettes resolves to `None` (the
    /// SFF-embedded palette) at render time via
    /// [`LoadedCharacter::override_palette`](crate::LoadedCharacter::override_palette),
    /// never a panic — so callers may set a selection before the palette list is
    /// known.
    pub fn set_active_palette(&mut self, selection: Option<usize>) {
        self.active_palette = selection;
    }

    /// Clears any external palette override, reverting to the SFF-embedded
    /// palette (equivalent to `set_active_palette(None)`).
    pub fn clear_active_palette(&mut self) {
        self.active_palette = None;
    }

    /// Captures this character's mutable runtime state as a serializable
    /// [`CharacterSnapshot`] (replay / rollback, #38).
    ///
    /// The snapshot mirrors only the runtime fields (position, velocity, life,
    /// the state-machine cursor, the variable banks, the RNG position, combat
    /// bookkeeping, …); it does **not** carry the loaded static data
    /// ([`LoadedCharacter`], [`CharacterConstants`], the command source), which is
    /// reloaded from the `.def`. See [`crate::snapshot`]. Pair it with
    /// [`restore_from_snapshot`](Self::restore_from_snapshot) on a character loaded
    /// from the same `.def`.
    #[must_use]
    pub fn snapshot(&self) -> crate::snapshot::CharacterSnapshot {
        crate::snapshot::CharacterSnapshot::capture(self)
    }

    /// Restores a [`CharacterSnapshot`] onto this already-loaded character.
    ///
    /// Overwrites the mutable runtime fields and leaves the static handles
    /// (`commands`, `constants`, the loaded assets) untouched — so it must be
    /// applied to a character loaded from the **same** `.def` the snapshot was
    /// taken from. Never panics (the variable banks are length-clamped). See
    /// [`crate::snapshot`].
    pub fn restore_from_snapshot(&mut self, snap: &crate::snapshot::CharacterSnapshot) {
        snap.apply_to(self);
    }

    /// Draws one raw Park–Miller value and advances this character's RNG state.
    ///
    /// Returns the next raw draw in `1..=2^31-2` (the generator's range), exactly
    /// what the [`EvalContext::random`] seam contract asks for: the evaluator
    /// maps it to MUGEN's `random` → `[0,999]` or `random(lo,hi)` → `[lo,hi]`.
    /// Reconstructs an [`Rng`] from the stored raw seed, steps it once, and
    /// writes the new seed back through the [`Cell`] — so the state lives in
    /// plain (serializable) struct state. Takes `&self` because the seam is
    /// immutable; the interior mutability is confined to the cell.
    fn draw_random(&self) -> i32 {
        let mut rng = Rng::new(self.rng_seed.get());
        let raw = rng.next_u31();
        self.rng_seed.set(rng.seed());
        raw
    }

    /// Replaces the command source (called by the executor to inject
    /// `fp-input`'s recognizer, or by tests to inject a synthetic set).
    pub fn set_command_source(&mut self, source: Box<dyn CommandSource>) {
        self.commands = source;
    }

    /// Installs the engine-global round / match clock this character's
    /// `RoundState`, `GameTime`, `MatchOver`, `RoundNo`, and `RoundsExisted`
    /// triggers read (audits #21 and T016).
    ///
    /// Called by the round coordinator (`fp-engine`'s `Match`) **before** ticking
    /// the character each frame, so its CNS triggers see the live round phase,
    /// game time, match-over flag, round number, and completed-round count. A
    /// character that is never given a view keeps the [`RoundView::default`] (the
    /// safe defaults). See [`RoundView`].
    pub fn set_round_view(&mut self, view: RoundView) {
        self.round_view = view;
    }

    /// Advances every tracked projectile-id contact/hit/guard counter by one tick
    /// (T026), aging the values the `ProjContact<id>` / `ProjHit<id>` /
    /// `ProjGuarded<id>` / `Proj*Time<id>` triggers report.
    ///
    /// The round coordinator (`fp-engine`'s `Match`) calls this once per frame for
    /// each player after recording any new connections this tick (see
    /// [`record_proj_event`](Self::record_proj_event)), so a counter reads `0` on
    /// the tick its projectile connected and counts up from there. A counter still
    /// at [`ProjContactTracker::NEVER`] (its event has not happened) stays there.
    /// Never panics.
    pub fn tick_proj_events(&mut self) {
        for tracker in self.proj_events.values_mut() {
            tracker.tick();
        }
    }

    /// Records that one of this owner's projectiles with the given `projid`
    /// connected this tick — `guarded` selects the clean-hit vs guard counter
    /// (contact is always recorded) — feeding the `ProjContact<id>` / `ProjHit<id>`
    /// / `ProjGuarded<id>` / `Proj*Time<id>` triggers (T026).
    ///
    /// The round coordinator (`fp-engine`) is the writer: when a projectile's
    /// `resolve_attack` connects it calls this on the **owning** character (the
    /// projectile slot-map lives with the `Player`, not on this self-only
    /// `Character`). Resets the id's contact time to `0` (and its hit *or* guard
    /// time to `0`); a not-yet-tracked id is inserted. Bounded by
    /// [`MAX_PROJ_IDS`](Self::MAX_PROJ_IDS) so a pathological stream of distinct
    /// ids can never grow the map without limit (the cap is generous; once full,
    /// only already-tracked ids keep updating). Never panics.
    pub fn record_proj_event(&mut self, projid: i32, guarded: bool) {
        // An already-tracked id is always updated; a NEW id is only inserted while
        // the map is under its cap, so a pathological stream of distinct ids can
        // never grow `proj_events` without bound.
        let known = self.proj_events.contains_key(&projid);
        if known || self.proj_events.len() < Self::MAX_PROJ_IDS {
            self.proj_events.entry(projid).or_default().record(guarded);
        }
    }

    /// A hard cap on the number of distinct projectile ids tracked for the
    /// `Proj*<id>` triggers, so a runaway spawner cycling through ever-new ids can
    /// never grow [`proj_events`](Self::proj_events) without bound. Generous (a
    /// character realistically uses a handful of `projid`s).
    pub const MAX_PROJ_IDS: usize = 64;

    /// The tracker for projectile id `projid`, or the empty "never" tracker when
    /// the id has had no contact/hit/guard yet (T026). The read seam the
    /// `Proj*<id>` triggers resolve through.
    #[must_use]
    fn proj_tracker(&self, projid: i32) -> ProjContactTracker {
        self.proj_events.get(&projid).copied().unwrap_or_default()
    }

    /// Returns the number of hit-pause (impact-freeze) ticks this character still
    /// has remaining — MUGEN's `hitpause_time`. `0` means the character is not
    /// frozen and ticks normally.
    ///
    /// This reads the [`hitpause`](Character::hitpause) field; the two name the
    /// same value. While it is positive the executor freezes this character (see
    /// the field docs and [`Character::tick`]).
    #[must_use]
    pub const fn hitpause_time(&self) -> i32 {
        self.hitpause
    }

    /// Sets the remaining hit-pause (impact-freeze) ticks — MUGEN's
    /// `hitpause_time` — clamping any negative input to `0` ("not paused").
    ///
    /// Hit resolution ([`resolve_attack`]) is the normal writer (it sets the
    /// attacker from `pausetime.p1` and the defender from `pausetime.p2`); this
    /// accessor is the explicit seam for callers and tests that want to freeze a
    /// character directly. Writes the [`hitpause`](Character::hitpause) field.
    pub fn set_hitpause_time(&mut self, ticks: i32) {
        self.hitpause = ticks.max(0);
    }

    /// Returns the character's current `PalFX` color tint, resolved for this
    /// tick (faithfulness audit #33; full modulation set T008).
    ///
    /// When a `PalFX` effect is active ([`CurPalFx::is_active`]) this is its
    /// per-frame [`effective`](CurPalFx::effective) modulation —
    /// `add` (with the current `sinadd` oscillation already folded in), `mul`,
    /// `color`, and `invertall` — with the live countdown; otherwise it is the
    /// **identity** tint ([`CurPalFx::IDENTITY`]), a guaranteed no-op so the
    /// sprite renders byte-identically. The renderer (`fp-app`) converts the
    /// returned effect into the renderer's `fp_render::PalFx` and passes it
    /// into the player's `SpriteDrawParams`. Kept renderer-agnostic (returns a
    /// `fp-character` type) so this low-level crate does not depend on
    /// `fp-render`. Always returns a valid effect (never panics).
    #[must_use]
    pub fn palfx(&self) -> CurPalFx {
        if self.cur_palfx.is_active() {
            self.cur_palfx.effective()
        } else {
            CurPalFx::IDENTITY
        }
    }

    /// Returns the character's live `AfterImage` trail state (faithfulness audit
    /// #33; frame-history ring, T007).
    ///
    /// The renderer (`fp-app`) reads this and, while
    /// [`AfterImageState::is_active`], draws the trail's captured
    /// [`frames`](AfterImageState::frames) (selected via
    /// [`AfterImageState::ghost_frames`]) behind the sprite, composited with the
    /// trail's [`trans`](AfterImageState::trans) and progressively modulated by
    /// its [`palfx`](AfterImageState::palfx) /
    /// [`palbright`](AfterImageState::palbright) /
    /// [`palcontrast`](AfterImageState::palcontrast). Returns a borrow (the ring
    /// owns a `Vec`, so the state is no longer `Copy`); inactive
    /// ([`AfterImageState::is_active`] is `false`) when no trail is running.
    #[must_use]
    pub const fn afterimage(&self) -> &AfterImageState {
        &self.afterimage
    }

    /// Computes MUGEN's `AnimElemTime(n)` for the **one-based** element index
    /// `n` of the *current* animation action: the number of ticks elapsed since
    /// element `n` began, within the current loop iteration.
    ///
    /// The value is **positive** for the current and already-played elements
    /// (growing as the action plays), exactly equal to
    /// [`anim_elem_time`](Character::anim_elem_time) for the current element (so
    /// there is no regression versus the legacy single-element behavior), and
    /// **negative** for elements not yet reached this iteration — matching
    /// MUGEN, where the `AnimElem = N, op M` lowering relies on
    /// `AnimElemTime(N) >= 0` as the "element reached" guard.
    ///
    /// # Looping
    ///
    /// The offset table and [`anim_elem`](Character::anim_elem) /
    /// [`anim_elem_time`](Character::anim_elem_time) are reset by the executor
    /// when the action loops back to its `loopstart`, so the returned time
    /// reflects the **current loop iteration**, not cumulative play-through —
    /// again matching MUGEN.
    ///
    /// # Edge cases (never panics)
    ///
    /// - If the offset table is empty (no AIR action is active yet, or the
    ///   active action has no frames) this falls back to the legacy scalar:
    ///   [`anim_elem_time`](Character::anim_elem_time) when `n` names the current
    ///   element, else [`ANIM_ELEM_NOT_REACHED`] (`-1`).
    /// - `n` out of range — `n < 1` or `n` greater than the number of elements —
    ///   is **clamped into range** before lookup (MUGEN's `AnimElemTime` clamps a
    ///   request to the nearest valid element rather than erroring), so the
    ///   result is always a sane, finite time and the function never panics.
    #[must_use]
    fn anim_elem_time_for(&self, n: i32) -> i32 {
        let offsets = &self.anim_elem_start_offsets;
        if offsets.is_empty() {
            // No per-element table available: preserve the legacy single-element
            // contract (current element → its elapsed time, else not-reached).
            return if n == self.anim_elem + 1 {
                self.anim_elem_time
            } else {
                ANIM_ELEM_NOT_REACHED
            };
        }

        // Clamp the one-based request into `1..=len` (MUGEN clamps rather than
        // erroring), then index zero-based. `len >= 1` here. `saturating_sub`
        // guards `n == i32::MIN` (where `n - 1` would overflow); `clamp_usize`
        // then folds the zero-based index safely into `0..len`.
        let len = offsets.len();
        let idx = clamp_usize(n.saturating_sub(1), len);

        // Elapsed ticks since the start of this loop iteration of the action:
        // the current element's start offset plus the time spent in it.
        let cur = clamp_usize(self.anim_elem, len);
        let elapsed = offsets[cur].saturating_add(self.anim_elem_time);
        // Time since element `idx` began (positive once reached, negative before).
        elapsed.saturating_sub(offsets[idx])
    }

    /// Returns the most recent pending in-expression assignment (`:=`) to slot
    /// `index` of `bank`, if any, so a read sees a same-expression write before it
    /// is flushed to the real banks (T036). The overlay log is scanned in reverse
    /// (last write wins); when empty (the common case) this is a cheap early-out.
    fn pending_assignment(&self, bank: AssignBank, index: i32) -> Option<Value> {
        let log = self.var_assignments.borrow();
        if log.is_empty() {
            return None;
        }
        log.iter()
            .rev()
            .find(|(b, i, _)| *b == bank && *i == index)
            .map(|(_, _, v)| *v)
    }

    /// Reads integer variable `index`, or `0` if the index is out of range.
    ///
    /// A pending `:=` assignment to this slot (T036) takes precedence over the
    /// stored bank value, so an in-expression write is visible to a later read in
    /// the same expression.
    #[must_use]
    fn read_var(&self, index: i32) -> i32 {
        if let Some(v) = self.pending_assignment(AssignBank::Var, index) {
            return v.to_int();
        }
        usize::try_from(index)
            .ok()
            .and_then(|i| self.vars.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// Reads float variable `index`, or `0.0` if the index is out of range.
    ///
    /// A pending `:=` assignment to this slot (T036) takes precedence.
    #[must_use]
    fn read_fvar(&self, index: i32) -> f32 {
        if let Some(v) = self.pending_assignment(AssignBank::FVar, index) {
            return v.to_float();
        }
        usize::try_from(index)
            .ok()
            .and_then(|i| self.fvars.get(i))
            .copied()
            .unwrap_or(0.0)
    }

    /// Reads system integer variable `index`, or `0` if out of range.
    ///
    /// A pending `:=` assignment to this slot (T036) takes precedence.
    #[must_use]
    fn read_sysvar(&self, index: i32) -> i32 {
        if let Some(v) = self.pending_assignment(AssignBank::SysVar, index) {
            return v.to_int();
        }
        usize::try_from(index)
            .ok()
            .and_then(|i| self.sysvars.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// Reads system float variable `index`, or `0.0` if out of range.
    ///
    /// A pending `:=` assignment to this slot (T036) takes precedence.
    #[must_use]
    fn read_sysfvar(&self, index: i32) -> f32 {
        if let Some(v) = self.pending_assignment(AssignBank::SysFVar, index) {
            return v.to_float();
        }
        usize::try_from(index)
            .ok()
            .and_then(|i| self.sysfvars.get(i))
            .copied()
            .unwrap_or(0.0)
    }

    /// Drains the pending in-expression assignment (`:=`) overlay (T036) into the
    /// real variable banks, applying each buffered write in order (so the last
    /// write to a slot wins) and clearing the log.
    ///
    /// The executor calls this at the top of every tick — once the previous tick's
    /// `&mut self` boundary is reached — so the banks always reflect every `:=`
    /// that fired, and a snapshot taken at a tick boundary sees the flushed values
    /// (the overlay is empty then). An out-of-range index is silently skipped,
    /// matching the engine-wide "bad input → safe no-op" rule. Returns the number
    /// of assignments applied (`0` is the common, fast case).
    pub fn flush_var_assignments(&mut self) -> usize {
        // Take ownership of the buffered writes, leaving the overlay empty.
        let pending = std::mem::take(self.var_assignments.get_mut());
        let applied = pending.len();
        for (bank, index, value) in pending {
            let Ok(i) = usize::try_from(index) else {
                continue;
            };
            match bank {
                AssignBank::Var => {
                    if let Some(slot) = self.vars.get_mut(i) {
                        *slot = value.to_int();
                    }
                }
                AssignBank::FVar => {
                    if let Some(slot) = self.fvars.get_mut(i) {
                        *slot = value.to_float();
                    }
                }
                AssignBank::SysVar => {
                    if let Some(slot) = self.sysvars.get_mut(i) {
                        *slot = value.to_int();
                    }
                }
                AssignBank::SysFVar => {
                    if let Some(slot) = self.sysfvars.get_mut(i) {
                        *slot = value.to_float();
                    }
                }
            }
        }
        applied
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

    /// Tests an attack attribute against a parsed `HitDefAttr = <standtype>,
    /// <attr-list>` query (Task A).
    ///
    /// Returns `true` iff `attr`'s state-class matches `standtype` (`"S"`/`"C"`/
    /// `"A"`, case-insensitive) **and** its 2-char attack code (power + kind, e.g.
    /// `"NA"`) equals one of `attr_codes` (case-insensitive). `attr_codes` is
    /// already upper-cased by the parser, but the comparison is made
    /// case-insensitively for robustness. An empty / malformed standtype or an
    /// empty code list yields `false` (never a panic), matching the
    /// "bad expression → never fires" rule.
    fn attack_attr_matches(
        attr: &fp_combat::AttackAttr,
        standtype: &str,
        attr_codes: &[String],
    ) -> bool {
        // Stand-type letter of the attacker's class.
        let class_letter = match attr.class {
            fp_combat::StateClass::Standing => "S",
            fp_combat::StateClass::Crouching => "C",
            fp_combat::StateClass::Air => "A",
        };
        if !class_letter.eq_ignore_ascii_case(standtype) {
            return false;
        }

        // The 2-char attack code: power class {N|S|H} + kind {A|T|P}.
        let power = match attr.power {
            fp_combat::AttackPower::Normal => 'N',
            fp_combat::AttackPower::Special => 'S',
            fp_combat::AttackPower::Hyper => 'H',
        };
        let kind = match attr.kind {
            fp_combat::AttackKind::Attack => 'A',
            fp_combat::AttackKind::Throw => 'T',
            fp_combat::AttackKind::Projectile => 'P',
        };
        let code = [power, kind];

        attr_codes.iter().any(|wanted| {
            let mut wc = wanted.chars();
            match (wc.next(), wc.next(), wc.next()) {
                // Exactly two chars, compared case-insensitively.
                (Some(a), Some(b), None) => {
                    a.eq_ignore_ascii_case(&code[0]) && b.eq_ignore_ascii_case(&code[1])
                }
                _ => false,
            }
        })
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
    /// - `velocity.walk.fwd.x` / `velocity.walk.back.x` (and the **bare**
    ///   forms `velocity.walk.fwd` / `velocity.walk.back`, which alias `.x` —
    ///   standard MUGEN content writes `VelSet x = const(velocity.walk.fwd)`;
    ///   the same bare→`.x` aliasing applies to `velocity.run.*`,
    ///   `velocity.jump.*`, and `velocity.airjump.*`)
    /// - `velocity.run.fwd.x` / `velocity.run.fwd.y`
    /// - `velocity.run.back.x` / `velocity.run.back.y`
    /// - `velocity.jump.neu.x` / `velocity.jump.y`
    /// - `velocity.jump.fwd.x` / `velocity.jump.back.x`
    /// - `velocity.jump.neu.y` / `velocity.jump.fwd.y` / `velocity.jump.back.y`
    ///   (all alias the single upward jump speed `velocity.jump.y`)
    /// - `velocity.runjump.fwd.x` / `velocity.runjump.fwd.y`
    /// - `velocity.runjump.back.x` / `velocity.runjump.back.y`
    /// - `velocity.airjump.neu.x` / `velocity.airjump.fwd.x` /
    ///   `velocity.airjump.back.x` / `velocity.airjump.y`
    /// - `size.ground.front` / `size.ground.back` / `size.height`
    /// - `movement.yaccel` / `movement.stand.friction` / `movement.crouch.friction`
    /// - `movement.airjump.num` (integer) / `movement.airjump.height` (float)
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
        if m.eq_ignore_ascii_case("movement.airjump.num") {
            return Value::Int(c.movement.airjump_num);
        }

        // Float-typed members (`[Velocity]` and `[Movement]`).
        //
        // Bare velocity members (no `.x`/`.y` axis suffix) alias their `.x`
        // component: standard MUGEN content (incl. the shipped trainingdummy
        // [Statedef 20] walk) writes `VelSet x = const(velocity.walk.fwd)`,
        // authoring walk/run/jump x-velocity x-first (walk is x-only). Without
        // this, the bare form fell through to `Value::DEFAULT` (0) and every
        // character "walked in place". Each bare arm returns the same value as
        // its corresponding `.x` arm below.
        if m.eq_ignore_ascii_case("velocity.walk.fwd.x")
            || m.eq_ignore_ascii_case("velocity.walk.fwd")
        {
            return Value::Float(c.velocity.walk_fwd.x);
        }
        if m.eq_ignore_ascii_case("velocity.walk.back.x")
            || m.eq_ignore_ascii_case("velocity.walk.back")
        {
            return Value::Float(c.velocity.walk_back.x);
        }
        if m.eq_ignore_ascii_case("velocity.run.fwd.x")
            || m.eq_ignore_ascii_case("velocity.run.fwd")
        {
            return Value::Float(c.velocity.run_fwd.x);
        }
        if m.eq_ignore_ascii_case("velocity.run.fwd.y") {
            return Value::Float(c.velocity.run_fwd.y);
        }
        if m.eq_ignore_ascii_case("velocity.run.back.x")
            || m.eq_ignore_ascii_case("velocity.run.back")
        {
            return Value::Float(c.velocity.run_back.x);
        }
        if m.eq_ignore_ascii_case("velocity.run.back.y") {
            return Value::Float(c.velocity.run_back.y);
        }
        if m.eq_ignore_ascii_case("velocity.jump.neu.x")
            || m.eq_ignore_ascii_case("velocity.jump.neu")
        {
            return Value::Float(c.velocity.jump_neu.x);
        }
        if m.eq_ignore_ascii_case("velocity.jump.fwd.x")
            || m.eq_ignore_ascii_case("velocity.jump.fwd")
        {
            return Value::Float(c.velocity.jump_fwd.x);
        }
        if m.eq_ignore_ascii_case("velocity.jump.back.x")
            || m.eq_ignore_ascii_case("velocity.jump.back")
        {
            return Value::Float(c.velocity.jump_back.x);
        }
        // The neutral/forward/back jump y-components all alias the single
        // upward jump speed (`velocity.jump.y` == `jump_up`): MUGEN authors a
        // jump y once and reuses it across directions, and the shipped
        // trainingdummy jumpstart writes `y = const(velocity.jump.neu.y)`.
        // Without these arms the neutral-jump UP velocity fell through to
        // `Value::DEFAULT` (0) and the character "jumped in place".
        if m.eq_ignore_ascii_case("velocity.jump.neu.y")
            || m.eq_ignore_ascii_case("velocity.jump.fwd.y")
            || m.eq_ignore_ascii_case("velocity.jump.back.y")
        {
            return Value::Float(c.velocity.jump_up);
        }
        if m.eq_ignore_ascii_case("velocity.jump.y") {
            return Value::Float(c.velocity.jump_up);
        }
        if m.eq_ignore_ascii_case("velocity.runjump.fwd.x") {
            return Value::Float(c.velocity.runjump_fwd.x);
        }
        if m.eq_ignore_ascii_case("velocity.runjump.fwd.y") {
            return Value::Float(c.velocity.runjump_fwd.y);
        }
        if m.eq_ignore_ascii_case("velocity.runjump.back.x") {
            return Value::Float(c.velocity.runjump_back.x);
        }
        if m.eq_ignore_ascii_case("velocity.runjump.back.y") {
            return Value::Float(c.velocity.runjump_back.y);
        }
        if m.eq_ignore_ascii_case("velocity.airjump.neu.x")
            || m.eq_ignore_ascii_case("velocity.airjump.neu")
        {
            return Value::Float(c.velocity.airjump_neu.x);
        }
        if m.eq_ignore_ascii_case("velocity.airjump.fwd.x")
            || m.eq_ignore_ascii_case("velocity.airjump.fwd")
        {
            return Value::Float(c.velocity.airjump_fwd.x);
        }
        if m.eq_ignore_ascii_case("velocity.airjump.back.x")
            || m.eq_ignore_ascii_case("velocity.airjump.back")
        {
            return Value::Float(c.velocity.airjump_back.x);
        }
        if m.eq_ignore_ascii_case("velocity.airjump.y") {
            return Value::Float(c.velocity.airjump_y);
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
        if m.eq_ignore_ascii_case("movement.stand.friction.threshold") {
            return Value::Float(c.movement.stand_friction_threshold);
        }
        if m.eq_ignore_ascii_case("movement.crouch.friction.threshold") {
            return Value::Float(c.movement.crouch_friction_threshold);
        }
        if m.eq_ignore_ascii_case("movement.down.friction.threshold") {
            return Value::Float(c.movement.down_friction_threshold);
        }
        if m.eq_ignore_ascii_case("movement.airjump.height") {
            return Value::Float(c.movement.airjump_height);
        }

        // Unknown member: safe default 0. `debug!` (not `warn!`) because unmodeled
        // const members are common and benign in community content.
        tracing::debug!("const({member}): unmodeled member, defaulting to 0");
        Value::DEFAULT
    }

    /// Scales a value authored in a high-resolution coordinate space down to this
    /// character's [`localcoord`](CharacterConstants::localcoord) space, for the
    /// MUGEN `Const720p` / `Const1280p` triggers.
    ///
    /// # MUGEN formula and reasoning
    ///
    /// MUGEN's coordinate model scales assets authored in one space into another
    /// **by the ratio of the target width to the source width** (the source's
    /// `localcoord` *height* is not used for this scale — see the engine
    /// architecture KB and Elecbyte's `coordspace.html`). `Const720p(v)` says "I
    /// authored `v` in the 720p space", and the engine converts it to the
    /// player's space:
    ///
    /// ```text
    /// Const720p(v)  = v * (localcoord.width / 1280)   // 720p  space is 1280 wide
    /// Const1280p(v) = v * (localcoord.width / 2560)   // 1280p space is 2560 wide
    /// ```
    ///
    /// The reference is the **width** of each named space: "720p" is 1280×720
    /// (width 1280) and "1280p" is the next tier, 2560×1440 (width 2560). Using
    /// the width ratio — not the height ratio — is what makes KFM's `(320, 240)`
    /// yield exactly `320 / 1280 = 0.25` for `Const720p` (so `Const720p(-8) =
    /// -2.0`), matching MUGEN; the height ratio `240 / 720 ≈ 0.333` would give the
    /// wrong `-2.667`. For `Const1280p` the KFM factor is `320 / 2560 = 0.125`
    /// (so `Const1280p(-8) = -1.0`).
    ///
    /// `reference_width` is the source space's width (`1280` for `Const720p`,
    /// `2560` for `Const1280p`). The arithmetic is done in `f32` and the sign of
    /// `value` is preserved (the scale is non-negative for any sane positive
    /// `localcoord`). A non-positive `reference_width` (never produced internally)
    /// yields `0.0` rather than dividing by zero, so the result is always finite
    /// and this never panics.
    #[must_use]
    fn const_coord_scale(&self, value: f32, reference_width: f32) -> f32 {
        if reference_width <= 0.0 {
            return 0.0;
        }
        let local_width = self.constants.localcoord.0 as f32;
        value * (local_width / reference_width)
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

/// Which member of the projectile-trigger family a `Proj*<id>` trigger names, and
/// whether it is the "time since" (`*Time`) form (T026).
///
/// `ProjContact2000` → `(Contact, time = false)`; `ProjHitTime2000` →
/// `(Hit, time = true)`. The owner's [`EvalContext::trigger`] resolves the value
/// from the [`ProjContactTracker`] keyed by the parsed id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjTriggerKind {
    Contact,
    Hit,
    Guarded,
}

/// Parses a bare projectile-info trigger name into its family, whether it is the
/// `*Time` form, and the trailing projectile id — `None` if `name` is not a
/// projectile-info trigger (T026).
///
/// MUGEN writes these as a fixed base name with the `projid` appended, e.g.
/// `ProjContact2000`, `ProjHitTime340`, `ProjGuarded5`. The match is
/// case-insensitive on the base and requires a parseable integer id suffix; a
/// negative id (`ProjContact-1`) and a missing/garbage suffix are rejected
/// (returns `None`), so a malformed name falls through to the ordinary trigger
/// path and the engine-wide "unknown trigger → 0" default. Longest bases are
/// tested first so `projcontacttime` is preferred over `projcontact`.
fn parse_proj_trigger(name: &str) -> Option<(ProjTriggerKind, bool, i32)> {
    // Cheap allocation-free guard: this runs as the FIRST check for every
    // bare-ident trigger the executor evaluates each tick (`Time`, `StateNo`,
    // `Anim`, …), so it must NOT heap-allocate on the common non-proj path. Every
    // projectile-info trigger begins with a case-insensitive `proj`; anything that
    // does not is rejected before any prefix matching (and well before the old
    // `to_ascii_lowercase()` String allocation that lived here).
    if name.len() < "projhit0".len() || !name.as_bytes()[..4].eq_ignore_ascii_case(b"proj") {
        return None;
    }
    // (base, kind, is_time) — `*time` variants listed first so they win the prefix
    // test over their non-time counterparts. Matched case-insensitively against the
    // name slice without lowercasing the whole string.
    const BASES: [(&str, ProjTriggerKind, bool); 6] = [
        ("projcontacttime", ProjTriggerKind::Contact, true),
        ("projguardedtime", ProjTriggerKind::Guarded, true),
        ("projhittime", ProjTriggerKind::Hit, true),
        ("projcontact", ProjTriggerKind::Contact, false),
        ("projguarded", ProjTriggerKind::Guarded, false),
        ("projhit", ProjTriggerKind::Hit, false),
    ];
    for (base, kind, is_time) in BASES {
        if let Some(prefix) = name.get(..base.len()) {
            if prefix.eq_ignore_ascii_case(base) {
                // The id suffix must be a non-negative integer; anything else
                // (empty, signed, non-numeric) is not a projectile-info trigger.
                let suffix = &name[base.len()..];
                return suffix.parse::<i32>().ok().map(|id| (kind, is_time, id));
            }
        }
    }
    None
}

impl Character {
    /// Resolves a bare projectile-info trigger (`ProjContact<id>` / `ProjHit<id>`
    /// / `ProjGuarded<id>` / `Proj*Time<id>`) against this owner's
    /// [`proj_events`](Self::proj_events) tracker (T026), or `None` when `name` is
    /// not such a trigger.
    ///
    /// The `*Time` form returns the ticks since the event for that id
    /// ([`ProjContactTracker::NEVER`] = `-1` if it never happened). The boolean
    /// form (`ProjContact<id>`) returns `1` iff the event happened **this tick**
    /// (the stored time is `0`), else `0` — the comma-tail
    /// `ProjContact<id> = v, op t` form (handled in the evaluator) reads the
    /// `*Time` value to compare against a window. Never panics.
    fn proj_trigger(&self, name: &str) -> Option<Value> {
        let (kind, is_time, id) = parse_proj_trigger(name)?;
        let tracker = self.proj_tracker(id);
        let time = match kind {
            ProjTriggerKind::Contact => tracker.contact_time(),
            ProjTriggerKind::Hit => tracker.hit_time(),
            ProjTriggerKind::Guarded => tracker.guarded_time(),
        };
        Some(if is_time {
            Value::Int(time)
        } else {
            // Bare boolean form: true only on the exact tick of the event.
            Value::from(time == 0)
        })
    }
}

impl EvalContext for Character {
    fn trigger(&self, name: &str, args: &[Value]) -> Value {
        // Helper: first arg coerced to i32 (used by indexed/axis triggers).
        let first_int = || args.first().map(|v| v.to_int());

        // Projectile-info triggers: `ProjContact<id>`, `ProjHit<id>`,
        // `ProjGuarded<id>`, and their `*Time<id>` variants (T026). These parse as
        // a bare `Ident` (the id is part of the name), so they arrive here with no
        // args; resolve them against this owner's per-id contact tracker. The
        // comma-tail `ProjContact<id> = v, op t` form is folded by the parser into
        // an `Expr::ProjTail` and reads the `*Time` value through this same seam.
        if args.is_empty() {
            if let Some(v) = self.proj_trigger(name) {
                return v;
            }
        }

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
            // `AnimElemTime(n)` is the time (in ticks) since element `n`
            // (one-based) of the CURRENT animation action began, within the
            // current loop iteration. Task A.P6 resolves this for ANY element of
            // the action via the per-element start-offset table the executor
            // builds in `advance_animation`: positive for the current/past
            // elements (equal to `anim_elem_time` for the current one — no
            // regression), and NEGATIVE for not-yet-reached future elements.
            //
            // The negative-for-future contract is load-bearing: the VM lowers
            // `AnimElem = N, op M` to a "reached" guard of `AnimElemTime(N) >= 0`,
            // so a not-yet-reached element must read negative or the tail would
            // spuriously fire. Out-of-range `n` is clamped to a valid element
            // (never panics); a missing argument is the safe default. See
            // [`Character::anim_elem_time_for`].
            return match first_int() {
                Some(n) => Value::Int(self.anim_elem_time_for(n)),
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

        // `HitVel X` / `HitVel Y` (T061) — the velocity the most recent hit taken
        // imparted, read from this character's [`GetHitVars`] (the `xvel`/`yvel`
        // members the engine populated on the connecting hit). Per-axis, routed
        // through the same `X = 0` / `Y = 1` axis coding as `Vel`/`Pos`; a missing
        // or malformed axis falls back to the X component, matching the
        // evaluator's lowering. With no hit taken the GetHitVars default to `0`,
        // so `HitVel` reports `0` — never a panic.
        if name.eq_ignore_ascii_case("HitVel") {
            let hit_vel = Vec2::new(self.get_hit_vars.xvel, self.get_hit_vars.yvel);
            return Value::Float(Self::axis_component(hit_vel, args));
        }

        // `HitOverridden` (T061) — 1 iff this character's current get-hit reaction
        // was redirected by an active [`HitOverride`](HitOverrides) slot, else 0.
        // The flag is set by hit resolution when a slot matches and cleared by the
        // next normal (non-overridden) hit. See [`Character::hit_overridden`].
        if name.eq_ignore_ascii_case("HitOverridden") {
            return Value::from(self.hit_overridden);
        }

        // Coordinate-scaling triggers (MUGEN 1.1). `Const720p(v)` / `Const1280p(v)`
        // take a value authored in a high-resolution space (720p = 1280 wide,
        // 1280p = 2560 wide) and scale it to this character's `localcoord` space
        // by the WIDTH ratio. They yield a float, sign-preserving. A missing/
        // garbage argument resolves to the safe default 0 rather than panicking.
        // common1.cns gates landing/air-anim/sprpriority and p2dist thresholds on
        // these (e.g. `Vel y > Const720p(-8)`); with them returning 0 (the old
        // behavior) such gates degenerated to `> 0`. See [`const_coord_scale`].
        if name.eq_ignore_ascii_case("Const720p") {
            return match args.first() {
                Some(v) => Value::Float(self.const_coord_scale(v.to_float(), 1280.0)),
                None => Value::DEFAULT,
            };
        }
        if name.eq_ignore_ascii_case("Const1280p") {
            return match args.first() {
                Some(v) => Value::Float(self.const_coord_scale(v.to_float(), 2560.0)),
                None => Value::DEFAULT,
            };
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

        // ---- Round / match clock (audit #21) --------------------------------
        // These read the engine-global round coordinator's view, pushed onto the
        // character by `fp-engine`'s `Match` via `set_round_view` before each
        // tick. A bare `Character` (no coordinator) carries the safe default view
        // (`RoundState 0` = intro, `GameTime 0`, match not over), so each of these
        // reads `0` when evaluated with no `Match` in hand — exactly the value
        // MUGEN reports before a round/match is under way. KFM gates its
        // intro-freeze and the wood-kick `Explod` on `RoundState`.
        if name.eq_ignore_ascii_case("RoundState") {
            return Value::Int(self.round_view.round_state);
        }
        if name.eq_ignore_ascii_case("GameTime") {
            return Value::Int(self.round_view.game_time);
        }
        if name.eq_ignore_ascii_case("MatchOver") {
            return Value::from(self.round_view.match_over);
        }
        // `RoundNo` is the current round number (1-based); `RoundsExisted` is the
        // number of rounds this player has already completed this match
        // (`RoundNo - 1` for a fighter present since round 1). Both read the
        // coordinator-pushed `round_view`; a bare `Character` carries the default
        // (`RoundNo 0`, `RoundsExisted 0`), reading `0` with no `Match` in hand
        // (T016).
        if name.eq_ignore_ascii_case("RoundNo") {
            return Value::Int(self.round_view.round_no);
        }
        if name.eq_ignore_ascii_case("RoundsExisted") {
            return Value::Int(self.round_view.rounds_existed);
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
        // (Round / match state — `RoundState`/`GameTime`/`MatchOver` (audit #21)
        // and `RoundNo`/`RoundsExisted` (T016) — is no longer deferred: all five
        // are answered above from the coordinator-pushed `round_view`.)
        // * Cross-entity geometry: `P2Dist`, `P2BodyDist`, the screen-edge
        //   distances (`FrontEdgeDist`/`BackEdgeDist`/`FrontEdgeBodyDist`/
        //   `BackEdgeBodyDist`/`ScreenPos`), and opponent reads via `p2, ...` /
        //   `enemy, ...` redirects are NOT answered here on the self-only
        //   `Character` — they need the opponent and stage. They are computed by
        //   the per-tick cross-entity wrapper [`EvalCtx`], which delegates the
        //   self-only triggers back to this impl. A bare `Character` evaluated on
        //   its own still reports `0` for them (no opponent in view).
        // * Animation table queries: `SelfAnimExist`. Needs the loaded `.air`
        //   action set, which the executor owns rather than `Character`. It is
        //   now answered by the per-tick cross-entity wrapper [`EvalCtx`] (audit
        //   P22), which threads the `.air` actions in via [`AnimSet`]. A bare
        //   `Character` evaluated on its own (no `.air` in view) still falls
        //   through to `0` here — i.e. "action absent" — which is also what the
        //   opponent context reports, since it carries an empty `AnimSet`.

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

    fn assign(&self, bank: AssignBank, index: i32, value: Value) -> Value {
        // In-expression assignment (`var(n) := e`, T036). The VM evaluates against
        // a shared `&self`, so the write is buffered into the interior-mutable
        // overlay here and flushed into the real bank by the executor at the next
        // tick boundary (see [`Character::flush_var_assignments`]). The overlay is
        // read-back first by `read_var`/etc., so a later read in the SAME
        // expression already sees this write. The returned value is coerced to the
        // bank's element type so it matches a subsequent read.
        let stored = match bank {
            AssignBank::Var | AssignBank::SysVar => Value::Int(value.to_int()),
            AssignBank::FVar | AssignBank::SysFVar => Value::Float(value.to_float()),
        };
        self.var_assignments
            .borrow_mut()
            .push((bank, index, stored));
        stored
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

    fn random(&self) -> i32 {
        // The RNG seam (faithfulness audit #28): advance this character's own
        // deterministic Park–Miller stream and return the raw draw. The
        // evaluator maps it to MUGEN's `random` ([0,999]) / `random(lo,hi)`
        // range. State lives in `rng_seed` (plain, serializable) so it survives
        // rollback. The default trait impl returns a fixed 0 — overriding it
        // here is what makes `random` actually random rather than constant 0.
        self.draw_random()
    }

    fn hitdef_attr_matches(&self, standtype: &str, attr_codes: &[String]) -> bool {
        // The `HitDefAttr = <standtype>, <attr-list>` seam (Task A): test this
        // character's currently-active HitDef against the requested stand-type and
        // attack-code list. With no HitDef active the answer is `false` (the
        // trigger never fires), which is correct — `HitDefAttr` is only meaningful
        // while an attack is in flight.
        match &self.active_hitdef {
            Some(hd) => Self::attack_attr_matches(&hd.attr, standtype, attr_codes),
            None => false,
        }
    }

    fn redirect(&self, _target: fp_vm::Redirect) -> Option<&dyn EvalContext> {
        // A bare `Character` is a *self-only* evaluation context with no view of
        // its relations, so every redirect resolves to `None` here. The
        // cross-entity context is supplied per tick by [`EvalCtx`], which wraps a
        // `Character` together with its opponent and the stage and overrides
        // `redirect` (and the opponent-dependent triggers) to see the other
        // entity. `Character`'s own impl stays self-only so that a redirect
        // target's *nested* redirects (e.g. the inner level of `p2, ...`) bottom
        // out rather than looping.
        None
    }
}

/// The engine-global **round / match clock** a character's triggers read across:
/// `RoundState`, `GameTime`, `MatchOver`, `RoundNo`, and `RoundsExisted`
/// (faithfulness audits #21 and T016).
///
/// These triggers describe state the *round coordinator* owns
/// (`fp-engine`'s `Match`), not a single `Character` — a lone fighter has no
/// round phase, no game clock, no round number, and no match-over flag of its
/// own. The coordinator builds a `RoundView` each tick from its round-state
/// machine and pushes it onto each character via [`Character::set_round_view`]
/// **before** ticking it, so the character's CNS triggers see the live values
/// (KFM gates its intro-freeze and the wood-kick `Explod` on `RoundState`).
///
/// It is a tiny `Copy` value, mirroring [`StageView`]: it lives in `fp-character`
/// (rather than `fp-engine`, which depends on this crate) so a `Character` can
/// carry one without a dependency cycle. A bare / freshly-constructed `Character`
/// carries the [`RoundView::default`] (`RoundState 0` = intro, `GameTime 0`,
/// `RoundNo 0`, `RoundsExisted 0`, match not over) — the documented safe default
/// for a character evaluated with no coordinator in view, so the self-only
/// triggers degrade to `0` exactly as before this was wired.
///
/// ## Field semantics (MUGEN-faithful)
///
/// - [`round_state`](Self::round_state) — MUGEN's `RoundState`: `0` intro, `1`
///   fight (control given, pre-KO), `2` pre-over (a KO or time-over just occurred;
///   the win-pose window), `3` over (the round is ending). `fp-engine`'s
///   `RoundState::{Intro, Fight, Ko, Win}` maps to `0/1/2/3` respectively.
/// - [`game_time`](Self::game_time) — MUGEN's `GameTime`: a monotonic count of
///   total game ticks elapsed (it does **not** reset between rounds).
/// - [`match_over`](Self::match_over) — MUGEN's `MatchOver`: `true` once the whole
///   best-of-N match is decided, else `false`.
/// - [`round_no`](Self::round_no) — MUGEN's `RoundNo`: the current round number,
///   1-based (round 1 is the first round of the match).
/// - [`rounds_existed`](Self::rounds_existed) — MUGEN's `RoundsExisted`: the
///   number of rounds the player has already completed this match. For a fighter
///   present since the first round this is `round_no - 1` (so it is `0` during the
///   first round and increments as each round ends).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RoundView {
    /// MUGEN's `RoundState` code: `0` intro, `1` fight, `2` pre-over (KO / time
    /// over), `3` over. Defaults to `0` (intro) for a character with no round
    /// coordinator in view.
    pub round_state: i32,
    /// MUGEN's `GameTime`: total game ticks elapsed, a monotonic counter that is
    /// **not** reset between the rounds of a match. Defaults to `0`.
    pub game_time: i32,
    /// MUGEN's `MatchOver`: `true` once the whole match is decided, else `false`.
    /// Defaults to `false`.
    pub match_over: bool,
    /// MUGEN's `RoundNo`: the current round number, 1-based. Defaults to `0` for a
    /// character with no round coordinator in view (the coordinator pushes `1` for
    /// the opening round). See [`RoundView`].
    pub round_no: i32,
    /// MUGEN's `RoundsExisted`: how many rounds this player has already completed
    /// this match. For a fighter present from the first round this is
    /// `round_no - 1` (`0` during round 1, climbing as rounds end). Defaults to
    /// `0`.
    pub rounds_existed: i32,
}

impl RoundView {
    /// Builds a round view from a `RoundState` code, the game-time counter, the
    /// match-over flag, the 1-based round number (`RoundNo`), and the player's
    /// completed-round count (`RoundsExisted`). The `round_state` is taken
    /// verbatim (callers pass the MUGEN `0..=3` code); see the type docs for the
    /// mapping.
    #[must_use]
    pub const fn new(
        round_state: i32,
        game_time: i32,
        match_over: bool,
        round_no: i32,
        rounds_existed: i32,
    ) -> Self {
        Self {
            round_state,
            game_time,
            match_over,
            round_no,
            rounds_existed,
        }
    }
}

/// A horizontal slice of the stage the fighters are pinned to, in world X.
///
/// This is the minimal stage view the cross-entity evaluation context
/// ([`EvalCtx`]) needs to answer the screen-edge distance triggers
/// (`FrontEdgeDist`, `BackEdgeDist`, …). It is a small `Copy` value so it can be
/// threaded through the per-tick eval path cheaply.
///
/// It lives in `fp-character` (rather than `fp-engine`, which owns the richer
/// `StageBounds`) because `fp-engine` depends on `fp-character`; putting the type
/// here lets `Character::tick` take it as a parameter without a dependency cycle.
/// `fp-engine` converts its own bounds into a `StageView` when ticking a match.
///
/// `left`/`right` are the world X of the playfield's left/right edges. **Approximation:**
/// `fp-engine` currently populates these from the static **stage bounds**
/// (`boundleft`/`boundright`), so the edge-distance triggers (`FrontEdgeDist` etc.)
/// measure to the fixed stage boundary, not the *scrolling camera/screen* edge that
/// MUGEN uses. The two coincide while the camera is centred (the only case modelled
/// today); once a scrolling camera lands, thread the camera's world-X window here
/// instead. A well-formed view has `left <= right`, but a reversed pair still yields
/// finite, deterministic distances (never a panic).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageView {
    /// World X of the left playfield edge (currently the stage `boundleft`).
    pub left: f32,
    /// World X of the right playfield edge (currently the stage `boundright`).
    pub right: f32,
    /// `GameWidth` — the stage's logical screen **width** in localcoord units
    /// (the 320×240-class space the edge / `ScreenPos` triggers measure in).
    /// Set from the stage's `[StageInfo] localcoord` width; defaults to MUGEN's
    /// classic `320` for the no-stage driver paths. See [`StageView::DEFAULT_GAME_WIDTH`].
    pub game_width: f32,
    /// `GameHeight` — the stage's logical screen **height** in localcoord units.
    /// Set from the stage's `[StageInfo] localcoord` height; defaults to `240`.
    /// See [`StageView::DEFAULT_GAME_HEIGHT`].
    pub game_height: f32,
}

impl StageView {
    /// MUGEN's classic logical screen width (`320`), used when no stage
    /// `localcoord` is in hand.
    pub const DEFAULT_GAME_WIDTH: f32 = 320.0;
    /// MUGEN's classic logical screen height (`240`), used when no stage
    /// `localcoord` is in hand.
    pub const DEFAULT_GAME_HEIGHT: f32 = 240.0;

    /// Creates a stage view from the left and right screen-edge world X values,
    /// using the classic `320×240` logical screen dimensions for
    /// `GameWidth`/`GameHeight`.
    #[must_use]
    pub const fn new(left: f32, right: f32) -> Self {
        Self {
            left,
            right,
            game_width: Self::DEFAULT_GAME_WIDTH,
            game_height: Self::DEFAULT_GAME_HEIGHT,
        }
    }

    /// Creates a stage view with explicit left/right world-X edges **and**
    /// logical `GameWidth`/`GameHeight` (localcoord) dimensions. Used by
    /// `fp-engine` to thread a stage's parsed `localcoord` onto the view that
    /// answers the game-dimension and screen-edge triggers (T060).
    #[must_use]
    pub const fn with_dims(left: f32, right: f32, game_width: f32, game_height: f32) -> Self {
        Self {
            left,
            right,
            game_width,
            game_height,
        }
    }

    /// The world X of the **visible left screen edge**, camera-relative.
    ///
    /// The visible window is the logical [`game_width`](Self::game_width) span
    /// centered on the playfield midpoint `(left + right) / 2` (the camera is
    /// modelled as centred — the only case modelled today). This keeps
    /// `right_edge() - left_edge() == game_width` exactly, in the same world-X
    /// convention the edge-distance triggers use.
    #[must_use]
    pub fn left_edge(&self) -> f32 {
        self.center_x() - self.game_width / 2.0
    }

    /// The world X of the **visible right screen edge**, camera-relative.
    /// See [`left_edge`](Self::left_edge).
    #[must_use]
    pub fn right_edge(&self) -> f32 {
        self.center_x() + self.game_width / 2.0
    }

    /// The world Y of the **visible top screen edge**.
    ///
    /// World Y increases downward with the ground plane at `Y = 0` (up is
    /// negative Y), so the top of the visible window sits `game_height` units
    /// **above** the floor: `top_edge() == -game_height`, `bottom_edge() == 0`.
    /// This keeps `bottom_edge() - top_edge() == game_height`.
    #[must_use]
    pub fn top_edge(&self) -> f32 {
        -self.game_height
    }

    /// The world Y of the **visible bottom screen edge** (the ground plane,
    /// `Y = 0`). See [`top_edge`](Self::top_edge).
    #[must_use]
    pub fn bottom_edge(&self) -> f32 {
        0.0
    }

    /// The world X midpoint of the playfield — the modelled (centred) camera
    /// center used to anchor the visible screen edges.
    fn center_x(&self) -> f32 {
        (self.left + self.right) / 2.0
    }
}

impl Default for StageView {
    /// A symmetric default view matching `fp-engine`'s default stage bounds
    /// (`[-200, 200]`) with the classic `320×240` logical screen, used by the
    /// single-character / no-stage driver paths.
    fn default() -> Self {
        Self {
            left: -200.0,
            right: 200.0,
            game_width: Self::DEFAULT_GAME_WIDTH,
            game_height: Self::DEFAULT_GAME_HEIGHT,
        }
    }
}

/// A read-only view of a character's loaded **animation action set**, the
/// minimum the cross-entity evaluation context ([`EvalCtx`]) needs to answer the
/// `SelfAnimExist(n)` trigger (audit P22).
///
/// MUGEN's `SelfAnimExist(n)` reports whether action number `n` exists in the
/// character's *own* `.air` table; `common1` uses it to pick anim fallbacks (e.g.
/// `[Statedef 50]`'s `SelfAnimExist(anim + 3)` selects the falling variant when
/// present, and `[Statedef 45]`'s `SelfAnimExist(44)` picks anim `44` over `41`
/// for an air jump). The action set lives with the executor (which owns the
/// [`AirFile`](fp_formats::air::AirFile)), not on [`Character`], so this thin
/// borrow threads it into the eval path.
///
/// It is a `Copy` wrapper around an optional shared reference to the loaded
/// `action → AnimAction` map, so it can ride along in the (also `Copy`)
/// `EvalEnv`/[`EvalCtx`] without forcing an allocation. A [`default`](AnimSet::default)
/// `AnimSet` holds **no** actions: contexts with no `.air` in hand (the opponent
/// context, the out-of-tick `change_state` seam, and bare-`Character`
/// evaluation) use it, so `SelfAnimExist` there degrades to `0` (action absent)
/// rather than guessing — a documented, panic-free fallback.
#[derive(Clone, Copy, Default)]
pub struct AnimSet<'a> {
    /// The loaded `action number → action` map, or `None` when no `.air` table
    /// is in view (an empty set: every action is reported absent).
    actions: Option<&'a HashMap<i32, AnimAction>>,
}

impl<'a> AnimSet<'a> {
    /// Wraps a loaded animation action map for `SelfAnimExist` resolution.
    #[must_use]
    pub fn new(actions: &'a HashMap<i32, AnimAction>) -> Self {
        Self {
            actions: Some(actions),
        }
    }

    /// Returns whether animation action `n` exists in this set.
    ///
    /// Always `false` for the empty ([`default`](AnimSet::default)) set. Never
    /// panics.
    #[must_use]
    pub fn contains(&self, n: i32) -> bool {
        self.actions.is_some_and(|a| a.contains_key(&n))
    }

    /// Looks up animation action `n`, or `None` when the set is empty / the
    /// action is absent. Lets a state entry seed `AnimTime` from the new action
    /// the moment the anim changes (so the previous anim's `AnimTime` cannot
    /// leak into the new state's first-tick trigger evaluation). Never panics.
    #[must_use]
    pub fn action(&self, n: i32) -> Option<&'a AnimAction> {
        self.actions.and_then(|a| a.get(&n))
    }
}

/// A per-tick **cross-entity** evaluation context: a [`Character`] (`me`) viewed
/// together with its opponent and the stage.
///
/// `Character` on its own is a *self-only* [`EvalContext`] — it can answer
/// `Life`, `Pos X`, `var(0)`, … about itself, but the opponent-dependent triggers
/// (`P2Dist`, `P2BodyDist`, `p2, life`, …) and every redirect resolve to the safe
/// default because a lone character has no view of the other entity. `EvalCtx`
/// supplies that missing view for the duration of one [`Character::tick_with`]:
///
/// - **Self-only reads delegate** to `me`'s [`EvalContext`] impl: `trigger_str`,
///   `command_active`, `random`, the variable banks, and every non-opponent
///   `trigger` name fall straight through to the wrapped `Character`.
/// - **Opponent-dependent triggers are computed here** from `me`, the opponent,
///   and the stage: `P2Dist`/`P2BodyDist` (facing-relative on X), `P2Life`/
///   `P2LifeMax`/`P2StateNo`/`P2MoveType`/`P2StateType`, and the screen-edge
///   distances (`FrontEdgeDist`/`BackEdgeDist`/`FrontEdgeBodyDist`/
///   `BackEdgeBodyDist`/`ScreenPos`).
/// - **`redirect` resolves the opponent targets** (`p2`/`enemy`/`enemynear(_)`)
///   to the opponent context and `root` to self; the entity-graph targets
///   (`parent`/`helper(id)` — T012; `target`/`partner`/`playerid(n)` — T014)
///   resolve against the installed [`EntityGraph`], falling back to `None` (the
///   sub-expression → `0`) when the owner wired no such relation (documented,
///   never a panic).
///
/// ## Borrow / lifetime shape
///
/// `opponent` is itself an `EvalCtx` (built once near the top of the tick with
/// *its* opponent set to `None`), so a single level of `p2, ...` works while the
/// opponent's own nested redirects bottom out — exactly MUGEN's behavior for a
/// non-helper's view of the other player. The opponent context borrows the
/// opponent `Character` immutably and is **not** mutated during `me`'s tick, so
/// `me`'s mutable controller dispatch and the immutable opponent view never
/// conflict. At each eval site the executor reborrows `&*self` into a fresh
/// `EvalCtx { me, .. }` that lives only for that one `eval` call and drops before
/// any `&mut self` mutation — so the whole thing type-checks with no `unsafe`.
pub struct EvalCtx<'a> {
    /// The character this context evaluates self-triggers against.
    me: &'a Character,
    /// The opponent's context, or `None` when there is no opponent.
    opponent: Option<&'a EvalCtx<'a>>,
    /// The stage edges, for the screen-edge distance triggers.
    stage: StageView,
    /// `me`'s loaded animation action set, for `SelfAnimExist(n)`. Empty (the
    /// [`AnimSet::default`]) when no `.air` is in view.
    anim: AnimSet<'a>,
    /// The multi-entity graph this context resolves `parent`/`root`/`helper(id)`
    /// (T012) and `target`/`partner`/`playerid(n)` (T014) redirects against.
    /// Empty (the [`EntityGraph::default`]) for a root player with no spawning
    /// chain / relations, in which case `root` falls back to `me` and the rest
    /// resolve to `None`.
    graph: EntityGraph<'a>,
}

/// The slice of the live multi-entity graph one [`EvalCtx`] can see, for
/// resolving the entity-relationship redirects `parent`, `root`, `helper(id)`
/// (T012) and `target`, `partner`, `playerid(n)` (T014).
///
/// MUGEN entities form a tree: a root player spawns helpers (each addressable by
/// a `helper(id)`), and every helper knows its immediate `parent` (its creator)
/// and the `root` at the top of the chain. Beyond that spawning chain, an entity
/// can address the opponent it most recently *hit* (`target`), a teammate
/// (`partner`), and any player/entity by its global id (`playerid(n)`). The
/// entity owner (`fp-engine`'s `Player` / `Match`, which holds the slot-map of
/// live entities) populates this graph for the duration of one tick so the
/// executor's redirect resolution ([`EvalCtx::redirect`]) can hop to a related
/// entity instead of bottoming out at `0`.
///
/// The graph is a tiny `Copy` bundle of immutable references (a few optional
/// `&Character`s plus two borrowed slices), so it threads through the `Copy`
/// [`EvalCtx`] / executor env with no allocation. A redirected related entity is
/// surfaced as a **self-only** context (a bare `&Character`), so a single hop —
/// `parent, life`, `root, stateno`, `helper(1), pos x`, `target, life`,
/// `playerid(2), stateno` — resolves while that related entity's own nested
/// redirects bottom out, exactly mirroring how the opponent (`p2, …`) redirect is
/// depth-limited.
#[derive(Clone, Copy, Default)]
pub struct EntityGraph<'a> {
    /// The immediate creator of `me` (the `parent` redirect), or `None` when `me`
    /// is a root player (no parent).
    parent: Option<&'a Character>,
    /// The root player at the top of `me`'s spawning chain (the `root` redirect),
    /// or `None` when `me` is itself the root — in which case `root` resolves to
    /// `me`.
    root: Option<&'a Character>,
    /// The helpers addressable from `me` via `helper(id)`, as `(id, entity)`
    /// pairs. The first pair whose id matches wins (MUGEN resolves the
    /// most-recently-spawned matching helper, but a single match is enough for
    /// the common one-helper-per-id case). Empty when `me` owns no helpers.
    helpers: &'a [(i32, &'a Character)],
    /// The entity `me` most recently hit — the `target` redirect (T014). `None`
    /// when `me` has no established target (it has not connected a hit, or the
    /// owner did not wire one this tick). In MUGEN `target` can address multiple
    /// victims by `target(id)`; this flat model carries the single most-recently
    /// hit opponent, which is the common case the `Target*` throw controllers act
    /// on.
    target: Option<&'a Character>,
    /// `me`'s teammate — the `partner` redirect (T014). `None` when `me` has no
    /// teammate (the common 1-v-1 case), in which case `partner, …` collapses to
    /// `0`.
    partner: Option<&'a Character>,
    /// Every entity addressable by its global id via `playerid(n)`, as
    /// `(id, entity)` pairs (T014). The first pair whose id matches wins. Empty
    /// when the owner wired no id table, in which case `playerid(n), …` collapses
    /// to `0`.
    players: &'a [(i32, &'a Character)],
    /// The owning player's full live-helper id list — the `helper_id` of every
    /// helper the *owning player* currently has alive, for the `NumHelper` /
    /// `NumHelper(id)` count triggers (NUMHELPER).
    ///
    /// Distinct from [`helpers`](Self::helpers): `helpers` carries the
    /// `(id, &Character)` pairs *this* entity can address via the `helper(id)`
    /// **redirect** (only populated for the root, since a helper has no sibling
    /// lookup, to avoid aliasing the slot-map mid-tick). `own_helper_ids` is a flat
    /// id-only slice the owner builds once per tick from its slot-map and installs
    /// on **both** the root's and every helper's context, so `NumHelper` reports
    /// the owning player's count regardless of which entity reads it. Empty for a
    /// root with no helpers (and for a bare/test context), in which case
    /// `NumHelper` reports `0`.
    own_helper_ids: &'a [i32],
    /// The owning player's full live-projectile id list — the `proj_id` of every
    /// projectile the *owning player* currently has alive, for the `NumProj`
    /// count trigger (T026).
    ///
    /// Built once per tick by the owner (`fp-engine`) from its projectile
    /// slot-map and installed on the context, exactly as
    /// [`own_helper_ids`](Self::own_helper_ids) is for `NumHelper`. Bare `NumProj`
    /// reports its length (the total live count). Empty for a player with no live
    /// projectiles (and for a bare/test context), in which case `NumProj` is `0`.
    /// (MUGEN's `NumProjID(id)` per-id form is not exposed by the parser today;
    /// the slice is id-keyed so it can be added without a shape change.)
    own_proj_ids: &'a [i32],
    /// The owning player's currently-bound *target* ids — the character id of every
    /// entity this player has hit and still holds as a `Target*` victim, for the
    /// `NumTarget` / `NumTarget(id)` count trigger (T061).
    ///
    /// Built once per tick by the owner (`fp-engine`) and installed exactly as
    /// [`own_helper_ids`](Self::own_helper_ids) / [`own_proj_ids`](Self::own_proj_ids)
    /// are. Bare `NumTarget` reports its length (the total bound-target count);
    /// `NumTarget(id)` counts only entries whose id matches. Empty when the owner
    /// holds no target (and for a bare/test context); in the flat 1-v-1 model the
    /// slice carries at most the single opponent the player most recently hit, so
    /// `NumTarget` is `0` or `1`. When no slice is wired, the self-only fallback in
    /// [`cross_entity_trigger`](EvalCtx::cross_entity_trigger) reads
    /// [`Character::has_target`] instead.
    own_target_ids: &'a [i32],
}

impl<'a> EntityGraph<'a> {
    /// Builds an entity graph from the optional `parent`/`root` chain and the
    /// borrowed `(id, &Character)` helper lookup, with the `target`/`partner`/
    /// `playerid` slots empty.
    ///
    /// Pass `None` for `parent`/`root` and an empty slice for a root player that
    /// owns no helpers (equivalent to [`EntityGraph::default`]). The references
    /// must outlive the [`EvalCtx`] the graph is installed on; the owner
    /// (`fp-engine`) builds this for the span of one tick. Layer on the T014
    /// relations with [`EntityGraph::with_target`], [`EntityGraph::with_partner`],
    /// and [`EntityGraph::with_players`].
    #[must_use]
    pub fn new(
        parent: Option<&'a Character>,
        root: Option<&'a Character>,
        helpers: &'a [(i32, &'a Character)],
    ) -> Self {
        Self {
            parent,
            root,
            helpers,
            target: None,
            partner: None,
            players: &[],
            own_helper_ids: &[],
            own_proj_ids: &[],
            own_target_ids: &[],
        }
    }

    /// Installs the `target` redirect — the entity `me` most recently hit (T014),
    /// returning the updated graph. A builder method so the existing
    /// [`EntityGraph::new`] call sites keep working unchanged.
    #[must_use]
    pub fn with_target(mut self, target: Option<&'a Character>) -> Self {
        self.target = target;
        self
    }

    /// Installs the `partner` redirect — `me`'s teammate, if any (T014),
    /// returning the updated graph.
    #[must_use]
    pub fn with_partner(mut self, partner: Option<&'a Character>) -> Self {
        self.partner = partner;
        self
    }

    /// Installs the `playerid(n)` lookup table — the `(id, entity)` pairs every
    /// `playerid(n)` redirect resolves against (T014), returning the updated
    /// graph. The slice must outlive the [`EvalCtx`] the graph is installed on.
    #[must_use]
    pub fn with_players(mut self, players: &'a [(i32, &'a Character)]) -> Self {
        self.players = players;
        self
    }

    /// Installs the owning player's full live-helper id list — the source for the
    /// `NumHelper` / `NumHelper(id)` count triggers (NUMHELPER), returning the
    /// updated graph.
    ///
    /// The owner (`fp-engine`) builds this once per tick from its helper slot-map
    /// and installs the **same** slice on the root's and every helper's context, so
    /// `NumHelper` reports the owning player's count from any entity. The slice must
    /// outlive the [`EvalCtx`] the graph is installed on. A builder method so
    /// existing [`EntityGraph::new`] call sites keep working unchanged.
    #[must_use]
    pub fn with_own_helper_ids(mut self, own_helper_ids: &'a [i32]) -> Self {
        self.own_helper_ids = own_helper_ids;
        self
    }

    /// Installs the owning player's full live-projectile id list — the source for
    /// the `NumProj` count trigger (T026), returning the updated graph.
    ///
    /// The owner (`fp-engine`) builds this once per tick from its projectile
    /// slot-map, exactly as [`with_own_helper_ids`](Self::with_own_helper_ids) does
    /// for helpers. The slice must outlive the [`EvalCtx`] the graph is installed
    /// on. A builder method so existing [`EntityGraph::new`] call sites keep
    /// working unchanged.
    #[must_use]
    pub fn with_own_proj_ids(mut self, own_proj_ids: &'a [i32]) -> Self {
        self.own_proj_ids = own_proj_ids;
        self
    }

    /// Installs the owning player's currently-bound target id list — the source for
    /// the `NumTarget` / `NumTarget(id)` count trigger (T061), returning the updated
    /// graph.
    ///
    /// The owner (`fp-engine`) builds this once per tick, exactly as
    /// [`with_own_proj_ids`](Self::with_own_proj_ids) does for projectiles. In the
    /// flat 1-v-1 model the slice carries at most the single opponent the player
    /// most recently hit (its character id). The slice must outlive the [`EvalCtx`]
    /// the graph is installed on. A builder method so existing
    /// [`EntityGraph::new`] call sites keep working unchanged.
    #[must_use]
    pub fn with_own_target_ids(mut self, own_target_ids: &'a [i32]) -> Self {
        self.own_target_ids = own_target_ids;
        self
    }

    /// Whether a target-id list was wired on this graph (via
    /// [`with_own_target_ids`](Self::with_own_target_ids)).
    ///
    /// `false` for a bare/test context whose owner installed no list — in which
    /// case the `NumTarget` trigger falls back to [`Character::has_target`] rather
    /// than reporting the empty-slice count.
    #[must_use]
    pub fn has_own_target_ids(&self) -> bool {
        !self.own_target_ids.is_empty()
    }

    /// Counts the owning player's currently-bound targets for the `NumTarget`
    /// trigger (T061): with `id = None` the **total** number bound; with
    /// `id = Some(n)` the number whose target id matches `n`. Returns `0` when the
    /// owner holds none (or wired no list — e.g. a bare/test context); never panics.
    ///
    /// Counts the flat [`own_target_ids`](Self::own_target_ids) slice the owner
    /// installs (via [`with_own_target_ids`](Self::with_own_target_ids)).
    #[must_use]
    pub fn num_targets(&self, id: Option<i32>) -> i32 {
        let count = match id {
            None => self.own_target_ids.len(),
            Some(n) => self.own_target_ids.iter().filter(|&&tid| tid == n).count(),
        };
        // The flat slice is bounded by the live opponent count, so this never
        // overflows i32; saturate defensively anyway rather than risk a panic.
        i32::try_from(count).unwrap_or(i32::MAX)
    }

    /// Counts the owning player's live projectiles for the `NumProj` trigger
    /// (T026): the total number alive. Returns `0` when the owner has none (or
    /// wired no list — e.g. a bare/test context); never panics.
    ///
    /// Counts the flat `own_proj_ids` slice the owner installs (via
    /// [`with_own_proj_ids`](Self::with_own_proj_ids)), so the answer is the owning
    /// player's live-projectile count.
    #[must_use]
    pub fn num_proj(&self) -> i32 {
        // The slot-map is bounded (`MAX_PROJECTILES_PER_PLAYER`), so this never
        // overflows i32; saturate defensively anyway rather than risk a panic.
        i32::try_from(self.own_proj_ids.len()).unwrap_or(i32::MAX)
    }

    /// Counts the owning player's live helpers for the `NumHelper` triggers
    /// (NUMHELPER): with `id = None` the **total** number of live helpers; with
    /// `id = Some(n)` the number whose `helper_id == n`. Returns `0` when the owner
    /// has none (or wired no list — e.g. a bare/test context), never panics.
    ///
    /// This counts the flat [`own_helper_ids`](Self::own_helper_ids) slice the
    /// owner installs on both the root and the helpers, so the answer is the
    /// owning player's count regardless of which entity reads it.
    #[must_use]
    pub fn num_helpers(&self, id: Option<i32>) -> i32 {
        let count = match id {
            None => self.own_helper_ids.len(),
            Some(n) => self.own_helper_ids.iter().filter(|&&hid| hid == n).count(),
        };
        // The slot-map is bounded (`MAX_HELPERS_PER_PLAYER`), so this never
        // overflows i32; saturate defensively anyway rather than risk a panic.
        i32::try_from(count).unwrap_or(i32::MAX)
    }

    /// The immediate parent entity (`parent` redirect), if any.
    #[must_use]
    pub fn parent(&self) -> Option<&'a Character> {
        self.parent
    }

    /// The root entity (`root` redirect), if a distinct root is recorded.
    /// `None` means `me` is itself the root.
    #[must_use]
    pub fn root(&self) -> Option<&'a Character> {
        self.root
    }

    /// Looks up the helper addressable by `id` (`helper(id)` redirect), returning
    /// the first matching entity or `None` when no helper carries that id.
    #[must_use]
    pub fn helper(&self, id: i32) -> Option<&'a Character> {
        self.helpers
            .iter()
            .find(|(hid, _)| *hid == id)
            .map(|(_, ent)| *ent)
    }

    /// The entity `me` most recently hit (`target` redirect, T014), or `None`
    /// when `me` has no established target.
    #[must_use]
    pub fn target(&self) -> Option<&'a Character> {
        self.target
    }

    /// `me`'s teammate (`partner` redirect, T014), or `None` in a 1-v-1.
    #[must_use]
    pub fn partner(&self) -> Option<&'a Character> {
        self.partner
    }

    /// Looks up the entity addressable by global `id` (`playerid(id)` redirect,
    /// T014), returning the first matching entity or `None` when none carries it.
    #[must_use]
    pub fn player(&self, id: i32) -> Option<&'a Character> {
        self.players
            .iter()
            .find(|(pid, _)| *pid == id)
            .map(|(_, ent)| *ent)
    }
}

impl<'a> EvalCtx<'a> {
    /// Builds a cross-entity context wrapping `me`, with `opponent` and `stage`.
    ///
    /// `me`'s own animation action set defaults to **empty**, so `SelfAnimExist`
    /// reports every action absent (`0`). Use [`EvalCtx::with_anim`] to supply the
    /// loaded `.air` actions so `SelfAnimExist(n)` resolves against them.
    #[must_use]
    pub fn new(me: &'a Character, opponent: Option<&'a EvalCtx<'a>>, stage: StageView) -> Self {
        Self {
            me,
            opponent,
            stage,
            anim: AnimSet::default(),
            graph: EntityGraph::default(),
        }
    }

    /// Builds a cross-entity context like [`EvalCtx::new`], additionally giving
    /// `me`'s loaded animation action set so `SelfAnimExist(n)` resolves against
    /// the real `.air` table instead of the empty default.
    #[must_use]
    pub fn with_anim(
        me: &'a Character,
        opponent: Option<&'a EvalCtx<'a>>,
        stage: StageView,
        anim: AnimSet<'a>,
    ) -> Self {
        Self {
            me,
            opponent,
            stage,
            anim,
            graph: EntityGraph::default(),
        }
    }

    /// Installs the helper-entity [`graph`](EvalCtx::graph) on this context so the
    /// `parent`/`root`/`helper(id)` redirects resolve against the spawning chain
    /// (T012), returning the updated context.
    ///
    /// A builder method (rather than another positional constructor) so existing
    /// call sites keep working unchanged: a context without a graph behaves as a
    /// root player (`root` → self, `parent`/`helper(id)` → `None`), and a caller
    /// that owns a helper chain layers it on with `.with_graph(graph)`.
    #[must_use]
    pub fn with_graph(mut self, graph: EntityGraph<'a>) -> Self {
        self.graph = graph;
        self
    }

    /// The opponent `Character`, if any (the entity behind the opponent context).
    fn opponent_char(&self) -> Option<&'a Character> {
        self.opponent.map(|o| o.me)
    }

    /// The world X position of this context's own character (`me`).
    ///
    /// Used by the executor's `facep2` state-entry handling: an entering
    /// character reads its *opponent's* `me_pos_x()` (the opponent context's own
    /// character) to decide which way to face. Crate-internal because the `me`
    /// field is private to this module.
    pub(crate) fn me_pos_x(&self) -> f32 {
        self.me.pos.x
    }

    /// `P2Dist X` — the **facing-relative** horizontal distance to the opponent:
    /// `(opponent.pos.x - me.pos.x) * facing_sign(me)`. Positive means the
    /// opponent is in front of `me`. With no opponent the safe default `0`.
    fn p2dist_x(&self) -> f32 {
        match self.opponent_char() {
            Some(o) => (o.pos.x - self.me.pos.x) * self.me.facing.sign() as f32,
            None => 0.0,
        }
    }

    /// `P2Dist Y` — the vertical distance to the opponent (`opponent.pos.y -
    /// me.pos.y`, no facing flip on Y). With no opponent the safe default `0`.
    fn p2dist_y(&self) -> f32 {
        match self.opponent_char() {
            Some(o) => o.pos.y - self.me.pos.y,
            None => 0.0,
        }
    }

    /// `P2BodyDist X` — the facing-relative edge-to-edge horizontal distance:
    /// `P2Dist X` shrunk (toward zero, preserving sign) by each fighter's
    /// half-width on the side facing the gap. When the opponent is in front
    /// (`P2Dist X >= 0`) that is `me`'s `size.ground.front`; when crossed up
    /// (behind, `P2Dist X < 0`) it is `me`'s `size.ground.back`. The opponent
    /// normally faces `me`, so its `front` width is used. With no opponent the
    /// safe default `0`.
    fn p2bodydist_x(&self) -> f32 {
        match self.opponent_char() {
            Some(o) => {
                let d = self.p2dist_x();
                let my_w = if d >= 0.0 {
                    self.me.constants.size.ground_front
                } else {
                    self.me.constants.size.ground_back
                } as f32;
                let opp_w = o.constants.size.ground_front as f32;
                let widths = my_w + opp_w;
                // Shrink the gap toward zero by both widths, preserving sign; the
                // result may be negative when the bodies overlap (MUGEN-faithful).
                if d >= 0.0 {
                    d - widths
                } else {
                    d + widths
                }
            }
            None => 0.0,
        }
    }

    /// `FrontEdgeDist` — distance from `me` to the screen edge it faces (positive
    /// when inside the playfield). Facing right ⇒ the right edge; facing left ⇒
    /// the left edge.
    fn front_edge_dist(&self) -> f32 {
        match self.me.facing {
            Facing::Right => self.stage.right - self.me.pos.x,
            Facing::Left => self.me.pos.x - self.stage.left,
        }
    }

    /// `BackEdgeDist` — distance from `me` to the screen edge behind it.
    fn back_edge_dist(&self) -> f32 {
        match self.me.facing {
            Facing::Right => self.me.pos.x - self.stage.left,
            Facing::Left => self.stage.right - self.me.pos.x,
        }
    }

    /// Resolves an opponent-dependent trigger by (case-insensitive) name to a
    /// [`Value`], or [`None`] if `name` is not one this context computes (so the
    /// caller can delegate to the wrapped `Character`).
    ///
    /// Every value here is the safe default `0` when there is no opponent; none
    /// of these ever panics. `P2*` reads of the opponent's own state route
    /// through the opponent context's [`EvalContext::trigger`] so they report the
    /// opponent's value (and stay correct if the opponent's own reporting
    /// changes).
    fn cross_entity_trigger(&self, name: &str, args: &[Value]) -> Option<Value> {
        // Axis helper: X = 0 (or absent), Y = 1, per the evaluator's coding.
        let is_y = || matches!(args.first().map(|v| v.to_int()), Some(AXIS_Y));

        if name.eq_ignore_ascii_case("P2Dist") {
            return Some(Value::Float(if is_y() {
                self.p2dist_y()
            } else {
                self.p2dist_x()
            }));
        }
        if name.eq_ignore_ascii_case("P2BodyDist") {
            // BodyDist Y has no width adjustment; it equals P2Dist Y.
            return Some(Value::Float(if is_y() {
                self.p2dist_y()
            } else {
                self.p2bodydist_x()
            }));
        }
        if name.eq_ignore_ascii_case("FrontEdgeDist") {
            return Some(Value::Float(self.front_edge_dist()));
        }
        if name.eq_ignore_ascii_case("BackEdgeDist") {
            return Some(Value::Float(self.back_edge_dist()));
        }
        if name.eq_ignore_ascii_case("FrontEdgeBodyDist") {
            // Edge-to-body: subtract this player's front half-width.
            let w = self.me.constants.size.ground_front as f32;
            return Some(Value::Float(self.front_edge_dist() - w));
        }
        if name.eq_ignore_ascii_case("BackEdgeBodyDist") {
            let w = self.me.constants.size.ground_back as f32;
            return Some(Value::Float(self.back_edge_dist() - w));
        }
        if name.eq_ignore_ascii_case("ScreenPos") {
            // ScreenPos X/Y: position relative to the left/top screen edge. Only X
            // is meaningful from a `StageView`; Y mirrors the world Y (no vertical
            // camera modeled), matching the single-camera assumption.
            return Some(Value::Float(if is_y() {
                self.me.pos.y
            } else {
                self.me.pos.x - self.stage.left
            }));
        }

        // `GameWidth` / `GameHeight` (T060) — the stage's logical screen
        // dimensions in localcoord units (the 320×240-class space the
        // `ScreenPos` / edge math already lives in). Zero-arg constants threaded
        // from the stage's `[StageInfo] localcoord` onto the `StageView`; the
        // no-stage driver paths report the classic `320×240`. Never panics.
        if name.eq_ignore_ascii_case("GameWidth") {
            return Some(Value::Float(self.stage.game_width));
        }
        if name.eq_ignore_ascii_case("GameHeight") {
            return Some(Value::Float(self.stage.game_height));
        }

        // `LeftEdge` / `RightEdge` / `TopEdge` / `BottomEdge` (T060) — the
        // camera-relative world coords of the visible screen edges, in the same
        // coordinate convention `FrontEdgeDist`/`BackEdgeDist` measure against.
        // The visible window is the `GameWidth × GameHeight` logical span
        // centred on the playfield (the camera is modelled centred), so
        // `RightEdge - LeftEdge == GameWidth` and (world Y down, floor at Y=0)
        // `BottomEdge - TopEdge == GameHeight`. Zero-arg; never panics.
        if name.eq_ignore_ascii_case("LeftEdge") {
            return Some(Value::Float(self.stage.left_edge()));
        }
        if name.eq_ignore_ascii_case("RightEdge") {
            return Some(Value::Float(self.stage.right_edge()));
        }
        if name.eq_ignore_ascii_case("TopEdge") {
            return Some(Value::Float(self.stage.top_edge()));
        }
        if name.eq_ignore_ascii_case("BottomEdge") {
            return Some(Value::Float(self.stage.bottom_edge()));
        }

        // `SelfAnimExist(n)` — does action number `n` exist in `me`'s loaded
        // `.air` table? Resolved here because the action set lives with the
        // executor's `AirFile`, not on the self-only `Character`. The VM parses
        // it as a function-call trigger, so `n` arrives as the first argument.
        // A missing or non-integer argument, or an empty action set (no `.air`
        // in view — opponent context / out-of-tick seam / bare `Character`),
        // yields `0` (action absent). Never panics.
        if name.eq_ignore_ascii_case("SelfAnimExist") {
            let exists = args.first().is_some_and(|v| self.anim.contains(v.to_int()));
            return Some(Value::from(exists));
        }

        // `NumHelper` / `NumHelper(id)` — the count of the owning player's live
        // helpers (NUMHELPER). Resolved here because the helper slot-map lives with
        // the entity owner (`fp-engine`'s `Player`), not on the self-only
        // `Character`; the owner installs its full live-helper id list on the graph
        // each tick (see [`EntityGraph::with_own_helper_ids`]) for both the root and
        // every helper. Bare `NumHelper` (no argument) is the total; `NumHelper(id)`
        // (the VM parses the parenthesized form as a function-call trigger, so `id`
        // arrives as the first argument) counts only helpers carrying that id. A
        // root with no helpers — or any context with no list wired — reports `0`.
        // This is the spawn-once latch: once a helper of id N exists,
        // `NumHelper(N) = 0` becomes false, so a `triggerall`-guarded `Helper`
        // controller fires exactly once instead of every tick. Never panics.
        if name.eq_ignore_ascii_case("NumHelper") {
            let id = args.first().map(|v| v.to_int());
            return Some(Value::Int(self.graph.num_helpers(id)));
        }

        // `NumProj` — the count of the owning player's live projectiles (T026).
        // Resolved here (not on the self-only `Character`) because the projectile
        // slot-map lives with the entity owner (`fp-engine`'s `Player`); the owner
        // installs its full live-projectile id list on the graph each tick (see
        // [`EntityGraph::with_own_proj_ids`]). A player with no live projectiles —
        // or any context with no list wired — reports `0`. Never panics.
        if name.eq_ignore_ascii_case("NumProj") {
            return Some(Value::Int(self.graph.num_proj()));
        }

        // `NumTarget` / `NumTarget(id)` — the count of the owning player's
        // currently-bound throw/hit targets (T061). Resolved here (not on the
        // self-only `Character`) because the bound-target set lives with the entity
        // owner (`fp-engine`), which installs its target id list on the graph each
        // tick (see [`EntityGraph::with_own_target_ids`]). Bare `NumTarget` is the
        // total; `NumTarget(id)` (the VM parses the parenthesized form as a
        // function-call trigger, so `id` arrives as the first argument) counts only
        // targets carrying that id. When the owner wired a list, the count comes
        // from it; otherwise (a self-only / bare / test context with no list) it
        // falls back to the entity's own [`has_target`](Character::has_target)
        // flag, so the single 1-v-1 target is still reported as `0` or `1`. A
        // `NumTarget(id)` with no list wired cannot match a specific id, so it
        // reports `0`. Never panics.
        if name.eq_ignore_ascii_case("NumTarget") {
            let id = args.first().map(|v| v.to_int());
            if self.graph.has_own_target_ids() {
                return Some(Value::Int(self.graph.num_targets(id)));
            }
            return Some(Value::Int(match id {
                None => i32::from(self.me.has_target),
                Some(_) => 0,
            }));
        }

        // Standalone `P2<field>` triggers that read the opponent's OWN self-field.
        // These resolve through the opponent context's `trigger` (so they report
        // the opponent's value and stay correct if its reporting changes); with no
        // opponent they read the safe default `0`. `enemy, <field>` (a redirect)
        // is the more general written form; these single-token aliases are the
        // few P2* fields MUGEN exposes directly.
        if let Some(opp_field) = name
            .strip_prefix("p2")
            .or_else(|| name.strip_prefix("P2"))
            .filter(|f| {
                f.eq_ignore_ascii_case("life")
                    || f.eq_ignore_ascii_case("lifemax")
                    || f.eq_ignore_ascii_case("stateno")
                    || f.eq_ignore_ascii_case("movetype")
                    || f.eq_ignore_ascii_case("statetype")
            })
        {
            return Some(match self.opponent {
                Some(opp) => opp.trigger(opp_field, args),
                None => Value::DEFAULT,
            });
        }

        None
    }
}

impl EvalContext for EvalCtx<'_> {
    fn trigger(&self, name: &str, args: &[Value]) -> Value {
        // Opponent-dependent / stage triggers are computed here; everything else
        // delegates to the wrapped character's self-only impl.
        if let Some(v) = self.cross_entity_trigger(name, args) {
            return v;
        }
        self.me.trigger(name, args)
    }

    fn trigger_str(&self, name: &str, key: &str) -> Value {
        self.me.trigger_str(name, key)
    }

    fn var(&self, index: i32) -> Value {
        self.me.var(index)
    }

    fn fvar(&self, index: i32) -> Value {
        self.me.fvar(index)
    }

    fn sysvar(&self, index: i32) -> Value {
        self.me.sysvar(index)
    }

    fn assign(&self, bank: AssignBank, index: i32, value: Value) -> Value {
        // `:=` writes the *self* character's variable bank (T036): delegate to the
        // wrapped character's interior-mutable overlay. A redirected assignment
        // (`root, var(0) := 1`) is evaluated against the redirect target's own
        // context, so the write still lands on whichever entity the redirect
        // resolved to — never on the wrong character.
        self.me.assign(bank, index, value)
    }

    fn command_active(&self, name: &str) -> bool {
        self.me.command_active(name)
    }

    fn random(&self) -> i32 {
        self.me.random()
    }

    fn hitdef_attr_matches(&self, standtype: &str, attr_codes: &[String]) -> bool {
        // `HitDefAttr` reads the *self* character's active HitDef, so it delegates
        // to the wrapped character's impl (no opponent/stage view needed).
        self.me.hitdef_attr_matches(standtype, attr_codes)
    }

    fn redirect(&self, target: Redirect) -> Option<&dyn EvalContext> {
        match target {
            // The opposing player. In standard 1-v-1 play `p2`, `enemy`, and the
            // nearest `enemynear(_)` all resolve to the single opponent.
            Redirect::Enemy | Redirect::EnemyNear(_) => {
                self.opponent.map(|o| o as &dyn EvalContext)
            }
            // `root` — the player at the top of the spawning chain (T012). A
            // helper's context carries its root in the entity graph; a root
            // player (no graph entry) resolves `root` to itself.
            Redirect::Root => Some(
                self.graph
                    .root()
                    .map_or(self as &dyn EvalContext, |r| r as &dyn EvalContext),
            ),
            // `parent` — the immediate creator (T012). Resolves to the parent
            // entity from the graph, or `None` for a root player (no parent), in
            // which case the redirected sub-expression collapses to `0`.
            Redirect::Parent => self.graph.parent().map(|p| p as &dyn EvalContext),
            // `helper(id)` — a helper owned by this entity, looked up by id in the
            // graph (T012). `None` (→ `0`) when no helper carries that id.
            Redirect::Helper(id) => self.graph.helper(id).map(|h| h as &dyn EvalContext),
            // `target` / `target(id)` — the entity `me` most recently hit (T014).
            // This flat model tracks a single target (the last opponent hit), so
            // both the bare `target` and an explicit `target(id)` resolve to it;
            // `None` (→ `0`) when `me` has no established target. The id form's
            // per-target selection (MUGEN matches `target` whose `HitDef.id == id`)
            // is not distinguished here — a single hit target is the common case
            // the `Target*` throw controllers act on.
            Redirect::Target(_) => self.graph.target().map(|t| t as &dyn EvalContext),
            // `partner` — `me`'s teammate (T014). `None` (→ `0`) in a 1-v-1 with no
            // teammate, resolving gracefully rather than erroring.
            Redirect::Partner => self.graph.partner().map(|p| p as &dyn EvalContext),
            // `playerid(n)` — the entity with global id `n` (T014). `None` (→ `0`)
            // when no entity carries that id.
            Redirect::PlayerId(id) => self.graph.player(id).map(|p| p as &dyn EvalContext),
        }
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
        // Triggers documented as deferred (need get-hit / cross-entity context not
        // yet modeled) must still resolve to the safe default of 0, never panic.
        // This pins the documented behavior so a future implementation is a
        // deliberate change, not an accidental one.
        //
        // `RoundState`/`GameTime`/`MatchOver` are NO LONGER in this list (audit
        // #21): they are now answered from the coordinator-pushed `round_view`.
        // Their *default* (bare-character) value is still 0, pinned in
        // `round_clock_triggers_default_to_zero`; their non-default behavior is
        // pinned in `round_clock_triggers_read_round_view`.
        let ch = sample();
        for t in [
            "HitOver",
            "HitFall",
            "CanRecover",
            "InGuardDist",
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
    fn round_clock_triggers_default_to_zero() {
        // A bare `Character` (no round coordinator) carries the default
        // `RoundView` (RoundState 0 = intro, GameTime 0, match not over,
        // RoundNo 0, RoundsExisted 0), so the round-clock triggers read 0 — the
        // same safe default they had before audit #21 / T016 wired them, and what
        // MUGEN reports pre-round.
        let ch = sample();
        assert_eq!(ch.trigger("RoundState", &[]), Value::Int(0));
        assert_eq!(ch.trigger("GameTime", &[]), Value::Int(0));
        assert_eq!(ch.trigger("MatchOver", &[]), Value::Int(0));
        assert_eq!(ch.trigger("RoundNo", &[]), Value::Int(0));
        assert_eq!(ch.trigger("RoundsExisted", &[]), Value::Int(0));
        // Via the parsed VM path too (case-insensitive trigger names).
        assert_eq!(ev("RoundState", &ch), Value::Int(0));
        assert_eq!(ev("gametime", &ch), Value::Int(0));
        assert_eq!(ev("MatchOver", &ch), Value::Int(0));
        assert_eq!(ev("roundno", &ch), Value::Int(0));
        assert_eq!(ev("RoundsExisted", &ch), Value::Int(0));
    }

    #[test]
    fn round_clock_triggers_read_round_view() {
        // Once the coordinator installs a live `RoundView`, the round-clock
        // triggers report its values (audits #21 / T016). Pins the wiring so a
        // regression that drops them back to a constant 0 is caught.
        let mut ch = sample();
        // Round 3, with two rounds already completed.
        ch.set_round_view(RoundView::new(2, 1234, true, 3, 2));
        assert_eq!(ch.trigger("RoundState", &[]), Value::Int(2));
        assert_eq!(ch.trigger("GameTime", &[]), Value::Int(1234));
        // MatchOver is a boolean trigger → int 1 when set.
        assert_eq!(ch.trigger("MatchOver", &[]), Value::Int(1));
        assert_eq!(ch.trigger("RoundNo", &[]), Value::Int(3));
        assert_eq!(ch.trigger("RoundsExisted", &[]), Value::Int(2));

        // Through the VM eval path, mirroring KFM's `RoundState = 2` gates.
        assert_eq!(ev("RoundState = 2", &ch), Value::Int(1));
        assert_eq!(ev("RoundState = 1", &ch), Value::Int(0));
        assert_eq!(ev("GameTime = 1234", &ch), Value::Int(1));
        assert_eq!(ev("MatchOver", &ch), Value::Int(1));
        assert_eq!(ev("RoundNo = 3", &ch), Value::Int(1));
        assert_eq!(ev("RoundsExisted = 2", &ch), Value::Int(1));
        // `RoundsExisted` is `RoundNo - 1` for a fighter present since round 1.
        assert_eq!(ev("RoundsExisted = RoundNo - 1", &ch), Value::Int(1));

        // The opening round of a fresh match: RoundNo 1, RoundsExisted 0.
        ch.set_round_view(RoundView::new(1, 60, false, 1, 0));
        assert_eq!(ev("RoundState = 1 && !MatchOver", &ch), Value::Int(1));
        assert_eq!(ev("RoundNo = 1 && RoundsExisted = 0", &ch), Value::Int(1));
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
            ..GetHitVars::default()
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
            fall_xvel: -2.25,
            fall_yvel: -6.5,
            fall_damage: 23,
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
        // Fall velocity/damage members (audit #23).
        assert_eq!(g.member("fall.xvel"), Value::Float(-2.25));
        assert_eq!(g.member("fall.yvel"), Value::Float(-6.5));
        assert_eq!(g.member("fall.damage"), Value::Int(23));
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
        assert!(
            ch.active_hitdef.is_none(),
            "no active HitDef on a fresh character"
        );
        assert_eq!(ch.get_hit_vars, GetHitVars::default());
        // Every GetHitVar member reads its no-hit default through the seam.
        assert_eq!(ch.trigger_str("GetHitVar", "damage"), Value::Int(0));
        assert_eq!(ch.trigger_str("GetHitVar", "chainid"), Value::Int(-1));
    }

    #[test]
    fn hitdefattr_trigger_matches_active_hitdef_attribute() {
        // Task A: `HitDefAttr = <standtype>, <attr-list>` reads the character's
        // active HitDef attribute through the `hitdef_attr_matches` seam, end to
        // end via the VM (parse → eval against the Character context).

        // No active HitDef → the trigger never fires (it's only meaningful in-move).
        let ch = Character::new();
        assert_eq!(ev("hitdefattr = C, NA && movecontact", &ch), Value::Int(0));
        assert_eq!(ev("hitdefattr = C, NA", &ch), Value::Int(0));

        // Give the character an active `C, NA` HitDef (the exact evilken form).
        let mut ch = Character::new();
        ch.active_hitdef = Some(fp_combat::HitDef {
            attr: fp_combat::AttackAttr::parse("C, NA"),
            ..Default::default()
        });
        assert_eq!(ev("hitdefattr = C, NA", &ch), Value::Int(1));
        // Case-insensitive standtype + code.
        assert_eq!(ev("hitdefattr = c, na", &ch), Value::Int(1));
        // A multi-code list fires if ANY code matches.
        assert_eq!(ev("hitdefattr = C, SA, NA", &ch), Value::Int(1));
        // A different standtype or code does not match.
        assert_eq!(ev("hitdefattr = S, NA", &ch), Value::Int(0));
        assert_eq!(ev("hitdefattr = C, SA", &ch), Value::Int(0));

        // A `S, SP` (standing special projectile) HitDef matches only S, SP.
        let mut ch = Character::new();
        ch.active_hitdef = Some(fp_combat::HitDef {
            attr: fp_combat::AttackAttr::parse("S, SP"),
            ..Default::default()
        });
        assert_eq!(ev("hitdefattr = S, SP", &ch), Value::Int(1));
        assert_eq!(ev("hitdefattr = S, NA", &ch), Value::Int(0));
    }

    #[test]
    fn character_default_has_no_active_palette_override() {
        // Default: no external .act palette override → the SFF-embedded palette is
        // used and the GPU upload is byte-identical to today.
        let ch = Character::new();
        assert_eq!(ch.active_palette(), None);
        assert_eq!(Character::default().active_palette(), None);
    }

    #[test]
    fn character_set_and_clear_active_palette() {
        let mut ch = Character::new();
        // Selecting a palette index records it on the runtime entity.
        ch.set_active_palette(Some(7));
        assert_eq!(ch.active_palette(), Some(7));
        // Re-selecting overwrites; clearing reverts to the SFF-embedded default.
        ch.set_active_palette(Some(2));
        assert_eq!(ch.active_palette(), Some(2));
        ch.clear_active_palette();
        assert_eq!(ch.active_palette(), None);
        // set_active_palette(None) is equivalent to clearing.
        ch.set_active_palette(Some(3));
        ch.set_active_palette(None);
        assert_eq!(ch.active_palette(), None);
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
            airjuggle: 24,
            size: SizeConstants {
                ground_front: 17,
                ground_back: 19,
                height: 63,
            },
            velocity: VelocityConstants {
                walk_fwd: Vec2::new(2.7, 0.0),
                walk_back: Vec2::new(-2.1, 0.0),
                run_fwd: Vec2::new(4.9, -1.5),
                run_back: Vec2::new(-4.3, -3.2),
                jump_neu: Vec2::new(0.3, -8.1),
                jump_fwd: Vec2::new(2.6, 0.0),
                jump_back: Vec2::new(-2.55, 0.0),
                runjump_fwd: Vec2::new(4.1, -8.2),
                runjump_back: Vec2::new(-2.65, -8.3),
                airjump_neu: Vec2::new(0.1, -8.0),
                airjump_fwd: Vec2::new(2.45, 0.0),
                airjump_back: Vec2::new(-2.35, 0.0),
                jump_up: -8.6,
                airjump_y: -7.9,
            },
            movement: MovementConstants {
                yaccel: 0.5,
                stand_friction: 0.83,
                crouch_friction: 0.81,
                stand_friction_threshold: 3.0,
                crouch_friction_threshold: 0.07,
                down_friction_threshold: 0.09,
                airjump_num: 2,
                airjump_height: 30.0,
            },
            localcoord: (320, 240),
        };
        Character::with_constants(consts)
    }

    #[test]
    fn const_resolves_velocity_members() {
        let ch = const_sample();
        // Float members thread through the float path via direct equality.
        assert_eq!(
            ch.trigger_str("const", "velocity.walk.fwd.x"),
            Value::Float(2.7)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.walk.back.x"),
            Value::Float(-2.1)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.run.fwd.x"),
            Value::Float(4.9)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.run.fwd.y"),
            Value::Float(-1.5)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.jump.neu.x"),
            Value::Float(0.3)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.jump.y"),
            Value::Float(-8.6)
        );
    }

    #[test]
    fn const_resolves_movement_velocity_members() {
        // A.P4: the jump/run/runjump/airjump movement velocities common1 reads
        // via const(velocity.*). const_sample() makes every value distinct so a
        // mis-mapping (aliasing or x/y transposition) is caught.
        let ch = const_sample();
        let float_pairs = [
            ("velocity.run.back.x", -4.3f32),
            ("velocity.run.back.y", -3.2),
            ("velocity.jump.fwd.x", 2.6),
            ("velocity.jump.back.x", -2.55),
            ("velocity.runjump.fwd.x", 4.1),
            ("velocity.runjump.fwd.y", -8.2),
            ("velocity.runjump.back.x", -2.65),
            ("velocity.runjump.back.y", -8.3),
            ("velocity.airjump.neu.x", 0.1),
            ("velocity.airjump.fwd.x", 2.45),
            ("velocity.airjump.back.x", -2.35),
            ("velocity.airjump.y", -7.9),
        ];
        for (m, want) in float_pairs {
            assert_eq!(
                ch.trigger_str("const", m),
                Value::Float(want),
                "member `{m}`"
            );
        }
    }

    #[test]
    fn const_movement_velocity_match_is_case_insensitive() {
        // The full dotted name folds case for the new members exactly like the
        // existing ones (axis suffix included).
        let ch = const_sample();
        assert_eq!(
            ch.trigger_str("const", "Velocity.Jump.Fwd.X"),
            Value::Float(2.6)
        );
        assert_eq!(
            ch.trigger_str("const", "VELOCITY.AIRJUMP.FWD.X"),
            Value::Float(2.45)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.RunJump.Fwd.Y"),
            Value::Float(-8.2)
        );
        assert_eq!(
            ch.trigger_str("const", "Velocity.AirJump.Y"),
            Value::Float(-7.9)
        );
    }

    #[test]
    fn const_movement_velocity_routes_through_parse_and_eval() {
        // End-to-end via fp_vm::parse_str + eval, mirroring how common1 reads
        // these in a `velset` expression.
        let ch = const_sample();
        assert_eq!(ev("const(velocity.jump.fwd.x) = 2.6", &ch), Value::Int(1));
        assert_eq!(
            ev("const(velocity.jump.back.x) = -2.55", &ch),
            Value::Int(1)
        );
        assert_eq!(ev("const(velocity.run.back.x) = -4.3", &ch), Value::Int(1));
        assert_eq!(
            ev("const(velocity.runjump.fwd.x) = 4.1", &ch),
            Value::Int(1)
        );
        assert_eq!(
            ev("const(velocity.airjump.fwd.x) = 2.45", &ch),
            Value::Int(1)
        );
        assert_eq!(ev("const(velocity.airjump.y) = -7.9", &ch), Value::Int(1));
    }

    #[test]
    fn const_default_character_movement_velocities_are_kfm_baseline() {
        // A default Character resolves the KFM-baseline movement velocities the
        // VelocityConstants defaults encode (kfm.cns [Velocity]).
        let ch = Character::new();
        assert_eq!(ev("const(velocity.jump.fwd.x) = 2.5", &ch), Value::Int(1));
        assert_eq!(
            ev("const(velocity.jump.back.x) = -2.55", &ch),
            Value::Int(1)
        );
        assert_eq!(ev("const(velocity.run.back.x) = -4.5", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.runjump.fwd.x) = 4", &ch), Value::Int(1));
        assert_eq!(
            ev("const(velocity.airjump.fwd.x) = 2.5", &ch),
            Value::Int(1)
        );
        assert_eq!(ev("const(velocity.airjump.y) = -8.1", &ch), Value::Int(1));
    }

    #[test]
    fn const_real_kfm_jump_and_airjump_are_nonzero() {
        // Gated real-fixture test: load KFM and assert const(velocity.jump.fwd.x)
        // and an airjump const are nonzero (KFM authored values), proving
        // common1 jumpstart/airjump velset will move horizontally. Skip-if-absent.
        let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let loaded = LoadedCharacter::load(&def).expect("kfm.def should load");
        let ch = Character::with_constants(loaded.constants);
        // Nonzero is the load-bearing property (the bug fixed here); the exact
        // KFM values are asserted in the loader's gated test.
        assert_eq!(ev("const(velocity.jump.fwd.x) != 0", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.airjump.fwd.x) != 0", &ch), Value::Int(1));
        // And they match KFM's authored 2.5 for both.
        assert_eq!(ev("const(velocity.jump.fwd.x) = 2.5", &ch), Value::Int(1));
        assert_eq!(
            ev("const(velocity.airjump.fwd.x) = 2.5", &ch),
            Value::Int(1)
        );
    }

    // ---- T048: bare velocity members alias their `.x` component -----------

    #[test]
    fn const_bare_velocity_members_alias_x_component() {
        // T048 root cause: standard MUGEN content (incl. the shipped
        // trainingdummy [Statedef 20] walk) writes
        // `VelSet x = const(velocity.walk.fwd)` — the BARE member with no axis
        // suffix. Before the fix the bare form matched no arm and fell through
        // to Value::DEFAULT (0), so every character "walked in place". Each
        // bare member must now resolve to the SAME value as its `.x` arm.
        let ch = const_sample();
        for (bare, dotted) in [
            ("velocity.walk.fwd", "velocity.walk.fwd.x"),
            ("velocity.walk.back", "velocity.walk.back.x"),
            ("velocity.run.fwd", "velocity.run.fwd.x"),
            ("velocity.run.back", "velocity.run.back.x"),
            ("velocity.jump.fwd", "velocity.jump.fwd.x"),
            ("velocity.jump.back", "velocity.jump.back.x"),
            ("velocity.jump.neu", "velocity.jump.neu.x"),
            ("velocity.airjump.fwd", "velocity.airjump.fwd.x"),
            ("velocity.airjump.back", "velocity.airjump.back.x"),
            ("velocity.airjump.neu", "velocity.airjump.neu.x"),
        ] {
            let bare_val = ch.trigger_str("const", bare);
            let dotted_val = ch.trigger_str("const", dotted);
            assert_eq!(bare_val, dotted_val, "bare `{bare}` must alias `{dotted}`");
            // The const_sample fixture authors a non-zero x for these, so the
            // alias is meaningfully non-default (proving it is not just 0==0).
            assert_ne!(
                bare_val,
                Value::DEFAULT,
                "bare `{bare}` should resolve to the authored non-zero velocity"
            );
        }
    }

    /// T048 behavioral regression: load the shipped (non-asset-gated) Training
    /// Dummy, drive it with a held-forward `holdfwd` command via the executor,
    /// and assert it (a) enters walk state 20 and (b) its world `pos.x`
    /// strictly increases (right-facing P1). Held-back must decrease `pos.x`.
    ///
    /// This FAILS on pre-fix code: state 20's `VelSet x = const(velocity.walk.fwd)`
    /// resolved to 0, so the dummy entered walk state 20 but never moved
    /// ("walked in place"). Asserts `const(velocity.walk.fwd) == 2.4` for the
    /// shipped dummy, the authored `[Velocity] walk.fwd`.
    #[test]
    fn training_dummy_walks_forward_and_back_via_const_velocity() {
        let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/trainingdummy/trainingdummy.def");
        // The dummy is shippable content (not asset-gated); it must be present.
        assert!(
            def.exists(),
            "shipped Training Dummy missing: {}",
            def.display()
        );
        let loaded = LoadedCharacter::load(&def).expect("trainingdummy.def should load");

        // The shipped dummy authors walk.fwd = 2.4 (trainingdummy.cns [Velocity]).
        // `CharacterConstants` is `Copy`, so each `with_constants` takes a copy.
        let probe = Character::with_constants(loaded.constants);
        assert_eq!(
            ev("const(velocity.walk.fwd) = 2.4", &probe),
            Value::Int(1),
            "shipped Training Dummy const(velocity.walk.fwd) must be 2.4"
        );

        // Drive a fresh entity directly through the executor with `holdfwd`
        // held. State -1 (engine ground locomotion) bridges held-forward into
        // walk state 20; state 20's VelSet then sets x = const(velocity.walk.fwd).
        let walk_player = |cmd: &str| -> (i32, f32, f32) {
            let mut ch = Character::with_constants(loaded.constants);
            ch.pos = Vec2::new(0.0, 0.0);
            ch.facing = Facing::Right; // forward = +x
            ch.state_no = 0;
            ch.ctrl = true;
            ch.anim = 0;
            ch.set_command_source(Box::new(ActiveCommands::from_names([cmd])));
            let start_x = ch.pos.x;
            let mut reached_walk = false;
            for _ in 0..30 {
                ch.tick(&loaded, None, StageView::default());
                if ch.state_no == 20 {
                    reached_walk = true;
                }
            }
            let entered = if reached_walk { 20 } else { ch.state_no };
            (entered, start_x, ch.pos.x)
        };

        // Held FORWARD: enters walk state 20 AND pos.x strictly increases.
        let (fwd_state, fwd_start, fwd_end) = walk_player("holdfwd");
        assert_eq!(
            fwd_state, 20,
            "held forward must enter walk state 20 (got {fwd_state})"
        );
        assert!(
            fwd_end > fwd_start,
            "held forward must increase pos.x (start {fwd_start}, end {fwd_end}) \
             — pre-fix this stayed 0 (\"walked in place\")"
        );

        // Held BACK: pos.x strictly decreases.
        let (_back_state, back_start, back_end) = walk_player("holdback");
        assert!(
            back_end < back_start,
            "held back must decrease pos.x (start {back_start}, end {back_end})"
        );
    }

    /// T052 behavioral regression: load the shipped (non-asset-gated) Training
    /// Dummy, drive it with a held-up `holdup` command via the executor, and
    /// assert it (a) enters a jump state (40 jumpstart and/or 50 air) and (b) its
    /// world `pos.y` leaves the ground (departs from 0 / becomes negative,
    /// Y-up = negative). Also asserts `const(velocity.jump.neu.y) == -8.4`.
    ///
    /// This FAILS on pre-fix code: state 40's `VelSet y = const(velocity.jump.neu.y)`
    /// resolved to 0 (no `.neu.y` resolver arm), so the dummy entered the jump
    /// states but had zero upward velocity and never left the ground ("jumped in
    /// place"). The shipped dummy authors `jump.neu = 0, -8.4` (trainingdummy.cns
    /// [Velocity]), so the neutral-jump y must be -8.4.
    #[test]
    fn training_dummy_jumps_and_leaves_ground_via_const_velocity() {
        let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/trainingdummy/trainingdummy.def");
        // The dummy is shippable content (not asset-gated); it must be present.
        assert!(
            def.exists(),
            "shipped Training Dummy missing: {}",
            def.display()
        );
        let loaded = LoadedCharacter::load(&def).expect("trainingdummy.def should load");

        // The shipped dummy authors jump.neu = 0, -8.4 (trainingdummy.cns
        // [Velocity]); const(velocity.jump.neu.y) must resolve to the authored
        // upward jump speed. Pre-fix this fell through to 0.
        let probe = Character::with_constants(loaded.constants);
        assert_eq!(
            ev("const(velocity.jump.neu.y) = -8.4", &probe),
            Value::Int(1),
            "shipped Training Dummy const(velocity.jump.neu.y) must be -8.4"
        );
        // The forward/back y-components alias the same upward jump speed.
        assert_eq!(
            ev("const(velocity.jump.fwd.y) = -8.4", &probe),
            Value::Int(1),
            "const(velocity.jump.fwd.y) must alias velocity.jump.y (-8.4)"
        );
        assert_eq!(
            ev("const(velocity.jump.back.y) = -8.4", &probe),
            Value::Int(1),
            "const(velocity.jump.back.y) must alias velocity.jump.y (-8.4)"
        );

        // Drive a fresh entity directly through the executor with `holdup` held.
        // The engine built-in `[State -1]` bridges stand(0) + holdup -> 40
        // (jumpstart); state 40's `VelSet y = const(velocity.jump.neu.y)` then
        // sets the upward velocity, and 40 -> 50 (air) on animtime.
        let mut ch = Character::with_constants(loaded.constants);
        ch.pos = Vec2::new(0.0, 0.0);
        ch.facing = Facing::Right;
        ch.state_no = 0;
        ch.ctrl = true;
        ch.anim = 0;
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));

        let start_y = ch.pos.y;
        let mut reached_jump_state = false;
        let mut min_y = start_y; // most-negative (highest) y reached
        for _ in 0..30 {
            ch.tick(&loaded, None, StageView::default());
            if ch.state_no == 40 || ch.state_no == 50 {
                reached_jump_state = true;
            }
            if ch.pos.y < min_y {
                min_y = ch.pos.y;
            }
        }

        assert!(
            reached_jump_state,
            "held up must enter a jump state (40 jumpstart and/or 50 air); \
             stuck in state {}",
            ch.state_no
        );
        assert!(
            min_y < start_y,
            "held up must leave the ground (pos.y departs from {start_y}, \
             min reached {min_y}) — pre-fix this stayed 0 (\"jumped in place\")"
        );
    }

    // ---- A.P4 (Proctor): edge cases, error paths, MUGEN semantics ----------

    #[test]
    fn const_unknown_movement_velocity_members_are_safe_default() {
        // Every plausible typo / unmodeled sub-member of the new groups resolves
        // to the safe default 0 and never panics — the task's explicit "unknown
        // const still returns 0" requirement applied to the new surface.
        let ch = const_sample();
        for m in [
            // `.y` of the bare-x AIR jumps is intentionally NOT a const member
            // (common1 reads the shared velocity.airjump.y for the vertical
            // component, never a per-direction airjump `.y`). NOTE: the GROUND
            // jump's `.neu.y`/`.fwd.y`/`.back.y` ARE valid members (T052: they
            // alias the shared velocity.jump.y), so they are asserted nonzero in
            // the jump-y alias test, not here.
            "velocity.airjump.fwd.y",
            "velocity.airjump.back.y",
            // Bogus axis suffix / nonexistent group. (NOTE: the bare
            // `velocity.airjump.fwd` IS a valid member — it aliases `.x` —
            // so it is asserted nonzero in the bare-form alias test, not here.)
            "velocity.airjump.neu.z",
            "velocity.runjump",
            "velocity.run.sideways.x",
            "velocity.jump.diag.x",
        ] {
            assert_eq!(
                ch.trigger_str("const", m),
                Value::DEFAULT,
                "unknown member `{m}` must default to 0, not panic or alias"
            );
        }
    }

    #[test]
    fn const_jump_fwd_x_and_jump_y_are_distinct_members() {
        // MUGEN semantics: the horizontal directional jump speed (jump.fwd.x) and
        // the vertical jump speed (jump.y -> jump_up) are independent const
        // members; a mis-map that aliased the directional jump's stored `y` would
        // surface here. const_sample(): jump_fwd = (2.6, 0), jump_up = -8.6.
        let ch = const_sample();
        assert_eq!(
            ch.trigger_str("const", "velocity.jump.fwd.x"),
            Value::Float(2.6)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.jump.y"),
            Value::Float(-8.6)
        );
        // T052: the directional jump's `.y` aliases the SHARED upward jump speed
        // (velocity.jump.y -> jump_up), not the directional member's stored `y`
        // (which is 0 for jump_fwd = (2.6, 0)). This proves the horizontal speed
        // (2.6) and the vertical speed (-8.6) stay independent: `.fwd.y` reads the
        // vertical, never leaking the directional horizontal.
        assert_eq!(
            ch.trigger_str("const", "velocity.jump.fwd.y"),
            Value::Float(-8.6)
        );
        // The AIR jump's directional `.y` remains unmapped (defaults to 0).
        assert_eq!(
            ch.trigger_str("const", "velocity.airjump.fwd.y"),
            Value::DEFAULT
        );
    }

    #[test]
    fn const_common1_jumpstart_velset_resolves_horizontal_motion() {
        // Replicate common1.cns State 40 (JumpStart) controllers 4 & 5 verbatim:
        //   x = ifelse(sysvar(1)=0, const(velocity.jump.neu.x),
        //        ifelse(sysvar(1)=1, const(velocity.jump.fwd.x),
        //               const(velocity.jump.back.x)))
        //   y = const(velocity.jump.y)
        //   (running jump) x = const(velocity.runjump.fwd.x)
        // sysvar(1) selects neutral(0)/forward(1)/back(2). Because sysvar reads
        // back 0 in this synthetic context, drive the branch with an explicit
        // const compare per direction instead, then prove each yields the
        // authored nonzero horizontal speed. const_sample(): jump_fwd=(2.6,..),
        // jump_back=(-2.55,..), runjump_fwd=(4.1,..), jump_up=-8.6.
        let ch = const_sample();
        // Forward branch.
        assert_eq!(
            ev(
                "ifelse(0=1, const(velocity.jump.fwd.x), const(velocity.jump.fwd.x)) = 2.6",
                &ch
            ),
            Value::Int(1)
        );
        // The exact nested-ifelse shape common1 uses, forced down each arm.
        assert_eq!(
            ev(
                "ifelse(1=0, const(velocity.jump.neu.x), ifelse(1=1, const(velocity.jump.fwd.x), const(velocity.jump.back.x))) = 2.6",
                &ch
            ),
            Value::Int(1),
            "forward jump arm -> jump.fwd.x"
        );
        assert_eq!(
            ev(
                "ifelse(2=0, const(velocity.jump.neu.x), ifelse(2=1, const(velocity.jump.fwd.x), const(velocity.jump.back.x))) = -2.55",
                &ch
            ),
            Value::Int(1),
            "back jump arm -> jump.back.x"
        );
        // The vertical component and the running-jump horizontal component.
        assert_eq!(ev("const(velocity.jump.y) = -8.6", &ch), Value::Int(1));
        assert_eq!(
            ev("const(velocity.runjump.fwd.x) = 4.1", &ch),
            Value::Int(1)
        );
        // The load-bearing property: the forward/back arms are nonzero, so the
        // VelSet moves horizontally instead of straight up.
        assert_eq!(ev("const(velocity.jump.fwd.x) != 0", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.jump.back.x) != 0", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.runjump.fwd.x) != 0", &ch), Value::Int(1));
    }

    #[test]
    fn const_common1_airjump_velset_resolves_horizontal_motion() {
        // Replicate common1.cns State 45 (AirJump) controller verbatim:
        //   x = ifelse(sysvar(1)=0, const(velocity.airjump.neu.x),
        //        ifelse(sysvar(1)=1, const(velocity.airjump.fwd.x),
        //               const(velocity.airjump.back.x)))
        //   y = const(velocity.airjump.y)
        // const_sample(): airjump_neu=(0.1,..), airjump_fwd=(2.45,..),
        // airjump_back=(-2.35,..), airjump_y=-7.9.
        let ch = const_sample();
        assert_eq!(
            ev(
                "ifelse(1=0, const(velocity.airjump.neu.x), ifelse(1=1, const(velocity.airjump.fwd.x), const(velocity.airjump.back.x))) = 2.45",
                &ch
            ),
            Value::Int(1),
            "forward air-jump arm -> airjump.fwd.x"
        );
        assert_eq!(
            ev(
                "ifelse(2=0, const(velocity.airjump.neu.x), ifelse(2=1, const(velocity.airjump.fwd.x), const(velocity.airjump.back.x))) = -2.35",
                &ch
            ),
            Value::Int(1),
            "back air-jump arm -> airjump.back.x"
        );
        assert_eq!(ev("const(velocity.airjump.y) = -7.9", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.airjump.fwd.x) != 0", &ch), Value::Int(1));
        assert_eq!(
            ev("const(velocity.airjump.back.x) != 0", &ch),
            Value::Int(1)
        );
    }

    #[test]
    fn const_run_back_velset_resolves_both_components() {
        // common1.cns State 105 (run back / hop) reads both axes:
        //   x = const(velocity.run.back.x)   y = const(velocity.run.back.y)
        // const_sample(): run_back = (-4.3, -3.2). Both must be the authored
        // (distinct, nonzero) components.
        let ch = const_sample();
        assert_eq!(ev("const(velocity.run.back.x) = -4.3", &ch), Value::Int(1));
        assert_eq!(ev("const(velocity.run.back.y) = -3.2", &ch), Value::Int(1));
        // x and y are distinct fields, not aliased.
        assert_eq!(
            ev(
                "const(velocity.run.back.x) = const(velocity.run.back.y)",
                &ch
            ),
            Value::Int(0)
        );
    }

    #[test]
    fn const_previously_mapped_velocities_still_resolve() {
        // Regression guard: the walk / run.fwd / jump.neu / jump.y members that
        // existed before A.P4 must keep resolving unchanged alongside the new ones.
        let ch = const_sample();
        assert_eq!(
            ch.trigger_str("const", "velocity.walk.fwd.x"),
            Value::Float(2.7)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.walk.back.x"),
            Value::Float(-2.1)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.run.fwd.x"),
            Value::Float(4.9)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.run.fwd.y"),
            Value::Float(-1.5)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.jump.neu.x"),
            Value::Float(0.3)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.jump.y"),
            Value::Float(-8.6)
        );
    }

    #[test]
    fn const_resolves_size_members() {
        let ch = const_sample();
        assert_eq!(ch.trigger_str("const", "size.ground.front"), Value::Int(17));
        assert_eq!(ch.trigger_str("const", "size.ground.back"), Value::Int(19));
        assert_eq!(ch.trigger_str("const", "size.height"), Value::Int(63));
    }

    /// A.P12: the friction thresholds common1 reads via
    /// `Const(movement.<stance>.friction.threshold)` resolve (were 0 -> idle-stop
    /// triggers like `abs(vel x) < Const(...)` never fired, so a walker never stopped).
    #[test]
    fn const_resolves_friction_thresholds() {
        let ch = const_sample();
        assert_eq!(
            ch.trigger_str("const", "movement.stand.friction.threshold"),
            Value::Float(3.0)
        );
        assert_eq!(
            ch.trigger_str("const", "movement.crouch.friction.threshold"),
            Value::Float(0.07)
        );
        assert_eq!(
            ch.trigger_str("const", "movement.down.friction.threshold"),
            Value::Float(0.09)
        );
        // The stand-friction stop trigger common1 uses now evaluates correctly.
        assert_eq!(
            ev("abs(1.5) < Const(movement.stand.friction.threshold)", &ch),
            Value::Int(1),
            "1.5 < 3.0 threshold -> idle-stop fires"
        );
    }

    #[test]
    fn const_resolves_movement_members() {
        let ch = const_sample();
        assert_eq!(
            ch.trigger_str("const", "movement.yaccel"),
            Value::Float(0.5)
        );
        assert_eq!(
            ch.trigger_str("const", "movement.stand.friction"),
            Value::Float(0.83)
        );
        assert_eq!(
            ch.trigger_str("const", "movement.crouch.friction"),
            Value::Float(0.81)
        );
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
        assert_eq!(
            ch.trigger_str("const", "Velocity.Walk.Fwd.X"),
            Value::Float(2.7)
        );
        assert_eq!(ch.trigger_str("const", "SIZE.GROUND.FRONT"), Value::Int(17));
        assert_eq!(
            ch.trigger_str("const", "Movement.YAccel"),
            Value::Float(0.5)
        );
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
            assert_eq!(
                ch.trigger_str("const", m),
                Value::Float(want),
                "member `{m}`"
            );
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
            "data.life",
            "data.power",
            "data.attack",
            "data.defence",
            "size.ground.front",
            "size.ground.back",
            "size.height",
        ] {
            assert!(
                ch.trigger_str("const", m).is_int(),
                "`{m}` must be int-typed"
            );
        }
        for m in [
            "velocity.walk.fwd.x",
            "velocity.walk.back.x",
            "velocity.run.fwd.x",
            "velocity.run.fwd.y",
            "velocity.jump.neu.x",
            "velocity.jump.y",
            "movement.yaccel",
            "movement.stand.friction",
            "movement.crouch.friction",
        ] {
            assert!(
                ch.trigger_str("const", m).is_float(),
                "`{m}` must be float-typed"
            );
        }
    }

    #[test]
    fn const_run_fwd_x_and_y_are_independent_components() {
        // `velocity.run.fwd` is the only modeled member with BOTH an x and a y
        // component read separately. Confirm they thread to the matching Vec2
        // axis (a bug returning x for both would pass a single-axis test).
        let ch = const_sample(); // run_fwd = (4.9, -1.5)
        assert_eq!(
            ch.trigger_str("const", "velocity.run.fwd.x"),
            Value::Float(4.9)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.run.fwd.y"),
            Value::Float(-1.5)
        );
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
        assert_eq!(
            ch.trigger_str("const", "velocity.jump.neu.x"),
            Value::Float(0.3)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.jump.y"),
            Value::Float(-8.6)
        );
    }

    // ---- AC1: case-insensitive matching on the FULL dotted name --------------

    #[test]
    fn const_member_match_handles_arbitrary_casing_per_segment() {
        // MUGEN folds case across the whole dotted name; segments may be mixed.
        // const_sample() keeps every value unique so a case slip that lands on a
        // different member would surface.
        let ch = const_sample();
        assert_eq!(
            ch.trigger_str("const", "VELOCITY.WALK.FWD.X"),
            Value::Float(2.7)
        );
        assert_eq!(
            ch.trigger_str("const", "velocity.WALK.fwd.X"),
            Value::Float(2.7)
        );
        assert_eq!(ch.trigger_str("const", "Size.Ground.Back"), Value::Int(19));
        assert_eq!(
            ch.trigger_str("const", "Movement.Crouch.Friction"),
            Value::Float(0.81)
        );
        assert_eq!(ch.trigger_str("const", "DATA.DEFENCE"), Value::Int(222));
    }

    #[test]
    fn const_member_leading_trailing_whitespace_is_tolerated() {
        // The resolver trims the member before matching, so a member arriving with
        // incidental surrounding whitespace still resolves rather than defaulting.
        // (Defends the never-panic / be-lenient posture on messy content.)
        let ch = const_sample();
        assert_eq!(
            ch.trigger_str("const", "  velocity.walk.fwd.x  "),
            Value::Float(2.7)
        );
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
            "data",                      // group only
            "velocity",                  // group only
            "velocity.walk",             // partial
            "size.ground",               // partial
            "movement",                  // group only
            "velocity.walk.fwd.x.extra", // trailing junk
            "size.ground.front.x",       // bogus axis on an int member
            "velocity.walk.fwd.z",       // unmodeled z axis
            "data.life.max",             // over-qualified
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
        assert_eq!(
            ch.trigger_str("const", "velocity. walk.fwd.x"),
            Value::DEFAULT
        );
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
        // The GetHitVar branch still defaults for members it does NOT model
        // (`xveladd` is not a GetHitVar field). `fall.yvel` IS now modeled (audit
        // #23), so it resolves to its zero float value on a fresh character —
        // routed by GetHitVar, not bleeding from `const`.
        assert_eq!(ch.trigger_str("GetHitVar", "xveladd"), Value::DEFAULT);
        assert_eq!(ch.trigger_str("GetHitVar", "fall.yvel"), Value::Float(0.0));
    }

    #[test]
    fn trigger_str_unknown_function_name_defaults() {
        // A string-keyed trigger name that is neither `const` nor `GetHitVar`
        // falls through to the safe default rather than mis-routing to either
        // branch.
        let ch = const_sample();
        assert_eq!(
            ch.trigger_str("NotARealStrTrigger", "velocity.walk.fwd.x"),
            Value::DEFAULT
        );
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
        assert_eq!(
            ev("const(movement.stand.friction) = 0.85", &ch),
            Value::Int(1)
        );
        assert_eq!(
            ev("const(movement.crouch.friction) = 0.82", &ch),
            Value::Int(1)
        );
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

    // ---- Const720p / Const1280p coordinate-scaling triggers (Audit P5) -------

    /// Builds a character whose `localcoord` is set to `(w, h)`, leaving all other
    /// constants at their defaults. Used by the coordinate-scaling trigger tests.
    fn char_with_localcoord(w: i32, h: i32) -> Character {
        let consts = CharacterConstants {
            localcoord: (w, h),
            ..CharacterConstants::default()
        };
        Character::with_constants(consts)
    }

    #[test]
    fn const720p_scales_by_width_ratio_through_eval() {
        // Chosen MUGEN formula: Const720p(v) = v * (localcoord.width / 1280).
        // For a 320-wide character the factor is 320/1280 = 0.25 exactly, so
        // Const720p(-8) = -2.0 (NOT 0, NOT the height-based -2.667). Routed through
        // the real fp_vm parse+eval path, the trigger yields a Float.
        let ch = char_with_localcoord(320, 240);
        assert_eq!(ev("Const720p(-8)", &ch), Value::Float(-2.0)); // negative arg, sign preserved
        assert_eq!(ev("Const720p(20)", &ch), Value::Float(5.0));
        assert_eq!(ev("Const720p(56)", &ch), Value::Float(14.0));
        assert_eq!(ev("Const720p(4)", &ch), Value::Float(1.0));
        // The headline behavior fix: `Vel y > Const720p(-8)` is now `> -2.0`, so a
        // small downward velocity below the HD threshold reads false (it used to
        // collapse to `> 0`).
        let mut moving = char_with_localcoord(320, 240);
        moving.vel = Vec2::new(0.0, -1.0); // descending slower than the -2.0 gate
        assert_eq!(ev("Vel y > Const720p(-8)", &moving), Value::Int(1));
        assert_eq!(ev("Vel y > 0", &moving), Value::Int(0));
    }

    #[test]
    fn const1280p_scales_by_2560_reference_width() {
        // Const1280p(v) = v * (localcoord.width / 2560). For 320 wide the factor is
        // 320/2560 = 0.125, so Const1280p(-8) = -1.0 and Const1280p(16) = 2.0.
        let ch = char_with_localcoord(320, 240);
        assert_eq!(ev("Const1280p(-8)", &ch), Value::Float(-1.0)); // negative arg
        assert_eq!(ev("Const1280p(16)", &ch), Value::Float(2.0));
        // The two triggers differ by exactly the reference-width ratio (1280:2560),
        // i.e. Const720p is twice Const1280p for the same arg.
        assert_eq!(ev("Const720p(8) = 2 * Const1280p(8)", &ch), Value::Int(1));
    }

    #[test]
    fn const_coord_scale_at_native_hd_localcoord_is_identity() {
        // A character authored natively at 1280x720 (the 720p space) gets factor
        // 1280/1280 = 1.0 for Const720p: the value passes through unchanged.
        let ch = char_with_localcoord(1280, 720);
        assert_eq!(ev("Const720p(-8)", &ch), Value::Float(-8.0));
        assert_eq!(ev("Const720p(100)", &ch), Value::Float(100.0));
        // The same character at Const1280p downscales by 1280/2560 = 0.5.
        assert_eq!(ev("Const1280p(-8)", &ch), Value::Float(-4.0));
    }

    #[test]
    fn const_coord_scale_default_character_uses_mugen_baseline() {
        // CharacterConstants::default() carries the MUGEN baseline (320, 240), so a
        // bare Character::new() already scales like KFM without any loader step.
        let ch = Character::new();
        assert_eq!(ch.constants.localcoord, (320, 240));
        assert_eq!(ev("Const720p(-8)", &ch), Value::Float(-2.0));
    }

    #[test]
    fn const720p_missing_or_garbage_arg_is_safe_default_no_panic() {
        let ch = char_with_localcoord(320, 240);
        // No argument -> safe default 0 (the parenless / empty-arg degenerate form
        // reaches the trigger with an empty arg slice). Never panics.
        assert_eq!(ch.trigger("Const720p", &[]), Value::DEFAULT);
        assert_eq!(ch.trigger("Const1280p", &[]), Value::DEFAULT);
        // A zero localcoord width would make the scale 0 (degenerate but defined),
        // and is still finite — exercise it so the path is covered.
        let zero = char_with_localcoord(0, 0);
        assert_eq!(
            zero.trigger("Const720p", &[Value::Int(-8)]),
            Value::Float(0.0)
        );
        // Case-insensitive dispatch through the trigger seam.
        assert_eq!(
            ch.trigger("const720P", &[Value::Int(20)]),
            Value::Float(5.0)
        );
        assert_eq!(
            ch.trigger("CONST1280P", &[Value::Int(16)]),
            Value::Float(2.0)
        );
    }

    #[test]
    fn const720p_real_kfm_localcoord_makes_threshold_nonzero() {
        // Gated real-fixture test (skip-if-missing): load KFM, confirm its loaded
        // localcoord threads into the constants, and assert Const720p(-8) is the
        // expected nonzero -2.0 -- so `Vel y > Const720p(-8)` no longer means
        // `> 0`. Routed through fp_vm parse+eval against the real character.
        let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let loaded = LoadedCharacter::load(&def).expect("kfm.def should load");
        // The loader threads [Info] localcoord onto the constants (not just onto
        // LoadedCharacter.localcoord).
        assert_eq!(loaded.constants.localcoord, (320, 240));
        let ch = Character::with_constants(loaded.constants);
        // The fix: a real nonzero HD threshold instead of the old collapsed 0.
        assert_eq!(ev("Const720p(-8)", &ch), Value::Float(-2.0));
        assert!(ev("Const720p(-8) < 0", &ch).as_bool());
        assert_eq!(ev("Const1280p(-8)", &ch), Value::Float(-1.0));
    }

    // ---- Proctor: additional Const720p/Const1280p edge & semantics coverage --

    #[test]
    fn const720p_scale_tracks_arbitrary_localcoord_widths() {
        // The factor is localcoord.width / 1280, independent of height. Pick widths
        // that yield clean ratios so the assertion is exact, and vary the height to
        // prove HEIGHT is irrelevant to the scale (the documented width-only rule).
        // 640 wide -> 0.5; height 240 vs 480 must not change the result.
        let w640_h240 = char_with_localcoord(640, 240);
        let w640_h480 = char_with_localcoord(640, 480);
        assert_eq!(ev("Const720p(8)", &w640_h240), Value::Float(4.0));
        assert_eq!(
            ev("Const720p(8)", &w640_h240),
            ev("Const720p(8)", &w640_h480),
            "height must not affect Const720p scaling"
        );
        // 2560 wide -> 2.0 (a value authored in 720p is doubled into a 2x space).
        let w2560 = char_with_localcoord(2560, 1440);
        assert_eq!(ev("Const720p(8)", &w2560), Value::Float(16.0));
        // 1920 wide -> 1.5.
        let w1920 = char_with_localcoord(1920, 1080);
        assert_eq!(ev("Const720p(100)", &w1920), Value::Float(150.0));
    }

    #[test]
    fn const1280p_native_localcoord_is_identity_and_height_independent() {
        // A character authored natively in the 1280p space (2560 wide) gets factor
        // 2560/2560 = 1.0 for Const1280p: identity. Height varied to confirm it is
        // unused.
        let native = char_with_localcoord(2560, 1440);
        assert_eq!(ev("Const1280p(-80)", &native), Value::Float(-80.0));
        let native_tall = char_with_localcoord(2560, 9999);
        assert_eq!(
            ev("Const1280p(-80)", &native),
            ev("Const1280p(-80)", &native_tall),
        );
    }

    #[test]
    fn const_coord_triggers_preserve_fractional_argument() {
        // The argument is read as a float (v.to_float()), so a fractional authored
        // value must NOT be truncated before scaling. 320/1280 = 0.25; 2.5 -> 0.625.
        let ch = char_with_localcoord(320, 240);
        assert_eq!(ev("Const720p(2.5)", &ch), Value::Float(0.625));
        // Negative fractional arg keeps its sign through the float multiply.
        assert_eq!(ev("Const720p(-2.5)", &ch), Value::Float(-0.625));
        // Const1280p: 320/2560 = 0.125; 2.5 -> 0.3125.
        assert_eq!(ev("Const1280p(2.5)", &ch), Value::Float(0.3125));
    }

    #[test]
    fn const_coord_triggers_always_yield_float_even_for_whole_results() {
        // Even when the scaled result is a whole number, the trigger yields a
        // Value::Float (not Value::Int) — MUGEN's Const720p/Const1280p are
        // float-typed. A whole-number Int would compare equal numerically but the
        // TYPE must be Float so downstream float arithmetic/printing is faithful.
        let ch = char_with_localcoord(1280, 720); // factor 1.0
        let v = ch.trigger("Const720p", &[Value::Int(7)]);
        assert_eq!(v, Value::Float(7.0));
        assert!(
            matches!(v, Value::Float(_)),
            "Const720p must be float-typed, got {v:?}"
        );
    }

    #[test]
    fn const_coord_scale_helper_guards_nonpositive_reference_width() {
        // Unit-cover the private helper's defensive branch directly: a non-positive
        // reference width (never produced internally, but the guard must hold) gives
        // exactly 0.0 rather than NaN/inf/panic from a divide-by-zero.
        let ch = char_with_localcoord(320, 240);
        assert_eq!(ch.const_coord_scale(-8.0, 0.0), 0.0);
        assert_eq!(ch.const_coord_scale(100.0, -1280.0), 0.0);
        // And the normal path still computes value * (width / reference).
        assert_eq!(ch.const_coord_scale(-8.0, 1280.0), -2.0);
        assert!(ch.const_coord_scale(-8.0, 0.0).is_finite());
    }

    #[test]
    fn const720p_resolves_all_common1_landing_thresholds() {
        // The exact Const720p args common1.cns authors for landing / air-anim /
        // sprpriority / p2dist gates (per the task brief): -8, -20, -80, 4, 20, 56.
        // For KFM's (320,240) the factor is 0.25, so each must scale to a*0.25 and
        // none collapse to 0 — the whole point of the fix.
        let ch = char_with_localcoord(320, 240);
        let cases = [
            (-8, -2.0),
            (-20, -5.0),
            (-80, -20.0),
            (4, 1.0),
            (20, 5.0),
            (56, 14.0),
        ];
        for (arg, want) in cases {
            let got = ev(&format!("Const720p({arg})"), &ch);
            assert_eq!(got, Value::Float(want), "Const720p({arg})");
            assert_ne!(
                got,
                Value::Float(0.0),
                "Const720p({arg}) must not collapse to 0"
            );
        }
    }

    #[test]
    fn const_coord_triggers_resolve_through_cross_entity_evalctx() {
        // The executor evaluates triggers through EvalCtx (the cross-entity wrapper),
        // which delegates self-only triggers to the wrapped Character. Const720p/
        // Const1280p must resolve identically via that seam, with or without an
        // opponent present, since they read only `me`'s localcoord.
        let me = char_with_localcoord(320, 240);
        let (_, opp) = two_chars();
        let stage = StageView::default();
        assert_eq!(
            ev_cross("Const720p(-8)", &me, Some(&opp), stage),
            Value::Float(-2.0)
        );
        assert_eq!(
            ev_cross("Const720p(-8)", &me, None, stage),
            Value::Float(-2.0)
        );
        assert_eq!(
            ev_cross("Const1280p(16)", &me, Some(&opp), stage),
            Value::Float(2.0)
        );
        // The headline behavior gate, evaluated through the real executor seam.
        let mut moving = char_with_localcoord(320, 240);
        moving.vel = Vec2::new(0.0, -1.0);
        assert_eq!(
            ev_cross("Vel y > Const720p(-8)", &moving, Some(&opp), stage),
            Value::Int(1)
        );
    }

    #[test]
    fn const720p_real_kfm_landing_gate_no_longer_degenerates_to_zero() {
        // Gated real-fixture (skip-if-missing): on the real KFM character, the
        // landing-style gate `Vel y > Const720p(-8)` must behave as `Vel y > -2.0`,
        // NOT the old `Vel y > 0`. Prove the discriminating case: a character
        // descending at -1.0 (between -2.0 and 0) reads TRUE under the fixed
        // threshold but FALSE under the old collapsed `> 0`.
        let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let loaded = LoadedCharacter::load(&def).expect("kfm.def should load");
        let mut ch = Character::with_constants(loaded.constants);
        ch.vel = Vec2::new(0.0, -1.0);
        assert_eq!(ev("Const720p(-8)", &ch), Value::Float(-2.0));
        assert_eq!(
            ev("Vel y > Const720p(-8)", &ch),
            Value::Int(1),
            "fixed: > -2.0"
        );
        assert_eq!(
            ev("Vel y > 0", &ch),
            Value::Int(0),
            "old collapsed gate would be false"
        );
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
            assert_eq!(
                ev(expr, &a),
                ev(expr, &b),
                "default vs new disagree on {expr}"
            );
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
        // On a fresh character every named member reports 0, routed through the
        // string-keyed seam (not the numeric path). These members are int-typed
        // (or unmodeled), so they read back as the int `Value::DEFAULT` (0).
        // (`fall.yvel`/`fall.xvel` are float-typed and read `Float(0.0)` — covered
        // separately — so they are intentionally not in this int-default list.)
        for member in ["xveladd", "yveladd", "animtype", "fall", "ground.velocity"] {
            assert_eq!(
                ch.trigger_str("GetHitVar", member),
                Value::DEFAULT,
                "GetHitVar({member}) should default to 0"
            );
        }
        // Case-insensitive trigger name (an unmodeled member still defaults).
        assert_eq!(ch.trigger_str("gethitvar", "xveladd"), Value::DEFAULT);
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
            "HitOver",
            "hitover",
            "HitFall",
            "CanRecover",
            "InGuardDist",
            // Round / match state (engine) is no longer deferred:
            // `RoundState`/`GameTime`/`MatchOver` (audit #21) and
            // `RoundNo`/`RoundsExisted` (T016) are all answered from the
            // coordinator-pushed `round_view` (default 0 for a bare character,
            // live values once a `Match` sets the view — see
            // `round_clock_triggers_read_round_view`).
            // Cross-entity geometry (Phase 7 redirection).
            "P2BodyDist",
            "P2Dist",
            "FrontEdgeBodyDist",
            "BackEdgeBodyDist",
            "BackEdgeDist",
            // Animation-table query (executor owns the .air set).
            "SelfAnimExist",
        ];
        for t in deferred {
            let v = ch.trigger(t, &[]);
            assert_eq!(
                v,
                Value::DEFAULT,
                "deferred trigger `{t}` must default to 0"
            );
            // Value::DEFAULT is documented as Value::Int(0); pin that contract so
            // a comparison against literal 0 in stock content still holds.
            assert_eq!(
                v,
                Value::Int(0),
                "deferred trigger `{t}` default must be int 0"
            );
        }
        // With representative args (these are functions in real content, e.g.
        // SelfAnimExist(44), P2BodyDist), the deferred path must still default,
        // not panic.
        assert_eq!(
            ch.trigger("SelfAnimExist", &[Value::Int(44)]),
            Value::Int(0)
        );
        assert_eq!(ch.trigger("P2BodyDist", &[Value::Int(0)]), Value::Int(0));
    }

    #[test]
    fn round_number_triggers_are_no_longer_deferred() {
        // T016: `RoundNo`/`RoundsExisted` used to fall through to the
        // unknown-trigger default 0 (they were in the audited-deferred set above).
        // They now read the coordinator-pushed `RoundView`. A bare character still
        // reads 0 (the safe default), but once a live view is installed they must
        // report it — guard against a regression that re-defers them to constant 0.
        let mut ch = sample();
        assert_eq!(ch.trigger("RoundNo", &[]), Value::Int(0));
        assert_eq!(ch.trigger("RoundsExisted", &[]), Value::Int(0));

        // Mid-match: round 2 with one round already completed.
        ch.set_round_view(RoundView::new(1, 500, false, 2, 1));
        assert_eq!(ch.trigger("RoundNo", &[]), Value::Int(2));
        assert_eq!(ch.trigger("RoundsExisted", &[]), Value::Int(1));
        // Case-insensitive, via the parsed VM path, as stock content spells them.
        assert_eq!(ev("roundno", &ch), Value::Int(2));
        assert_eq!(ev("RoundsExisted", &ch), Value::Int(1));
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
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../test-assets/kfm/common1.cns"
        );
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
        assert!(
            saw_plain,
            "expected an `alive` recovery guard in common1.cns"
        );
    }

    // =====================================================================
    // Cross-entity evaluation context (EvalCtx): P2Dist / P2BodyDist / p2,...
    // redirects / screen-edge distances. These drive REAL trigger expressions
    // through the VM eval path against an EvalCtx, so the redirect/VM seam (not
    // just an internal helper) is exercised — exactly how the executor calls it.
    // =====================================================================

    /// Evaluates a trigger expression string against `me` viewed with `opponent`
    /// (or `None`) and the given stage, through the same VM eval path the
    /// executor uses. Panics only in test code on a parse error.
    fn ev_cross(
        expr: &str,
        me: &Character,
        opponent: Option<&Character>,
        stage: StageView,
    ) -> Value {
        let ast = parse_str(expr).expect("test expression should parse");
        // Build the opponent context one level deep (its own opponent is None),
        // mirroring `Character::tick_with`.
        let opp_ctx = opponent.map(|o| EvalCtx::new(o, None, stage));
        let ctx = EvalCtx::new(me, opp_ctx.as_ref(), stage);
        eval(&ast, &ctx)
    }

    /// Two facing-opposed characters at x=0 (me) and x=60 (opponent), each with a
    /// distinct life/state so opponent reads are unambiguous.
    fn two_chars() -> (Character, Character) {
        let mut me = Character::new();
        me.pos = Vec2::new(0.0, 0.0);
        me.facing = Facing::Right;
        me.life = 700;
        me.state_no = 200;
        // KFM-default widths (ground_front = 16) on both via Character::new().

        let mut opp = Character::new();
        opp.pos = Vec2::new(60.0, 10.0);
        opp.facing = Facing::Left;
        opp.life = 450;
        opp.state_no = 1300;
        opp.move_type = MoveType::Attack;
        opp.state_type = StateType::Air;
        (me, opp)
    }

    #[test]
    fn p2dist_x_is_facing_relative() {
        let (mut me, opp) = two_chars();
        let stage = StageView::default();
        // Facing Right: opponent 60px ahead → P2Dist X == 60 (positive = in front).
        me.facing = Facing::Right;
        assert_eq!(
            ev_cross("P2Dist X", &me, Some(&opp), stage),
            Value::Float(60.0)
        );
        // Facing Left: the same world gap is now BEHIND, so the sign flips.
        me.facing = Facing::Left;
        assert_eq!(
            ev_cross("P2Dist X", &me, Some(&opp), stage),
            Value::Float(-60.0)
        );
    }

    #[test]
    fn p2dist_y_has_no_facing_flip() {
        let (me, opp) = two_chars(); // opp.y = 10, me.y = 0
        let stage = StageView::default();
        assert_eq!(
            ev_cross("P2Dist Y", &me, Some(&opp), stage),
            Value::Float(10.0)
        );
        // Facing does not affect the Y axis.
        let mut me_left = me;
        me_left.facing = Facing::Left;
        assert_eq!(
            ev_cross("P2Dist Y", &me_left, Some(&opp), stage),
            Value::Float(10.0)
        );
    }

    #[test]
    fn p2bodydist_x_subtracts_both_front_widths() {
        let (me, opp) = two_chars();
        let stage = StageView::default();
        // Edge-to-edge: 60 - (me.front 16 + opp.front 16) == 28.
        let widths = (me.constants.size.ground_front + opp.constants.size.ground_front) as f32;
        assert_eq!(
            ev_cross("P2BodyDist X", &me, Some(&opp), stage),
            Value::Float(60.0 - widths)
        );
        // BodyDist Y has no width adjustment; equals P2Dist Y.
        assert_eq!(
            ev_cross("P2BodyDist Y", &me, Some(&opp), stage),
            Value::Float(10.0)
        );
    }

    #[test]
    fn enemy_redirect_reads_opponent_self_fields() {
        let (me, opp) = two_chars();
        let stage = StageView::default();
        // `enemy, life` reads the OPPONENT's life (450), not me's (700). `enemy`
        // is MUGEN's redirect keyword for the opposing player (the parser maps it
        // to `Redirect::Enemy`, which EvalCtx resolves to the opponent context).
        assert_eq!(
            ev_cross("enemy, Life", &me, Some(&opp), stage),
            Value::Int(450)
        );
        assert_eq!(ev_cross("Life", &me, Some(&opp), stage), Value::Int(700));
        // `enemy, stateno` reads the opponent's state number.
        assert_eq!(
            ev_cross("enemy, StateNo = 1300", &me, Some(&opp), stage),
            Value::Int(1)
        );
        // Letter-coded opponent reads route through the opponent's own trigger.
        assert_eq!(
            ev_cross("enemy, MoveType = A", &me, Some(&opp), stage),
            Value::Int(1)
        );
        assert_eq!(
            ev_cross("enemy, StateType = A", &me, Some(&opp), stage),
            Value::Int(1)
        );
        // `enemynear(0), ...` resolves to the same single opponent.
        assert_eq!(
            ev_cross("enemynear(0), Life", &me, Some(&opp), stage),
            Value::Int(450)
        );
    }

    #[test]
    fn standalone_p2_field_triggers_read_opponent() {
        let (me, opp) = two_chars();
        let stage = StageView::default();
        // The single-token `P2<field>` aliases read the opponent's own fields.
        assert_eq!(
            ev_cross("P2Life = 450", &me, Some(&opp), stage),
            Value::Int(1)
        );
        assert_eq!(
            ev_cross("P2LifeMax = 1000", &me, Some(&opp), stage),
            Value::Int(1)
        );
        assert_eq!(
            ev_cross("P2StateNo = 1300", &me, Some(&opp), stage),
            Value::Int(1)
        );
        assert_eq!(
            ev_cross("P2MoveType = A", &me, Some(&opp), stage),
            Value::Int(1)
        );
        assert_eq!(
            ev_cross("P2StateType = A", &me, Some(&opp), stage),
            Value::Int(1)
        );
        // With no opponent each reads the safe default 0.
        assert_eq!(ev_cross("P2Life", &me, None, stage), Value::Int(0));
        assert_eq!(ev_cross("P2StateNo", &me, None, stage), Value::Int(0));
    }

    #[test]
    fn root_redirect_is_self() {
        let (me, opp) = two_chars();
        let stage = StageView::default();
        // A non-helper's `root` is itself: `root, life` == own life.
        assert_eq!(
            ev_cross("root, Life", &me, Some(&opp), stage),
            Value::Int(700)
        );
        assert_eq!(
            ev_cross("root, StateNo = 200", &me, Some(&opp), stage),
            Value::Int(1)
        );
    }

    #[test]
    fn no_opponent_cross_entity_reads_are_zero_and_never_panic() {
        let (me, _opp) = two_chars();
        let stage = StageView::default();
        // With no opponent every opponent-dependent read is the safe default 0.
        assert_eq!(ev_cross("P2Dist X", &me, None, stage), Value::Float(0.0));
        assert_eq!(ev_cross("P2Dist Y", &me, None, stage), Value::Float(0.0));
        assert_eq!(
            ev_cross("P2BodyDist X", &me, None, stage),
            Value::Float(0.0)
        );
        // An `enemy, ...` redirect resolves to None → the whole sub-expr is 0.
        assert_eq!(ev_cross("enemy, Life", &me, None, stage), Value::Int(0));
        assert_eq!(
            ev_cross("enemy, StateNo = 1300", &me, None, stage),
            Value::Int(0)
        );
        // A compound gated on a cross-entity read collapses to false, not a panic.
        // (A redirect binds looser than every operator, so `enemy, EXPR` retargets
        // the whole trailing compound; with no opponent it is 0.)
        assert_eq!(
            ev_cross("enemy, MoveType = A && Life > 0", &me, None, stage),
            Value::Int(0)
        );
        assert_eq!(
            ev_cross("P2BodyDist X < 30 && P2Life > 0", &me, None, stage),
            Value::Int(0)
        );
    }

    #[test]
    fn unsupported_redirects_are_none_through_evalctx() {
        let (me, opp) = two_chars();
        let stage = StageView::default();
        // With NO entity graph installed (a bare `EvalCtx::new`, the default
        // empty `EntityGraph`), the graph-resolved redirects — parent / helper(id)
        // (T012) and target / partner / playerid(n) (T014) — have nothing to point
        // at, so the redirected sub-expression collapses to 0 (never panics). The
        // T012/T014 tests below exercise the populated-graph case.
        for expr in [
            "parent, Life",
            "partner, Life",
            "helper(1), Life",
            "target, Life",
            "playerid(7), Life",
        ] {
            assert_eq!(
                ev_cross(expr, &me, Some(&opp), stage),
                Value::Int(0),
                "`{expr}` should resolve to 0 with no entity graph installed"
            );
        }
    }

    // =====================================================================
    // T012 — Helper entity graph: parent / root / helper(id) redirects resolve
    // through EvalCtx against an installed EntityGraph (the spawning chain),
    // exercised end-to-end through the same VM eval path the executor uses.
    // =====================================================================

    /// Evaluates a trigger expression against `me` with an [`EntityGraph`]
    /// installed — i.e. `me` is a *helper* whose `parent`/`root`/`helper(id)`
    /// redirects resolve to the supplied chain. Panics only in test code on a
    /// parse error. Drives the real VM eval path, so the redirect/VM seam is
    /// exercised exactly as the executor calls it.
    fn ev_graph(expr: &str, me: &Character, graph: EntityGraph<'_>) -> Value {
        let ast = parse_str(expr).expect("test expression should parse");
        let ctx = EvalCtx::new(me, None, StageView::default()).with_graph(graph);
        eval(&ast, &ctx)
    }

    /// AC: `parent` / `root` redirects from a helper resolve to the spawning
    /// chain (not 0). With a graph installed, `parent, life` reads the parent's
    /// life and `root, stateno` reads the root's state — distinct from the
    /// helper's own values.
    #[test]
    fn helper_parent_and_root_redirects_resolve_to_chain() {
        // The spawning chain: root → parent → me(helper). Give each a distinct
        // life and state so the reads are unambiguous.
        let mut root = Character::new();
        root.life = 999;
        root.state_no = 1500;

        let mut parent = Character::new();
        parent.life = 432;
        parent.state_no = 640;

        let mut helper = Character::new();
        helper.life = 50;
        helper.state_no = 1000;

        let graph = EntityGraph::new(Some(&parent), Some(&root), &[]);

        // `parent, life` → the parent's life (432), NOT the helper's 50 or 0.
        assert_eq!(ev_graph("parent, Life", &helper, graph), Value::Int(432));
        // `root, stateno` → the root's state (1500).
        assert_eq!(ev_graph("root, StateNo", &helper, graph), Value::Int(1500));
        // A bare self read still reports the helper's own value.
        assert_eq!(ev_graph("Life", &helper, graph), Value::Int(50));
    }

    /// AC: `helper(id)` resolves to the matching helper, and a non-matching id is
    /// the safe default `0`.
    #[test]
    fn helper_id_redirect_resolves_matching_entity() {
        // A root that owns two helpers with distinct ids/lives.
        let root = Character::new();

        let mut h1 = Character::new();
        h1.life = 111;
        let mut h2 = Character::new();
        h2.life = 222;

        let helpers: [(i32, &Character); 2] = [(1, &h1), (2, &h2)];
        let graph = EntityGraph::new(None, None, &helpers);

        // `helper(1), life` → 111; `helper(2), life` → 222.
        assert_eq!(ev_graph("helper(1), Life", &root, graph), Value::Int(111));
        assert_eq!(ev_graph("helper(2), Life", &root, graph), Value::Int(222));
        // An id no helper carries → the redirect is None → the sub-expr is 0.
        assert_eq!(ev_graph("helper(3), Life", &root, graph), Value::Int(0));
    }

    /// A root player with NO graph still resolves `root` to itself and leaves
    /// `parent` / `helper(id)` as the safe default `0` — the pre-T012 behavior is
    /// preserved for the no-graph case.
    #[test]
    fn root_player_without_graph_resolves_root_to_self() {
        let mut me = Character::new();
        me.life = 700;
        me.state_no = 200;
        let graph = EntityGraph::default();
        // `root, life` == own life (a root player's root is itself).
        assert_eq!(ev_graph("root, Life", &me, graph), Value::Int(700));
        // No parent / helpers: both collapse to 0.
        assert_eq!(ev_graph("parent, Life", &me, graph), Value::Int(0));
        assert_eq!(ev_graph("helper(1), Life", &me, graph), Value::Int(0));
    }

    /// The [`EntityGraph`] accessors return exactly the wired references (and the
    /// helper lookup returns the first id match / `None`).
    #[test]
    fn entity_graph_accessors_round_trip() {
        let parent = Character::new();
        let root = Character::new();
        let h = Character::new();
        let helpers: [(i32, &Character); 1] = [(7, &h)];
        let graph = EntityGraph::new(Some(&parent), Some(&root), &helpers);
        assert!(graph.parent().is_some());
        assert!(graph.root().is_some());
        assert!(graph.helper(7).is_some(), "matching id resolves");
        assert!(graph.helper(8).is_none(), "missing id is None");

        // The default (root-player) graph has no relations.
        let empty = EntityGraph::default();
        assert!(empty.parent().is_none());
        assert!(empty.root().is_none());
        assert!(empty.helper(0).is_none());
        // T014 relations are also empty on the default graph.
        assert!(empty.target().is_none());
        assert!(empty.partner().is_none());
        assert!(empty.player(0).is_none());
        // NUMHELPER: no own-helper list wired → every count is 0, never panics.
        assert_eq!(empty.num_helpers(None), 0);
        assert_eq!(empty.num_helpers(Some(3)), 0);
    }

    /// NUMHELPER (AC: `fp-character` unit test): with the owning player's
    /// live-helper id list installed, [`EntityGraph::num_helpers`] returns the
    /// total (`None`) or the per-id count (`Some(id)`), and the `NumHelper` /
    /// `NumHelper(id)` triggers resolve to the same counts end-to-end through the
    /// VM eval path the executor uses.
    #[test]
    fn num_helper_counts_owning_players_live_helpers() {
        let me = Character::new();
        // Owning player has 3 live helpers: two of id 3, one of id 7 — the kind of
        // multi-helper-per-id slot-map evilken's spawn guards latch against.
        let own_helper_ids: [i32; 3] = [3, 7, 3];
        let graph = EntityGraph::default().with_own_helper_ids(&own_helper_ids);

        // Direct accessor: total is 3; id 3 appears twice; id 7 once; unknown 0.
        assert_eq!(graph.num_helpers(None), 3, "total live helpers");
        assert_eq!(graph.num_helpers(Some(3)), 2, "two helpers carry id 3");
        assert_eq!(graph.num_helpers(Some(7)), 1, "one helper carries id 7");
        assert_eq!(graph.num_helpers(Some(99)), 0, "no helper carries id 99");

        // End-to-end through the VM: bare `NumHelper` is the total; `NumHelper(id)`
        // counts that id. This is the path the `[State -2]` spawn guard evaluates.
        assert_eq!(ev_graph("NumHelper", &me, graph), Value::Int(3));
        assert_eq!(ev_graph("NumHelper(3)", &me, graph), Value::Int(2));
        assert_eq!(ev_graph("NumHelper(7)", &me, graph), Value::Int(1));
        assert_eq!(ev_graph("NumHelper(99)", &me, graph), Value::Int(0));

        // The spawn-once guard latches: once id 3 exists, `NumHelper(3) = 0` reads
        // false, so a `triggerall`-guarded `Helper` controller no longer fires.
        assert_eq!(ev_graph("NumHelper(3) = 0", &me, graph), Value::Int(0));
    }

    /// NUMHELPER: a context with no own-helper list (a root with no helpers, or any
    /// bare context) reports `0` for both forms — the spawn-once guard's open state
    /// (`NumHelper(N) = 0` is true), so the FIRST spawn is allowed.
    #[test]
    fn num_helper_is_zero_with_no_helpers() {
        let me = Character::new();
        let graph = EntityGraph::default();
        assert_eq!(ev_graph("NumHelper", &me, graph), Value::Int(0));
        assert_eq!(ev_graph("NumHelper(3)", &me, graph), Value::Int(0));
        // The open guard reads true while there are no helpers of that id.
        assert_eq!(ev_graph("NumHelper(3) = 0", &me, graph), Value::Int(1));
    }

    // =====================================================================
    // T026 — projectile contact/hit/guard triggers (Proj*<id> / NumProj) read
    // the owner's per-id tracker / live-projectile list through the same VM eval
    // path the executor uses.
    // =====================================================================

    /// T026 (AC1/AC3): the `Proj*<id>` / `Proj*Time<id>` triggers report the
    /// owner's per-id tracker across a projectile lifecycle —
    /// before any contact, on the connecting tick, and as the time-since ages.
    #[test]
    fn proj_triggers_track_contact_hit_guard_over_lifecycle() {
        let mut me = Character::new();

        // Before any contact: every trigger reads "no event".
        assert_eq!(ev("ProjContact2000", &me), Value::Int(0));
        assert_eq!(ev("ProjHit2000", &me), Value::Int(0));
        assert_eq!(ev("ProjGuarded2000", &me), Value::Int(0));
        assert_eq!(
            ev("ProjContactTime2000", &me),
            Value::Int(ProjContactTracker::NEVER)
        );
        assert_eq!(
            ev("ProjHitTime2000", &me),
            Value::Int(ProjContactTracker::NEVER)
        );

        // A projectile id 2000 lands a CLEAN HIT this tick.
        me.record_proj_event(2000, false);
        // On the connecting tick: contact and hit fire (time 0), guard does not.
        assert_eq!(ev("ProjContact2000", &me), Value::Int(1));
        assert_eq!(ev("ProjHit2000", &me), Value::Int(1));
        assert_eq!(ev("ProjGuarded2000", &me), Value::Int(0));
        assert_eq!(ev("ProjContactTime2000", &me), Value::Int(0));
        assert_eq!(ev("ProjHitTime2000", &me), Value::Int(0));
        assert_eq!(
            ev("ProjGuardedTime2000", &me),
            Value::Int(ProjContactTracker::NEVER)
        );

        // The window form fires while the contact is recent.
        assert_eq!(ev("ProjContact2000 = 1, <= 4", &me), Value::Int(1));

        // Age two ticks: the boolean form stops firing (time != 0) but the time
        // counts up and the window form still matches.
        me.tick_proj_events();
        me.tick_proj_events();
        assert_eq!(ev("ProjContact2000", &me), Value::Int(0));
        assert_eq!(ev("ProjContactTime2000", &me), Value::Int(2));
        assert_eq!(ev("ProjHitTime2000", &me), Value::Int(2));
        assert_eq!(ev("ProjContact2000 = 1, <= 4", &me), Value::Int(1));
        assert_eq!(ev("ProjContact2000 = 1, < 2", &me), Value::Int(0));

        // A different id is independent: untouched id 2001 still reads "never".
        assert_eq!(
            ev("ProjContactTime2001", &me),
            Value::Int(ProjContactTracker::NEVER)
        );
    }

    /// T026: a GUARDED projectile sets the guard counter (not the hit counter);
    /// contact fires for both hit and guard.
    #[test]
    fn proj_guarded_event_sets_guard_not_hit() {
        let mut me = Character::new();
        me.record_proj_event(42, true); // guarded
        assert_eq!(ev("ProjContact42", &me), Value::Int(1));
        assert_eq!(ev("ProjGuarded42", &me), Value::Int(1));
        assert_eq!(ev("ProjHit42", &me), Value::Int(0));
        assert_eq!(ev("ProjGuardedTime42", &me), Value::Int(0));
        assert_eq!(
            ev("ProjHitTime42", &me),
            Value::Int(ProjContactTracker::NEVER)
        );
    }

    /// T026 (AC2): `NumProj` reports the owner's live-projectile count, surfaced
    /// through the [`EntityGraph`] exactly like `NumHelper`.
    #[test]
    fn num_proj_counts_owning_players_live_projectiles() {
        let me = Character::new();

        // No live projectiles: bare context reads 0.
        assert_eq!(
            ev_graph("NumProj", &me, EntityGraph::default()),
            Value::Int(0)
        );

        // Owner has three live projectiles (ids are immaterial to the bare count).
        let own_proj_ids: [i32; 3] = [2000, 2000, 2001];
        let graph = EntityGraph::default().with_own_proj_ids(&own_proj_ids);
        assert_eq!(graph.num_proj(), 3, "direct accessor: 3 live projectiles");
        assert_eq!(ev_graph("NumProj", &me, graph), Value::Int(3));
        // The spawn-once guard: `NumProj = 0` is false while projectiles are live.
        assert_eq!(ev_graph("NumProj = 0", &me, graph), Value::Int(0));
    }

    /// T026: the per-id tracker is bounded — a flood of distinct ids cannot grow
    /// the map past [`Character::MAX_PROJ_IDS`], and an already-tracked id keeps
    /// updating even once the cap is reached.
    #[test]
    fn proj_events_map_is_bounded() {
        let mut me = Character::new();
        // Record one event for an id we will keep checking.
        me.record_proj_event(0, false);
        // Flood far past the cap with distinct ids.
        for id in 1..(Character::MAX_PROJ_IDS as i32 + 100) {
            me.record_proj_event(id, false);
        }
        assert!(
            me.proj_events.len() <= Character::MAX_PROJ_IDS,
            "the tracker map never exceeds the cap"
        );
        // The already-tracked id 0 still updates (a guard event after the flood).
        me.record_proj_event(0, true);
        assert_eq!(ev("ProjGuarded0", &me), Value::Int(1));
    }

    // =====================================================================
    // T014 — target / partner / playerid(n) redirects resolve through EvalCtx
    // against an installed EntityGraph, exercised end-to-end through the same VM
    // eval path the executor uses.
    // =====================================================================

    /// AC: `target` resolves to the entity `me` most recently hit — with the
    /// `target` relation installed, `target, life` reads the target's life (not
    /// `me`'s, not 0), and a graph with no target leaves it at the safe default 0.
    #[test]
    fn target_redirect_resolves_to_hit_entity() {
        let mut me = Character::new();
        me.life = 800;

        let mut victim = Character::new();
        victim.life = 123;
        victim.state_no = 5050;

        // With the target wired, `target, …` hops to the victim.
        let graph = EntityGraph::default().with_target(Some(&victim));
        assert_eq!(ev_graph("target, Life", &me, graph), Value::Int(123));
        assert_eq!(ev_graph("target, StateNo", &me, graph), Value::Int(5050));
        // `target(id)` (the optional-id form) resolves to the same single target.
        assert_eq!(ev_graph("target(0), Life", &me, graph), Value::Int(123));
        // A bare self read still reports `me`'s own life.
        assert_eq!(ev_graph("Life", &me, graph), Value::Int(800));

        // With NO target wired the redirect collapses to 0 (graceful default).
        let no_target = EntityGraph::default();
        assert_eq!(ev_graph("target, Life", &me, no_target), Value::Int(0));
    }

    /// AC: `partner` resolves to a teammate when present, and gracefully to 0 when
    /// there is no teammate (the 1-v-1 case).
    #[test]
    fn partner_redirect_resolves_to_teammate_or_zero() {
        let me = Character::new();

        let mut mate = Character::new();
        mate.life = 654;

        // Teammate present → `partner, life` reads the mate's life.
        let graph = EntityGraph::default().with_partner(Some(&mate));
        assert_eq!(ev_graph("partner, Life", &me, graph), Value::Int(654));

        // No teammate (1-v-1) → `partner, …` is the safe default 0, not a panic.
        let solo = EntityGraph::default();
        assert_eq!(ev_graph("partner, Life", &me, solo), Value::Int(0));
    }

    /// AC: `playerid(n)` resolves to the entity carrying global id `n` in the
    /// populated lookup table, and a non-matching id is the safe default 0.
    #[test]
    fn playerid_redirect_resolves_by_id() {
        let me = Character::new();

        let mut p2 = Character::new();
        p2.life = 222;
        let mut p7 = Character::new();
        p7.life = 777;

        let players: [(i32, &Character); 2] = [(2, &p2), (7, &p7)];
        let graph = EntityGraph::default().with_players(&players);

        // Each id resolves to its entity.
        assert_eq!(ev_graph("playerid(2), Life", &me, graph), Value::Int(222));
        assert_eq!(ev_graph("playerid(7), Life", &me, graph), Value::Int(777));
        // An id no entity carries → None → the sub-expression is 0.
        assert_eq!(ev_graph("playerid(3), Life", &me, graph), Value::Int(0));
    }

    /// The T014 [`EntityGraph`] accessors return exactly the wired references, and
    /// the `playerid` lookup returns the first id match / `None`. Confirms the
    /// T012 relations remain intact when the T014 builders layer on top.
    #[test]
    fn entity_graph_t014_accessors_round_trip() {
        let parent = Character::new();
        let root = Character::new();
        let helper = Character::new();
        let target = Character::new();
        let partner = Character::new();
        let pa = Character::new();
        let pb = Character::new();
        let helpers: [(i32, &Character); 1] = [(1, &helper)];
        let players: [(i32, &Character); 2] = [(2, &pa), (5, &pb)];

        let graph = EntityGraph::new(Some(&parent), Some(&root), &helpers)
            .with_target(Some(&target))
            .with_partner(Some(&partner))
            .with_players(&players);

        // T012 relations still resolve after the T014 builders run.
        assert!(graph.parent().is_some());
        assert!(graph.root().is_some());
        assert!(graph.helper(1).is_some());
        // T014 relations resolve.
        assert!(graph.target().is_some());
        assert!(graph.partner().is_some());
        assert!(graph.player(2).is_some(), "matching playerid resolves");
        assert!(graph.player(5).is_some());
        assert!(graph.player(99).is_none(), "missing playerid is None");
    }

    #[test]
    fn screen_edge_distances_use_stage_and_facing() {
        let mut me = Character::new();
        me.pos = Vec2::new(50.0, 0.0);
        let stage = StageView::new(-200.0, 200.0);
        // Facing Right: front edge is the right edge → 200 - 50 = 150;
        // back edge is the left edge → 50 - (-200) = 250.
        me.facing = Facing::Right;
        assert_eq!(
            ev_cross("FrontEdgeDist", &me, None, stage),
            Value::Float(150.0)
        );
        assert_eq!(
            ev_cross("BackEdgeDist", &me, None, stage),
            Value::Float(250.0)
        );
        // Facing Left swaps which edge is front vs back.
        me.facing = Facing::Left;
        assert_eq!(
            ev_cross("FrontEdgeDist", &me, None, stage),
            Value::Float(250.0)
        );
        assert_eq!(
            ev_cross("BackEdgeDist", &me, None, stage),
            Value::Float(150.0)
        );
        // ScreenPos X is the offset from the left edge.
        assert_eq!(
            ev_cross("ScreenPos X", &me, None, stage),
            Value::Float(250.0)
        );
        // Body-edge variants subtract THIS player's facing-appropriate half-width:
        // FrontEdgeBodyDist uses `front`, BackEdgeBodyDist uses `back` (the asymmetry
        // a regression could silently swap). Facing Left: front edge dist = 250,
        // back edge dist = 150.
        let gf = me.constants.size.ground_front as f32;
        let gb = me.constants.size.ground_back as f32;
        assert_eq!(
            ev_cross("FrontEdgeBodyDist", &me, None, stage),
            Value::Float(250.0 - gf)
        );
        assert_eq!(
            ev_cross("BackEdgeBodyDist", &me, None, stage),
            Value::Float(150.0 - gb)
        );
        // ScreenPos Y is raw world Y (no vertical camera modeled).
        assert_eq!(
            ev_cross("ScreenPos Y", &me, None, stage),
            Value::Float(me.pos.y)
        );
    }

    /// T060: `GameWidth`/`GameHeight` report the stage's localcoord dimensions
    /// and `LeftEdge`/`RightEdge`/`TopEdge`/`BottomEdge` report the
    /// camera-relative world coords of the visible screen edges, with the
    /// invariant `RightEdge - LeftEdge == GameWidth` (and
    /// `BottomEdge - TopEdge == GameHeight`).
    #[test]
    fn edge_and_game_dim_triggers() {
        let me = Character::new();

        // A stage whose body-clamp bounds [-300, 300] are WIDER than its logical
        // screen (GameWidth = 320, GameHeight = 240), centred on the origin:
        // visible edges = origin ± GameWidth/2 = ±160, independent of the wider
        // playfield bounds. Position/facing must not affect these (screen edges,
        // not body distances).
        let stage = StageView::with_dims(-300.0, 300.0, 320.0, 240.0);

        // Game dimensions are the localcoord constants verbatim.
        assert_eq!(ev_cross("GameWidth", &me, None, stage), Value::Float(320.0));
        assert_eq!(
            ev_cross("GameHeight", &me, None, stage),
            Value::Float(240.0)
        );

        // Visible screen edges: centred on (left+right)/2 = 0, spanning GameWidth.
        assert_eq!(ev_cross("LeftEdge", &me, None, stage), Value::Float(-160.0));
        assert_eq!(ev_cross("RightEdge", &me, None, stage), Value::Float(160.0));
        // World Y is down with the floor at Y=0, so the top edge is GameHeight
        // above the floor and the bottom edge is the floor.
        assert_eq!(ev_cross("TopEdge", &me, None, stage), Value::Float(-240.0));
        assert_eq!(ev_cross("BottomEdge", &me, None, stage), Value::Float(0.0));

        // The pinned invariants the AC names.
        let left = ev_cross("LeftEdge", &me, None, stage).to_float();
        let right = ev_cross("RightEdge", &me, None, stage).to_float();
        let gw = ev_cross("GameWidth", &me, None, stage).to_float();
        assert_eq!(right - left, gw, "RightEdge - LeftEdge == GameWidth");
        let top = ev_cross("TopEdge", &me, None, stage).to_float();
        let bottom = ev_cross("BottomEdge", &me, None, stage).to_float();
        let gh = ev_cross("GameHeight", &me, None, stage).to_float();
        assert_eq!(bottom - top, gh, "BottomEdge - TopEdge == GameHeight");

        // An off-centre camera window: bounds centred at x=100 shift both X edges
        // by +100 but keep the GameWidth span (consistent, single coord space).
        let off = StageView::with_dims(0.0, 200.0, 320.0, 240.0);
        assert_eq!(
            ev_cross("LeftEdge", &me, None, off),
            Value::Float(100.0 - 160.0)
        );
        assert_eq!(
            ev_cross("RightEdge", &me, None, off),
            Value::Float(100.0 + 160.0)
        );
        assert_eq!(
            ev_cross("RightEdge", &me, None, off).to_float()
                - ev_cross("LeftEdge", &me, None, off).to_float(),
            320.0
        );

        // The classic `StageView::new` / default carry the 320×240 localcoord.
        let dflt = StageView::default();
        assert_eq!(ev_cross("GameWidth", &me, None, dflt), Value::Float(320.0));
        assert_eq!(ev_cross("GameHeight", &me, None, dflt), Value::Float(240.0));
    }

    #[test]
    fn evalctx_delegates_self_only_triggers_to_character() {
        // The wrapper must not perturb ordinary self-only reads: they pass straight
        // through to the wrapped Character's impl, with or without an opponent.
        let (mut me, opp) = two_chars();
        me.vars[1] = 5;
        me.fvars[0] = 1.5;
        let stage = StageView::default();
        for (expr, expected) in [
            ("StateNo = 200", Value::Int(1)),
            ("Life = 700", Value::Int(1)),
            ("var(1) = 5", Value::Int(1)),
            ("Facing = 1", Value::Int(1)),
        ] {
            assert_eq!(ev_cross(expr, &me, Some(&opp), stage), expected, "{expr}");
            assert_eq!(
                ev_cross(expr, &me, None, stage),
                expected,
                "{expr} (no opp)"
            );
        }
        // fvar reads through the typed seam too.
        assert_eq!(
            ev_cross("fvar(0) = 1.5", &me, Some(&opp), stage),
            Value::Int(1)
        );
    }

    // =====================================================================
    // SelfAnimExist(n) against the loaded AIR action set (audit P22).
    // These drive REAL trigger expressions through the VM eval path against an
    // `EvalCtx` carrying an `AnimSet`, exactly how the executor calls it.
    // =====================================================================

    /// Builds a synthetic `action number → action` map from a list of action
    /// numbers (frames/loopstart are irrelevant to `SelfAnimExist`).
    fn anim_actions(nums: &[i32]) -> HashMap<i32, AnimAction> {
        nums.iter()
            .map(|&n| {
                (
                    n,
                    AnimAction {
                        action_number: n,
                        frames: Vec::new(),
                        loopstart: 0,
                    },
                )
            })
            .collect()
    }

    /// Evaluates `expr` against `me` viewed with the given animation action set
    /// (and no opponent / default stage), through the same VM eval path the
    /// executor uses. Panics only in test code on a parse error.
    fn ev_with_anim(expr: &str, me: &Character, actions: &HashMap<i32, AnimAction>) -> Value {
        let ast = parse_str(expr).expect("test expression should parse");
        let ctx = EvalCtx::with_anim(me, None, StageView::default(), AnimSet::new(actions));
        eval(&ast, &ctx)
    }

    #[test]
    fn selfanimexist_resolves_against_loaded_air_actions() {
        // An AIR table with exactly actions {0, 41, 44}.
        let actions = anim_actions(&[0, 41, 44]);
        let ch = Character::new();

        // Present actions → 1; absent → 0; through the real trigger/eval path.
        assert_eq!(
            ev_with_anim("SelfAnimExist(44)", &ch, &actions),
            Value::Int(1)
        );
        assert_eq!(
            ev_with_anim("SelfAnimExist(0)", &ch, &actions),
            Value::Int(1)
        );
        assert_eq!(
            ev_with_anim("SelfAnimExist(41)", &ch, &actions),
            Value::Int(1)
        );
        assert_eq!(
            ev_with_anim("SelfAnimExist(99)", &ch, &actions),
            Value::Int(0)
        );

        // Case-insensitive trigger name.
        assert_eq!(
            ev_with_anim("selfanimexist(44)", &ch, &actions),
            Value::Int(1)
        );
        assert_eq!(
            ev_with_anim("SELFANIMEXIST(99)", &ch, &actions),
            Value::Int(0)
        );

        // Direct trigger call with a missing arg → 0, never panics.
        let ctx = EvalCtx::with_anim(&ch, None, StageView::default(), AnimSet::new(&actions));
        assert_eq!(ctx.trigger("SelfAnimExist", &[]), Value::Int(0));
        // A garbage (NaN) arg coerces to action 0 via `to_int` (NaN → 0) and never
        // panics; here action 0 IS present, so it reports 1 — the point is that a
        // non-integer arg resolves deterministically to a valid lookup, not a crash.
        assert_eq!(
            ctx.trigger("SelfAnimExist", &[Value::Float(f32::NAN)]),
            Value::Int(1)
        );

        // A garbage arg against a set WITHOUT action 0 → 0 (absent), no panic.
        let no_zero = anim_actions(&[41, 44]);
        let ctx2 = EvalCtx::with_anim(&ch, None, StageView::default(), AnimSet::new(&no_zero));
        assert_eq!(
            ctx2.trigger("SelfAnimExist", &[Value::Float(f32::NAN)]),
            Value::Int(0)
        );
    }

    #[test]
    fn selfanimexist_drives_common1_fallback_idiom() {
        // common1 `[Statedef 50]` picks the falling variant via
        // `SelfAnimExist(anim + 3)`; `[Statedef 45]` AirJump uses `SelfAnimExist(44)`.
        // Action set has the base jump-related actions plus 44 but NOT 53.
        let actions = anim_actions(&[40, 41, 44, 50]);
        let mut ch = Character::new();

        // anim = 50 → anim + 3 = 53, which is ABSENT → the fallback is not taken.
        ch.anim = 50;
        assert_eq!(
            ev_with_anim("SelfAnimExist(anim + 3)", &ch, &actions),
            Value::Int(0)
        );
        // anim = 41 → anim + 3 = 44, which is PRESENT → the fallback IS taken.
        ch.anim = 41;
        assert_eq!(
            ev_with_anim("SelfAnimExist(anim + 3)", &ch, &actions),
            Value::Int(1)
        );
        // The AirJump idiom: action 44 exists, so the air-jump anim is chosen.
        assert_eq!(
            ev_with_anim("SelfAnimExist(44)", &ch, &actions),
            Value::Int(1)
        );
    }

    #[test]
    fn selfanimexist_with_no_air_in_view_is_zero_without_panic() {
        // The opponent context / out-of-tick seam / bare-`Character` evaluation
        // carry an EMPTY `AnimSet`: `SelfAnimExist(n)` reports every action
        // absent (0) and never panics — the documented no-AIR fallback.
        let ch = Character::new();
        let stage = StageView::default();

        // EvalCtx::new (no anim supplied) → empty default set.
        let ctx = EvalCtx::new(&ch, None, stage);
        assert_eq!(
            ctx.trigger("SelfAnimExist", &[Value::Int(44)]),
            Value::Int(0)
        );
        assert_eq!(
            ev_cross("SelfAnimExist(44)", &ch, None, stage),
            Value::Int(0)
        );

        // A bare `Character` (self-only context) also has no AIR → 0.
        assert_eq!(
            ch.trigger("SelfAnimExist", &[Value::Int(44)]),
            Value::Int(0)
        );

        // `enemy, SelfAnimExist(...)` degrades to 0: the opponent context is built
        // without an `.air` view (documented approximation for a flat 1-v-1).
        let (me, opp) = two_chars();
        assert_eq!(
            ev_cross("enemy, SelfAnimExist(0)", &me, Some(&opp), stage),
            Value::Int(0)
        );
    }

    // ---- Gated real-KFM test (skip-if-missing) --------------------------

    /// With real KFM loaded, `SelfAnimExist` must report a known action present
    /// and a bogus one absent, through the real trigger/eval path. Skips cleanly
    /// (printed reason) when `test-assets/` is absent.
    #[test]
    fn real_kfm_selfanimexist_known_action_exists() {
        let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let lc = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: kfm.def failed to load: {e}");
                return;
            }
        };
        // KFM ships a stand light punch as action 200; a 5-digit action like
        // 99999 is never authored. Assert both through the eval path.
        let actions = &lc.air.actions;
        if !actions.contains_key(&200) {
            eprintln!("skipping: KFM action 200 not present in this fixture");
            return;
        }
        let ch = Character::with_constants(lc.constants);
        assert_eq!(
            ev_with_anim("SelfAnimExist(200)", &ch, actions),
            Value::Int(1),
            "real KFM action 200 should exist"
        );
        assert_eq!(
            ev_with_anim("SelfAnimExist(99999)", &ch, actions),
            Value::Int(0),
            "bogus action 99999 should not exist"
        );
    }

    // =====================================================================
    // Proctor: additional SelfAnimExist / AnimSet edge cases (audit P22).
    // These extend Forge's suite with the AnimSet unit API, signed/boundary
    // action numbers, float-arg truncation, the empty-map (not just None)
    // case, and Copy semantics — every path documented as "never panics".
    // =====================================================================

    #[test]
    fn animset_contains_unit_api() {
        // The thin AnimSet wrapper is the load-bearing primitive: `contains`
        // must be exact on a populated set and uniformly false on the empty
        // (default) set, never panicking on any input.
        let actions = anim_actions(&[0, 41, 44]);
        let set = AnimSet::new(&actions);
        assert!(set.contains(0));
        assert!(set.contains(41));
        assert!(set.contains(44));
        assert!(!set.contains(1));
        assert!(!set.contains(-7));
        assert!(!set.contains(i32::MAX));
        assert!(!set.contains(i32::MIN));

        // The default (no `.air` in view) set reports every action absent.
        let empty = AnimSet::default();
        assert!(!empty.contains(0));
        assert!(!empty.contains(44));
        assert!(!empty.contains(i32::MIN));
    }

    #[test]
    fn animset_is_copy_and_shares_the_map() {
        // AnimSet must stay `Copy` (it rides inside the `Copy` EvalEnv/EvalCtx).
        // A bitwise copy observes the same backing map — no clone, no alloc.
        let actions = anim_actions(&[5]);
        let set = AnimSet::new(&actions);
        let copy = set; // Copy, not move: `set` stays usable below.
        assert!(set.contains(5));
        assert!(copy.contains(5));
        assert!(!copy.contains(6));
    }

    #[test]
    fn selfanimexist_empty_air_table_is_zero() {
        // A loaded-but-empty `.air` (`actions` map present yet empty) is distinct
        // from the `None` default; both must report every action absent.
        let empty: HashMap<i32, AnimAction> = HashMap::new();
        let ch = Character::new();
        assert_eq!(ev_with_anim("SelfAnimExist(0)", &ch, &empty), Value::Int(0));
        assert_eq!(
            ev_with_anim("SelfAnimExist(200)", &ch, &empty),
            Value::Int(0)
        );
    }

    #[test]
    fn selfanimexist_handles_negative_and_boundary_actions() {
        // MUGEN action numbers can be negative; SelfAnimExist must resolve them
        // exactly and never panic on the i32 extremes.
        let actions = anim_actions(&[-1, 0, i32::MAX, i32::MIN]);
        let ch = Character::new();
        assert_eq!(
            ev_with_anim("SelfAnimExist(-1)", &ch, &actions),
            Value::Int(1)
        );
        assert_eq!(
            ev_with_anim("SelfAnimExist(0)", &ch, &actions),
            Value::Int(1)
        );
        // Absent negative number → 0.
        assert_eq!(
            ev_with_anim("SelfAnimExist(-2)", &ch, &actions),
            Value::Int(0)
        );

        // i32 boundaries through the direct trigger path (literal parsing of the
        // extremes is brittle, so drive them as explicit Value args).
        let ctx = EvalCtx::with_anim(&ch, None, StageView::default(), AnimSet::new(&actions));
        assert_eq!(
            ctx.trigger("SelfAnimExist", &[Value::Int(i32::MAX)]),
            Value::Int(1)
        );
        assert_eq!(
            ctx.trigger("SelfAnimExist", &[Value::Int(i32::MIN)]),
            Value::Int(1)
        );
    }

    #[test]
    fn selfanimexist_float_arg_truncates_toward_zero() {
        // The VM coerces a float arg via `to_int` (truncation toward zero). A
        // fractional action number must look up the truncated integer, not round.
        let actions = anim_actions(&[44]);
        let ch = Character::new();
        let ctx = EvalCtx::with_anim(&ch, None, StageView::default(), AnimSet::new(&actions));
        // 44.9 → 44 (present); 45.9 → 45 (absent). Truncation, not rounding.
        assert_eq!(
            ctx.trigger("SelfAnimExist", &[Value::Float(44.9)]),
            Value::Int(1)
        );
        assert_eq!(
            ctx.trigger("SelfAnimExist", &[Value::Float(45.9)]),
            Value::Int(0)
        );
        // Negative fractional truncates toward zero too: -0.9 → 0 (absent here).
        assert_eq!(
            ctx.trigger("SelfAnimExist", &[Value::Float(-0.9)]),
            Value::Int(0)
        );
    }

    #[test]
    fn selfanimexist_extra_args_use_only_the_first() {
        // MUGEN's SelfAnimExist takes one argument; a stray second arg must be
        // ignored (the first decides the result) and never panic.
        let actions = anim_actions(&[44]);
        let ch = Character::new();
        let ctx = EvalCtx::with_anim(&ch, None, StageView::default(), AnimSet::new(&actions));
        assert_eq!(
            ctx.trigger("SelfAnimExist", &[Value::Int(44), Value::Int(99)]),
            Value::Int(1)
        );
        assert_eq!(
            ctx.trigger("SelfAnimExist", &[Value::Int(99), Value::Int(44)]),
            Value::Int(0)
        );
    }

    #[test]
    fn selfanimexist_in_boolean_compounds_through_eval() {
        // The common1 fallback idiom embeds SelfAnimExist in boolean logic
        // (`trigger1 = SelfAnimExist(anim + 3)`), so it must compose with &&/||/!
        // and comparisons through the real eval path, not just stand alone.
        let actions = anim_actions(&[41, 44]);
        let mut ch = Character::new();
        ch.anim = 41; // anim + 3 == 44 (present)
                      // Present → the AND with a true self-read holds.
        assert_eq!(
            ev_with_anim("SelfAnimExist(anim + 3) && Anim = 41", &ch, &actions),
            Value::Int(1)
        );
        // Negation of a present action is false.
        assert_eq!(
            ev_with_anim("!SelfAnimExist(44)", &ch, &actions),
            Value::Int(0)
        );
        // OR short-circuits to the present branch even when the first is absent.
        assert_eq!(
            ev_with_anim("SelfAnimExist(999) || SelfAnimExist(44)", &ch, &actions),
            Value::Int(1)
        );
        // SelfAnimExist used as an integer (compared to 1/0) also resolves.
        assert_eq!(
            ev_with_anim("SelfAnimExist(44) = 1", &ch, &actions),
            Value::Int(1)
        );
        assert_eq!(
            ev_with_anim("SelfAnimExist(999) = 0", &ch, &actions),
            Value::Int(1)
        );
    }

    // ---- RNG-in-state: the `random` trigger (faithfulness audit #28) ----

    #[test]
    fn random_seam_is_not_constant_zero() {
        // Regression guard for audit gap #28: before this fix `Character` used the
        // trait default `EvalContext::random` which always returned 0, so every
        // `random` trigger read 0. A fresh (default-seeded) character must now draw
        // a *varied* [0,999] sequence — i.e. NOT all zeros.
        let ch = Character::new();
        let draws: Vec<i32> = (0..50)
            .map(|_| match ev("random", &ch) {
                Value::Int(i) => i,
                other => panic!("bare `random` should be Int, got {other:?}"),
            })
            .collect();
        // Every draw is in MUGEN's classic inclusive range [0, 999].
        for &v in &draws {
            assert!((0..=999).contains(&v), "random out of [0,999]: {v}");
        }
        // The sequence is not pinned to a single value (would prove it is still
        // the old constant-0 / constant-anything stub).
        assert!(
            draws.iter().any(|&v| v != draws[0]),
            "random must vary across draws, got all {}",
            draws[0]
        );
    }

    #[test]
    fn random_covers_a_wide_band_of_the_range() {
        // Across many draws `random` should spread across the [0,999] range, not
        // cluster at one end — a sanity check that the Park–Miller stream is wired
        // and mapped, not returning a degenerate fixed/near-fixed value.
        let ch = Character::new();
        let mut lo = i32::MAX;
        let mut hi = i32::MIN;
        for _ in 0..1000 {
            if let Value::Int(v) = ev("random", &ch) {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        assert!(
            lo < 100,
            "min draw {lo} unexpectedly high — range not covered"
        );
        assert!(
            hi > 900,
            "max draw {hi} unexpectedly low — range not covered"
        );
    }

    #[test]
    fn random_is_deterministic_for_a_fixed_seed() {
        // Same seed ⇒ identical sequence (the determinism that netplay rollback /
        // replay, #38, relies on).
        let mut a = Character::new();
        let mut b = Character::new();
        a.seed_rng(12345);
        b.seed_rng(12345);
        let seq_a: Vec<Value> = (0..20).map(|_| ev("random", &a)).collect();
        let seq_b: Vec<Value> = (0..20).map(|_| ev("random", &b)).collect();
        assert_eq!(
            seq_a, seq_b,
            "equal seeds must yield equal random sequences"
        );
    }

    #[test]
    fn random_different_seeds_diverge() {
        // Distinct seeds should give distinct streams (otherwise the seed is
        // ignored and seeding is a no-op).
        let mut a = Character::new();
        let mut b = Character::new();
        a.seed_rng(1);
        b.seed_rng(99999);
        let seq_a: Vec<Value> = (0..20).map(|_| ev("random", &a)).collect();
        let seq_b: Vec<Value> = (0..20).map(|_| ev("random", &b)).collect();
        assert_ne!(seq_a, seq_b, "different seeds should diverge");
    }

    #[test]
    fn random_advances_each_draw() {
        // Two successive draws from the same character advance the RNG state, so
        // they (with overwhelming probability for Park–Miller) differ; at minimum
        // the underlying raw seed must change between draws.
        let ch = Character::new();
        let before = ch.rng_seed.get();
        let _ = ch.random();
        let after = ch.rng_seed.get();
        assert_ne!(before, after, "a draw must advance the stored RNG state");
        // And the raw `Character::random` seam itself returns successive,
        // generally-distinct draws in the generator's range 1..=2^31-2.
        let r1 = ch.random();
        let r2 = ch.random();
        // `next_u31` returns the generator state in 1..=2^31-2.
        const PARK_MILLER_MAX: i32 = 2_147_483_646;
        assert!(
            (1..=PARK_MILLER_MAX).contains(&r1),
            "raw draw out of range: {r1}"
        );
        assert!(
            (1..=PARK_MILLER_MAX).contains(&r2),
            "raw draw out of range: {r2}"
        );
        assert_ne!(r1, r2, "successive raw draws should differ");
    }

    #[test]
    fn random_matches_fp_vm_rng_for_same_seed() {
        // The character's stream must BE fp-vm's Park–Miller `Rng` (we reuse it,
        // not reimplement it): the raw draws from `Character::random` after seeding
        // to S equal `Rng::new(S)`'s `next_u31()` sequence.
        let mut ch = Character::new();
        ch.seed_rng(7);
        let mut reference = Rng::new(7);
        for _ in 0..16 {
            assert_eq!(ch.random(), reference.next_u31());
        }
    }

    #[test]
    fn random_call_form_uses_the_same_seam() {
        // `random(lo, hi)` evaluated against a character draws from the same seam
        // as the bare `random`; equally-seeded characters agree on the call form.
        let mut a = Character::new();
        let mut b = Character::new();
        a.seed_rng(42);
        b.seed_rng(42);
        for _ in 0..10 {
            let va = ev("random(10, 20)", &a);
            let vb = ev("random(10, 20)", &b);
            assert_eq!(va, vb);
            if let Value::Int(i) = va {
                assert!((10..=20).contains(&i), "random(10,20) out of range: {i}");
            } else {
                panic!("random(lo,hi) should be Int, got {va:?}");
            }
        }
    }

    #[test]
    fn redirected_random_uses_targets_seam() {
        // `enemy, random` must advance the OPPONENT's RNG, not self's — the
        // `EvalCtx` redirect forwards `random()` to the target character. Seed the
        // opponent so its stream is reproducible, leave `me` on the default seed,
        // and confirm the redirected draw equals the opponent's own first draw.
        let me = Character::new();
        let mut opp = Character::new();
        opp.seed_rng(555);
        let stage = StageView::default();
        // Reference: the opponent's own first [0,999] draw from an identical seed.
        let mut reference = Character::new();
        reference.seed_rng(555);
        let expected = ev("random", &reference);
        let drawn = ev_cross("enemy, random", &me, Some(&opp), stage);
        assert_eq!(
            drawn, expected,
            "redirected `enemy, random` must draw from the opponent's RNG seam"
        );
        // And it must have advanced the OPPONENT's state, not `me`'s: me's seed is
        // untouched by the redirected draw.
        assert_eq!(
            me.rng_seed.get(),
            Character::new().rng_seed.get(),
            "redirected random must not advance self's RNG"
        );
    }

    #[test]
    fn seed_rng_normalizes_degenerate_seed() {
        // Seeding with 0 (Park–Miller is undefined there) must not collapse the
        // stream: the stored seed is normalized into 1..=2^31-2 and draws stay in
        // range and vary, never panicking.
        let mut ch = Character::new();
        ch.seed_rng(0);
        assert_ne!(
            ch.rng_seed.get(),
            0,
            "seed 0 must be normalized away from 0"
        );
        let mut any_nonzero = false;
        for _ in 0..20 {
            if let Value::Int(v) = ev("random", &ch) {
                assert!((0..=999).contains(&v));
                any_nonzero |= v != 0;
            }
        }
        assert!(
            any_nonzero,
            "seed-0 stream must still produce nonzero draws"
        );
    }

    // =========================================================================
    // T054 — Range-of-motion (ROM) behavioral test table
    //
    // Table-driven: drives the shipped trainingdummy through the executor using
    // the same synthetic held-input path as T048/T052 seed tests above.  Each
    // row maps a held-input motion → expected state + pos/vel outcome.  Adding a
    // new motion = adding one `RomCase` row.
    // =========================================================================

    /// One row in the ROM table: a motion (as held command names) → assertions.
    struct RomCase {
        /// Human-readable label printed on failure.
        name: &'static str,
        /// Command names installed as `ActiveCommands` every tick.
        cmds: &'static [&'static str],
        /// State the character starts in (0 = stand, 11 = crouching-held, …).
        start_state: i32,
        /// Whether the character has control at tick 0.
        start_ctrl: bool,
        /// How many ticks to drive before sampling.
        ticks: u32,
        /// The character must pass through at least one of these states within
        /// `ticks` ticks.  Empty slice means no state requirement.
        expect_states: &'static [i32],
        /// `pos.x` must be strictly Greater / Less / Equal compared to the
        /// initial value after `ticks` ticks.
        expect_pos_x: std::cmp::Ordering,
        /// `pos.y` must be strictly Greater / Less / Equal compared to the
        /// initial value after `ticks` ticks. (Y-up = negative in MUGEN.)
        expect_pos_y: std::cmp::Ordering,
    }

    /// ROM table covering idle, walk fwd/back, crouch, stand-from-crouch,
    /// jump neutral, and basic attack.  Order follows the task description.
    const ROM: &[RomCase] = &[
        // ----------------------------------------------------------------
        // Idle: no input → stays in stand state 0; pos does not drift.
        // ----------------------------------------------------------------
        RomCase {
            name: "idle",
            cmds: &[],
            start_state: 0,
            start_ctrl: true,
            ticks: 30,
            expect_states: &[0],
            expect_pos_x: std::cmp::Ordering::Equal,
            expect_pos_y: std::cmp::Ordering::Equal,
        },
        // ----------------------------------------------------------------
        // Walk forward: holdfwd → enters walk state 20, pos.x increases.
        // ----------------------------------------------------------------
        RomCase {
            name: "walk fwd",
            cmds: &["holdfwd"],
            start_state: 0,
            start_ctrl: true,
            ticks: 30,
            expect_states: &[20],
            expect_pos_x: std::cmp::Ordering::Greater,
            expect_pos_y: std::cmp::Ordering::Equal,
        },
        // ----------------------------------------------------------------
        // Walk back: holdback → enters walk state 20, pos.x decreases.
        // ----------------------------------------------------------------
        RomCase {
            name: "walk back",
            cmds: &["holdback"],
            start_state: 0,
            start_ctrl: true,
            ticks: 30,
            expect_states: &[20],
            expect_pos_x: std::cmp::Ordering::Less,
            expect_pos_y: std::cmp::Ordering::Equal,
        },
        // ----------------------------------------------------------------
        // Crouch: holddown → enters crouch start state 10 (then 11).
        // ----------------------------------------------------------------
        RomCase {
            name: "crouch",
            cmds: &["holddown"],
            start_state: 0,
            start_ctrl: true,
            ticks: 30,
            expect_states: &[10, 11],
            expect_pos_x: std::cmp::Ordering::Equal,
            expect_pos_y: std::cmp::Ordering::Equal,
        },
        // ----------------------------------------------------------------
        // Stand from crouch: start in crouching-held (11), release input →
        // transitions through 12 and returns to stand state 0.
        // ----------------------------------------------------------------
        RomCase {
            name: "stand from crouch",
            cmds: &[],
            start_state: 11,
            start_ctrl: true,
            ticks: 40,
            expect_states: &[0],
            expect_pos_x: std::cmp::Ordering::Equal,
            expect_pos_y: std::cmp::Ordering::Equal,
        },
        // ----------------------------------------------------------------
        // Jump neutral: holdup → enters jumpstart 40 then air state 50.
        // pos.y departs from 0 (negative = upward in MUGEN convention).
        // The jumpstart action (40) has 3-tick duration; the character
        // passes through state 40 and then lands in state 50 (air).
        // Both states are accepted so the check is robust to timing.
        // ----------------------------------------------------------------
        RomCase {
            name: "jump neutral",
            cmds: &["holdup"],
            start_state: 0,
            start_ctrl: true,
            ticks: 30,
            expect_states: &[40, 50],
            expect_pos_x: std::cmp::Ordering::Equal,
            expect_pos_y: std::cmp::Ordering::Less,
        },
        // ----------------------------------------------------------------
        // Basic attack: command "a" → enters attack state 200.
        // pos assertions are Equal: a one-frame button press starts the
        // attack but state 200 of the trainingdummy has no horizontal vel.
        // ----------------------------------------------------------------
        RomCase {
            name: "basic attack",
            cmds: &["a"],
            start_state: 0,
            start_ctrl: true,
            ticks: 5,
            expect_states: &[200],
            expect_pos_x: std::cmp::Ordering::Equal,
            expect_pos_y: std::cmp::Ordering::Equal,
        },
    ];

    /// Drives one ROM case through the executor and returns
    /// `(reached_expected_state, final_pos_x, final_pos_y, start_pos_x, start_pos_y)`.
    fn run_rom_case(loaded: &LoadedCharacter, case: &RomCase) -> (bool, f32, f32, f32, f32) {
        let mut ch = Character::with_constants(loaded.constants);
        ch.pos = Vec2::new(0.0, 0.0);
        ch.facing = Facing::Right; // forward = +x
        ch.state_no = case.start_state;
        ch.ctrl = case.start_ctrl;
        ch.anim = 0;
        ch.set_command_source(Box::new(ActiveCommands::from_names(
            case.cmds.iter().copied(),
        )));

        let start_x = ch.pos.x;
        let start_y = ch.pos.y;
        // Track whether the character visited any of the expected states.
        let mut reached = case.expect_states.is_empty(); // trivially true when no state required

        for _ in 0..case.ticks {
            ch.tick(loaded, None, StageView::default());
            if !reached && case.expect_states.contains(&ch.state_no) {
                reached = true;
            }
        }

        (reached, ch.pos.x, ch.pos.y, start_x, start_y)
    }

    /// T054: table-driven range-of-motion test on the shipped trainingdummy.
    ///
    /// Drives idle, walk fwd/back, crouch, stand-from-crouch, jump neutral, and
    /// basic attack through the executor using the synthetic `ActiveCommands`
    /// path — the same pattern as the T048/T052 seed tests.  Asserts the
    /// correct state is reached and pos/vel respond as expected.
    #[test]
    fn trainingdummy_range_of_motion_table() {
        let def = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/trainingdummy/trainingdummy.def");
        assert!(
            def.exists(),
            "shipped Training Dummy missing: {}",
            def.display()
        );
        let loaded = LoadedCharacter::load(&def).expect("trainingdummy.def should load");

        for case in ROM {
            let (reached, end_x, end_y, start_x, start_y) = run_rom_case(&loaded, case);

            // State assertion.
            if !case.expect_states.is_empty() {
                assert!(
                    reached,
                    "[ROM: {}] expected to visit one of {:?} within {} ticks",
                    case.name, case.expect_states, case.ticks
                );
            }

            // pos.x ordering assertion.
            let actual_x_ord = end_x
                .partial_cmp(&start_x)
                .unwrap_or(std::cmp::Ordering::Equal);
            assert_eq!(
                actual_x_ord, case.expect_pos_x,
                "[ROM: {}] pos.x ordering wrong: start={start_x}, end={end_x}, \
                 expected {:?}",
                case.name, case.expect_pos_x
            );

            // pos.y ordering assertion.
            let actual_y_ord = end_y
                .partial_cmp(&start_y)
                .unwrap_or(std::cmp::Ordering::Equal);
            assert_eq!(
                actual_y_ord, case.expect_pos_y,
                "[ROM: {}] pos.y ordering wrong: start={start_y}, end={end_y}, \
                 expected {:?}",
                case.name, case.expect_pos_y
            );
        }
    }

    // =====================================================================
    // T061 — Target/hit introspection triggers: `NumTarget` / `NumTarget(id)`,
    // `HitVel X` / `HitVel Y`, and `HitOverridden`. Each reads existing entity
    // state (the bound-target list, GetHitVars, the override flag) and resolves
    // through the same VM eval path the executor uses.
    // =====================================================================

    /// AC: `NumTarget` (and `NumTarget(id)`) reports the count of currently bound
    /// targets. With a target id list wired on the graph (as `fp-engine` does each
    /// tick), a binder with one bound target reads `1`; `NumTarget(id)` matches by
    /// id; an unbound binder reads `0`.
    #[test]
    fn target_and_hit_triggers_num_target_from_graph() {
        let me = Character::new();

        // One bound target with id 2 (the 1-v-1 opponent's player id).
        let target_ids = [2_i32];
        let graph = EntityGraph::default().with_own_target_ids(&target_ids);
        assert_eq!(ev_graph("NumTarget", &me, graph), Value::Int(1));
        // `NumTarget(id)` filters by id: 2 matches, 7 does not.
        assert_eq!(ev_graph("NumTarget(2)", &me, graph), Value::Int(1));
        assert_eq!(ev_graph("NumTarget(7)", &me, graph), Value::Int(0));

        // No target bound (empty list wired is the same as no list): 0.
        let none = EntityGraph::default();
        assert_eq!(ev_graph("NumTarget", &me, none), Value::Int(0));
    }

    /// AC: with no target id list wired (a bare/self-only context, e.g. before the
    /// engine installs one), `NumTarget` falls back to the entity's own
    /// `has_target` flag, so a binder that has connected reads `1` and one that has
    /// not reads `0`. A specific-id query with no list cannot match → `0`.
    #[test]
    fn target_and_hit_triggers_num_target_self_only_fallback() {
        let mut me = Character::new();
        let graph = EntityGraph::default();

        // No target yet → 0.
        assert_eq!(ev_graph("NumTarget", &me, graph), Value::Int(0));

        // Connected a hit → has_target set → 1 (bare form).
        me.has_target = true;
        assert_eq!(ev_graph("NumTarget", &me, graph), Value::Int(1));
        // Without a wired id list, the id form has nothing to match → 0.
        assert_eq!(ev_graph("NumTarget(2)", &me, graph), Value::Int(0));
    }

    /// AC: `HitVel X` / `HitVel Y` return the velocity the most recent hit taken
    /// imparted, read from this character's GetHitVars (`xvel`/`yvel`). A
    /// freshly-built character (no hit) reads `0` on both axes.
    #[test]
    fn target_and_hit_triggers_hit_vel_by_axis() {
        let mut ch = Character::new();
        // No hit taken yet — both axes default to 0.
        assert_eq!(ev("HitVel X", &ch), Value::Float(0.0));
        assert_eq!(ev("HitVel Y", &ch), Value::Float(0.0));

        // Populate GetHitVars as hit resolution would.
        ch.get_hit_vars.xvel = -4.5;
        ch.get_hit_vars.yvel = -7.0;
        assert_eq!(ev("HitVel X", &ch), Value::Float(-4.5));
        assert_eq!(ev("HitVel Y", &ch), Value::Float(-7.0));
        // A bare `HitVel` (no/garbage axis) falls back to the X component.
        assert_eq!(ev("HitVel", &ch), Value::Float(-4.5));
        // Threads through comparison operators like the other vel triggers.
        assert_eq!(ev("HitVel Y < 0", &ch), Value::Int(1));
    }

    /// AC: `HitOverridden` returns 1 iff the current get-hit is redirected by an
    /// active HitOverride slot — modeled by the `hit_overridden` flag. Default is
    /// 0; set it and the trigger reads 1.
    #[test]
    fn target_and_hit_triggers_hit_overridden_flag() {
        let mut ch = Character::new();
        assert_eq!(ev("HitOverridden", &ch), Value::Int(0));

        ch.hit_overridden = true;
        assert_eq!(ev("HitOverridden", &ch), Value::Int(1));

        // Cleared again → 0.
        ch.hit_overridden = false;
        assert_eq!(ev("HitOverridden", &ch), Value::Int(0));
    }
}
