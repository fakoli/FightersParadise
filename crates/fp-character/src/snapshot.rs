//! Runtime-state snapshot for a live [`Character`] (replay / rollback, #38).
//!
//! [`CharacterSnapshot`] is a **plain-data** mirror of the *mutable runtime state*
//! a [`Character`] carries from tick to tick — position, velocity, facing,
//! life/power, the state-machine cursor, the variable banks, the RNG position,
//! the combat bookkeeping (`active_hitdef`, `get_hit_vars`, hitpause, juggle, …),
//! and every transient per-tick effect (`asserted`, `cur_width`, `cur_palfx`,
//! `afterimage`, `hit_overrides`, `invuln`). It derives
//! [`Serialize`]/[`Deserialize`] so a whole [`crate::Character`] can be saved and
//! restored for frame-perfect netplay rollback or deterministic replay.
//!
//! # Runtime vs. static split (important)
//!
//! A snapshot captures **only** the runtime state. It deliberately does **not**
//! carry the *loaded static data* that is reloaded from the character's `.def`:
//!
//! - the compiled state graph, sprites, AIR, and CMD ([`crate::LoadedCharacter`]),
//! - the per-character [`CharacterConstants`](crate::CharacterConstants) (life/
//!   power maxima, sizes, velocities, gravity/friction — authored, reloaded),
//! - the command-source seam ([`crate::CommandSource`]), which is rebuilt from the
//!   per-frame input each tick.
//!
//! [`Character::restore_from_snapshot`](crate::Character::restore_from_snapshot)
//! is therefore applied to an **already-loaded** character (same `.def`): it
//! overwrites the mutable runtime fields and leaves the static handles untouched.
//! Restoring into a *different* character is a logic error the caller must avoid;
//! the snapshot carries no identity to detect it.
//!
//! # Never panics
//!
//! Constructing a snapshot is infallible. Decoding one from bytes goes through
//! [`bincode`] (in `fp-engine`'s match snapshot), which returns a recoverable
//! error on truncated / malformed input rather than panicking — the snapshot type
//! itself is pure data with no validation that could fail.

use serde::{Deserialize, Serialize};

use crate::invuln::InvulnMask;
use crate::{
    AfterImageState, AssertedFlags, Character, CurPalFx, Facing, GetHitVars, HitOverrides,
    MoveConnect, MoveType, Physics, RoundView, StateType, WidthOverride, NUM_FVARS, NUM_SYSFVARS,
    NUM_SYSVARS, NUM_VARS,
};
use fp_combat::HitDef;
use fp_core::Vec2;

/// A serializable snapshot of a [`Character`]'s mutable runtime state (#38).
///
/// Every field mirrors the like-named [`Character`] field; see that struct for
/// the per-field semantics. The [`rng_seed`](Self::rng_seed) is stored as a plain
/// `i32` (the raw Park–Miller state read out of the character's
/// [`Cell`](std::cell::Cell)). Build one with [`CharacterSnapshot::capture`] and
/// apply it with [`CharacterSnapshot::apply_to`] (or, more conveniently, via
/// [`Character::snapshot`](crate::Character::snapshot) /
/// [`Character::restore_from_snapshot`](crate::Character::restore_from_snapshot)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CharacterSnapshot {
    // ---- Kinematics --------------------------------------------------------
    /// World position in pixels.
    pub pos: Vec2<f32>,
    /// Velocity in pixels/tick.
    pub vel: Vec2<f32>,
    /// Which way the character faces.
    pub facing: Facing,

    // ---- Resources ---------------------------------------------------------
    /// Current life.
    pub life: i32,
    /// Maximum life (runtime copy; mutated by `LifeSet`/round flow).
    pub life_max: i32,
    /// Current power / super meter.
    pub power: i32,
    /// Maximum power (runtime copy).
    pub power_max: i32,
    /// Whether the player currently has control.
    pub ctrl: bool,
    /// Whether the character is holding "back" (guarding).
    pub holding_back: bool,

    // ---- State categories --------------------------------------------------
    /// Stance category.
    pub state_type: StateType,
    /// Action category.
    pub move_type: MoveType,
    /// Physics mode.
    pub physics: Physics,

    // ---- Animation cursor --------------------------------------------------
    /// Current animation (action) id.
    pub anim: i32,
    /// Zero-based current animation element index.
    pub anim_elem: i32,
    /// Ticks elapsed within the current animation element.
    pub anim_elem_time: i32,
    /// Ticks remaining until the current animation finishes.
    pub anim_time: i32,
    /// Per-element cumulative start-offset table for the current action.
    pub anim_elem_start_offsets: Vec<i32>,
    /// The action number the offset table was built for.
    pub anim_table_action: Option<i32>,

    // ---- State machine cursor ----------------------------------------------
    /// Current state number.
    pub state_no: i32,
    /// Previous state number.
    pub prev_state_no: i32,
    /// Ticks elapsed in the current state.
    pub state_time: i32,

    // ---- Variable banks ----------------------------------------------------
    /// Integer variable bank, `var(0)..=var(59)`.
    pub vars: Vec<i32>,
    /// Float variable bank, `fvar(0)..=fvar(39)`.
    pub fvars: Vec<f32>,
    /// System integer variable bank.
    pub sysvars: Vec<i32>,
    /// System float variable bank.
    pub sysfvars: Vec<f32>,

    // ---- Executor bookkeeping ---------------------------------------------
    /// Per-state-entry firing counts (the `persistent` enforcement table),
    /// keyed by `(owning_state_number, controller_index)`.
    pub fire_counts: Vec<((i32, usize), i32)>,
    /// Air-jumps performed since last leaving the ground.
    pub air_jump_count: i32,
    /// Whether up was held on the previous tick (air-jump edge detection).
    pub up_held_prev: bool,

    // ---- Combat ------------------------------------------------------------
    /// The character's currently-active `HitDef`, if any.
    pub active_hitdef: Option<HitDef>,
    /// The last-hit-taken effect variables.
    pub get_hit_vars: GetHitVars,
    /// Remaining hit-pause ticks.
    pub hitpause: i32,
    /// Remaining hit-shake ticks.
    pub shaketime: i32,
    /// The current move's connection state.
    pub move_connect: MoveConnect,
    /// Whether the character has a hit-established target.
    pub has_target: bool,
    /// Per-projectile-id contact/hit/guard timing (the `Proj*<id>` triggers),
    /// captured as a key-sorted `Vec` so the snapshot bytes stay deterministic
    /// (the live field is a `HashMap`).
    pub proj_events: Vec<(i32, crate::ProjContactTracker)>,
    /// Runtime attack multiplier.
    pub attack_mul: f32,
    /// Runtime defence multiplier.
    pub defence_mul: f32,
    /// Current sprite-draw priority.
    pub cur_sprpriority: i32,
    /// Remaining air-juggle points.
    pub juggle_points: i32,
    /// The current move's juggle cost.
    pub cur_juggle_cost: i32,
    /// Whether the `HitDef` controller fired this tick.
    pub hitdef_set_this_tick: bool,

    // ---- Per-tick / windowed effects --------------------------------------
    /// The `NotHitBy`/`HitBy` invulnerability mask.
    pub invuln: InvulnMask,
    /// The per-tick `AssertSpecial` flag set.
    pub asserted: AssertedFlags,
    /// The per-tick player-push width override.
    pub cur_width: WidthOverride,
    /// The live `PalFX` color tint.
    pub cur_palfx: CurPalFx,
    /// The live `AfterImage` trail.
    pub afterimage: AfterImageState,
    /// The 8-slot `HitOverride` table.
    pub hit_overrides: HitOverrides,

    // ---- RNG + round clock -------------------------------------------------
    /// Raw Park–Miller RNG state (the `Cell<i32>` read out as a plain `i32`).
    pub rng_seed: i32,
    /// The engine-assigned AI difficulty level (`0` = human, `1..=8` = CPU), the
    /// value the `AILevel` trigger reads (T052). Stored as a plain `u8` alongside
    /// the RNG seed so a snapshot round-trips a fighter's CPU identity.
    pub ai_level: u8,
    /// The engine-global round / match clock view.
    pub round_view: RoundView,
}

impl CharacterSnapshot {
    /// Captures a snapshot of the given character's mutable runtime state.
    ///
    /// Reads (never mutates) every runtime field. The static handles (`commands`,
    /// `constants`, the loaded asset graph) are not captured — see the
    /// [module docs](crate::snapshot).
    #[must_use]
    pub fn capture(ch: &Character) -> Self {
        // `fire_counts` is a HashMap whose iteration order is process-randomized;
        // sort the captured pairs by key so the snapshot bytes are DETERMINISTIC
        // across runs (two identical states must serialize to identical bytes for
        // the byte-equality determinism / replay proofs). Apply order is
        // irrelevant (it is a plain key→count map), only the encoding order is.
        let mut fire_counts: Vec<((i32, usize), i32)> =
            ch.fire_counts.iter().map(|(&k, &v)| (k, v)).collect();
        fire_counts.sort_unstable_by_key(|&(k, _)| k);

        // Likewise `proj_events` (the `Proj*<id>` trigger tracker) is a HashMap —
        // sort by projid for deterministic snapshot bytes.
        let mut proj_events: Vec<(i32, crate::ProjContactTracker)> =
            ch.proj_events.iter().map(|(&k, &v)| (k, v)).collect();
        proj_events.sort_unstable_by_key(|&(k, _)| k);

        Self {
            pos: ch.pos,
            vel: ch.vel,
            facing: ch.facing,
            life: ch.life,
            life_max: ch.life_max,
            power: ch.power,
            power_max: ch.power_max,
            ctrl: ch.ctrl,
            holding_back: ch.holding_back,
            state_type: ch.state_type,
            move_type: ch.move_type,
            physics: ch.physics,
            anim: ch.anim,
            anim_elem: ch.anim_elem,
            anim_elem_time: ch.anim_elem_time,
            anim_time: ch.anim_time,
            anim_elem_start_offsets: ch.anim_elem_start_offsets.clone(),
            anim_table_action: ch.anim_table_action,
            state_no: ch.state_no,
            prev_state_no: ch.prev_state_no,
            state_time: ch.state_time,
            vars: ch.vars.to_vec(),
            fvars: ch.fvars.to_vec(),
            sysvars: ch.sysvars.to_vec(),
            sysfvars: ch.sysfvars.to_vec(),
            fire_counts,
            air_jump_count: ch.air_jump_count,
            up_held_prev: ch.up_held_prev,
            active_hitdef: ch.active_hitdef,
            get_hit_vars: ch.get_hit_vars,
            hitpause: ch.hitpause,
            shaketime: ch.shaketime,
            move_connect: ch.move_connect,
            has_target: ch.has_target,
            proj_events,
            attack_mul: ch.attack_mul,
            defence_mul: ch.defence_mul,
            cur_sprpriority: ch.cur_sprpriority,
            juggle_points: ch.juggle_points,
            cur_juggle_cost: ch.cur_juggle_cost,
            hitdef_set_this_tick: ch.hitdef_set_this_tick,
            invuln: ch.invuln.clone(),
            asserted: ch.asserted.clone(),
            cur_width: ch.cur_width,
            cur_palfx: ch.cur_palfx,
            afterimage: ch.afterimage.clone(),
            hit_overrides: ch.hit_overrides.clone(),
            rng_seed: ch.rng_seed.get(),
            ai_level: ch.ai_level,
            round_view: ch.round_view,
        }
    }

    /// Restores this snapshot's runtime state onto an already-loaded character.
    ///
    /// Overwrites every mutable runtime field of `ch` and leaves the static
    /// handles (`commands`, `constants`, the loaded assets) untouched. The
    /// variable banks are length-clamped defensively: a snapshot whose bank
    /// vectors are short/long (only reachable from hand-built or corrupted data,
    /// since [`capture`](Self::capture) always emits the exact bank lengths) is
    /// copied element-wise up to the smaller length, never panicking on an
    /// out-of-range index.
    pub fn apply_to(&self, ch: &mut Character) {
        ch.pos = self.pos;
        ch.vel = self.vel;
        ch.facing = self.facing;
        ch.life = self.life;
        ch.life_max = self.life_max;
        ch.power = self.power;
        ch.power_max = self.power_max;
        ch.ctrl = self.ctrl;
        ch.holding_back = self.holding_back;
        ch.state_type = self.state_type;
        ch.move_type = self.move_type;
        ch.physics = self.physics;
        ch.anim = self.anim;
        ch.anim_elem = self.anim_elem;
        ch.anim_elem_time = self.anim_elem_time;
        ch.anim_time = self.anim_time;
        ch.anim_elem_start_offsets = self.anim_elem_start_offsets.clone();
        ch.anim_table_action = self.anim_table_action;
        ch.state_no = self.state_no;
        ch.prev_state_no = self.prev_state_no;
        ch.state_time = self.state_time;
        copy_clamped(&mut ch.vars, &self.vars);
        copy_clamped(&mut ch.fvars, &self.fvars);
        copy_clamped(&mut ch.sysvars, &self.sysvars);
        copy_clamped(&mut ch.sysfvars, &self.sysfvars);
        ch.fire_counts = self.fire_counts.iter().copied().collect();
        ch.air_jump_count = self.air_jump_count;
        ch.up_held_prev = self.up_held_prev;
        ch.active_hitdef = self.active_hitdef;
        ch.get_hit_vars = self.get_hit_vars;
        ch.hitpause = self.hitpause;
        ch.shaketime = self.shaketime;
        ch.move_connect = self.move_connect;
        ch.has_target = self.has_target;
        ch.proj_events = self.proj_events.iter().copied().collect();
        ch.attack_mul = self.attack_mul;
        ch.defence_mul = self.defence_mul;
        ch.cur_sprpriority = self.cur_sprpriority;
        ch.juggle_points = self.juggle_points;
        ch.cur_juggle_cost = self.cur_juggle_cost;
        ch.hitdef_set_this_tick = self.hitdef_set_this_tick;
        ch.invuln = self.invuln.clone();
        ch.asserted = self.asserted.clone();
        ch.cur_width = self.cur_width;
        ch.cur_palfx = self.cur_palfx;
        ch.afterimage = self.afterimage.clone();
        ch.hit_overrides = self.hit_overrides.clone();
        ch.rng_seed.set(self.rng_seed);
        ch.ai_level = self.ai_level;
        ch.round_view = self.round_view;
    }
}

/// Copies `src` into `dst` element-wise up to `min(dst.len(), src.len())`.
///
/// Used to restore the fixed-size variable banks from a snapshot's `Vec` without
/// ever indexing out of range — a defensive guard against a hand-built or
/// corrupted snapshot whose bank length differs from the live bank. A
/// [`capture`](CharacterSnapshot::capture)d snapshot always matches lengths
/// exactly, so the clamp is a no-op on the happy path.
fn copy_clamped<T: Copy>(dst: &mut [T], src: &[T]) {
    let n = dst.len().min(src.len());
    dst[..n].copy_from_slice(&src[..n]);
}

/// Compile-time assertion that the snapshot bank capacities match the live banks.
/// Purely documentary — the runtime `copy_clamped` already tolerates a mismatch.
const _: () = {
    assert!(NUM_VARS == 60);
    assert!(NUM_FVARS == 40);
    assert!(NUM_SYSVARS == 5);
    assert!(NUM_SYSFVARS == 5);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Character;

    #[test]
    fn capture_apply_round_trips_all_fields() {
        let mut ch = Character::new();
        // Mutate a representative spread of runtime fields.
        ch.pos = Vec2::new(12.5, -3.0);
        ch.vel = Vec2::new(-1.0, 2.5);
        ch.facing = Facing::Left;
        ch.life = 432;
        ch.power = 1500;
        ch.ctrl = true;
        ch.state_no = 200;
        ch.prev_state_no = 0;
        ch.state_time = 7;
        ch.anim = 201;
        ch.anim_elem = 3;
        ch.vars[5] = 99;
        ch.fvars[2] = 1.25;
        ch.sysvars[1] = -4;
        ch.hitpause = 6;
        ch.juggle_points = 9;
        ch.attack_mul = 1.5;
        ch.set_ai_level(7);
        ch.seed_rng(12345);
        // Advance the RNG so the stored seed is non-default.
        let _ = fp_vm::EvalContext::random(&ch);
        ch.fire_counts.insert((200, 2), 1);
        // A couple of projectile-contact trackers in the runtime map.
        ch.record_proj_event(2000, false);
        ch.record_proj_event(2001, true);

        let snap = CharacterSnapshot::capture(&ch);

        // Apply onto a freshly-defaulted character and confirm equality of the
        // captured fields.
        let mut restored = Character::new();
        snap.apply_to(&mut restored);
        let snap2 = CharacterSnapshot::capture(&restored);
        assert_eq!(snap, snap2, "capture→apply→capture must round-trip");
    }

    #[test]
    fn apply_tolerates_short_var_bank() {
        // A hand-built snapshot with a short var bank must not panic on apply.
        let mut ch = Character::new();
        let mut snap = CharacterSnapshot::capture(&ch);
        snap.vars = vec![7, 8]; // only two entries
        snap.apply_to(&mut ch);
        assert_eq!(ch.vars[0], 7);
        assert_eq!(ch.vars[1], 8);
        // Remaining slots are left at their prior value (0) — no panic.
        assert_eq!(ch.vars[2], 0);
    }

    #[test]
    fn snapshot_bytes_are_deterministic_across_fire_count_insertion_order() {
        // Two characters with the SAME fire_counts inserted in DIFFERENT orders
        // must produce byte-identical snapshots (the capture sorts by key). This
        // asserts on the ENCODED bytes — the property the whole-Match snapshot /
        // replay determinism proofs actually depend on — not just the sorted Vec.
        let mut a = Character::new();
        let mut b = Character::new();
        for &k in &[(1, 0usize), (2, 3), (0, 7), (5, 1)] {
            a.fire_counts.insert(k, 1);
        }
        for &k in &[(5, 1usize), (0, 7), (2, 3), (1, 0)] {
            b.fire_counts.insert(k, 1);
        }
        let sa = CharacterSnapshot::capture(&a);
        let sb = CharacterSnapshot::capture(&b);
        let bytes_a = bincode::serialize(&sa).expect("serialize a");
        let bytes_b = bincode::serialize(&sb).expect("serialize b");
        assert_eq!(
            bytes_a, bytes_b,
            "snapshot encoded bytes must be identical regardless of fire_counts insert order"
        );
    }
}
