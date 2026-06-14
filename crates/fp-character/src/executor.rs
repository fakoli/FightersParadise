//! # State-machine executor (task 5.3)
//!
//! Drives a live [`Character`] one 60Hz tick at a time against the compiled
//! state graph of a [`LoadedCharacter`]. This is the runtime counterpart to the
//! loader (task 5.2): the loader produces compiled states; the executor runs
//! them.
//!
//! ## What one tick does ([`Character::tick`])
//!
//! 1. **Special-state order.** MUGEN processes the special states `-3`, `-2`,
//!    `-1` and then the *current* state number, in that order, every tick (KB
//!    [03 §3]). `-3` is skipped only when the player is temporarily running
//!    another player's state data (mid-throw custom state) — not yet possible
//!    with a single entity, so `-3` always runs here.
//! 2. **Controller gating.** For each [`CompiledController`] in a state,
//!    evaluated top-to-bottom:
//!    - every `triggerall` expression must be true (logical AND); if any is
//!      false the controller is skipped;
//!    - at least one numbered trigger *group* must be fully true (a group is the
//!      AND of its conditions; groups OR together);
//!    - **CB6 trigger-group contiguity** is applied: groups are considered in
//!      ascending number starting at `trigger1`, and the first gap in the
//!      numbering truncates the rest (`trigger1, trigger2, trigger4` with no
//!      `trigger3` drops `trigger4` and everything after it).
//! 3. **Universal params.** `persistent` controls re-firing across ticks
//!    (`1` = every qualifying tick, the default; `0` = once per state entry;
//!    `n` = every `n`th qualifying tick). `ignorehitpause` lets a controller run
//!    *during* a hit-pause freeze (task 6.5): while [`Character::hitpause`] is
//!    positive the character is frozen (no anim/time/physics advance) and only
//!    `ignorehitpause`-flagged controllers are evaluated; every other controller
//!    is skipped until the pause ends.
//! 4. **State entry & transitions.** On entering a state the executor applies
//!    the statedef's `type`/`movetype`/`physics`/`anim`/`ctrl`/`velset`. A
//!    `ChangeState` controller updates `state_no`/`prev_state_no` and resets
//!    `state_time`, then the new current state is processed in the same tick.
//! 5. **Time & physics.** After controllers run, the statedef `physics` is
//!    applied (stand/crouch friction on x-velocity, air gravity on y-velocity),
//!    then the world position is integrated from velocity, then time-in-state
//!    and the animation element/time advance from the AIR action frame
//!    durations.
//!
//! ## Facing-relative velocity (MUGEN semantics)
//!
//! MUGEN state-controller velocities are **facing-relative**: `+x` is the
//! direction the character faces. The engine integrates the *world* position as
//! `pos.x += vel.x * facing_sign` (facing right `+1`, left `-1`); the Y axis is
//! never mirrored. The stored `vel.x` is therefore kept facing-relative — it is
//! never mirrored at `VelSet`/`VelAdd`, and the `Vel X` trigger returns the
//! stored (facing-relative) value unchanged. Only the per-tick world-position
//! integration applies the facing sign. `PosAdd` is likewise facing-relative
//! (`pos.x += dx * facing_sign`), while `PosSet` and the `Pos X` trigger operate
//! on the **absolute** stage position (no mirroring). See
//! `Character::integrate_position` (private).
//!
//! ## Controller dispatch
//!
//! The dispatch handles the core MOVEMENT/CONTROL controllers needed to run
//! KFM's basic states. From task 5.3: `ChangeState`, `VelSet`, `VelAdd`,
//! `CtrlSet`, and `Null`. Added in task 5.4: `ChangeAnim` (and the
//! `ChangeAnim2` alias), `PosSet`, `PosAdd`, `VarSet`, `VarAdd`, `VarRangeSet`,
//! `StateTypeSet`, `Turn`, and `PlaySnd`. Task 8.3a turns `PlaySnd` into a real
//! emitter: it pushes a [`SoundRequest`] onto [`TickReport::sound_requests`] for
//! a downstream audio player to consume — `fp-character` stays a pure simulation
//! crate and produces no audio itself. Task 6.2 adds the `HitDef` controller
//! (builds a [`fp_combat::HitDef`] into [`Character::active_hitdef`]). Any other controller
//! type is a safe no-op (debug-logged) and is deferred to a later task. The
//! dispatch never panics; a malformed parameter resolves to its safe default.
//!
//! ## Get-hit state readiness (task 6.2, part C)
//!
//! The common get-hit states (`5000`–`5xxx` from `common1.cns`) are *runnable*
//! by this executor today: every standard trigger they read resolves, and
//! `GetHitVar(<member>)` now resolves against [`Character::get_hit_vars`] (it
//! previously deferred to a hard `0`). Their `ChangeState` / `ChangeAnim` /
//! `VelSet` / `PosSet` / `VarSet` controllers are all handled by the dispatch.
//!
//! Two **documented gaps** remain — neither silently mis-runs:
//!
//! 1. The get-hit-specific controllers (`HitVelSet`, `HitFallSet`, `HitFallVel`,
//!    `HitFallDamage`, `HitAdd`, `LifeAdd`, …) are not yet
//!    implemented; the dispatch routes them to its safe, **debug-logged** no-op
//!    branch (visible, not silent) and they are deferred to task 6.3+.
//!    (`SelfState` and `VelMul`, formerly in this list, are now handled —
//!    audit P3+P11.)
//! 2. [`Character::get_hit_vars`] stays at its default until hit *resolution*
//!    (task 6.3) populates it, so a get-hit state run *before* 6.3 sees zeroed
//!    hit effects. This is expected: 6.2 only wires the read path.
//!
//! [03 §3]: ../../../docs/knowledge-base/03-engine-architecture.md

use std::collections::HashMap;

use fp_core::Vec2;
use fp_formats::air::AirFile;
use fp_vm::{eval, Value};

use crate::loader::{
    CompiledController, CompiledExpr, CompiledParam, CompiledState, CompiledTriggerGroup,
};
use crate::{
    Character, EvalCtx, Facing, LoadedCharacter, MoveType, Physics, StageView, StateType,
    NUM_FVARS, NUM_VARS,
};

/// The per-tick **cross-entity evaluation environment**: the opponent's context
/// and the stage, threaded through the executor so every expression the tick
/// evaluates sees the other entity.
///
/// This is a tiny `Copy` bundle (an opponent reference plus two floats) carried
/// alongside `&mut self` through the controller dispatch chain. At each eval site
/// the executor reborrows `&*self` into a fresh
/// [`EvalCtx`](crate::EvalCtx)`{ me, opponent, stage }` that lives only for that
/// one `eval` call — the reborrow drops before any `&mut self` mutation, so the
/// borrow checker is satisfied with no `unsafe`.
///
/// The opponent context is built **once** near the top of
/// [`Character::tick_with`] (with its own opponent set to `None`), so a single
/// level of `p2, ...` resolves while the opponent's nested redirects bottom out —
/// matching MUGEN's view of the other player from a non-helper. The opponent
/// `Character` is borrowed immutably and is not mutated during this character's
/// tick.
#[derive(Clone, Copy)]
struct EvalEnv<'a> {
    /// The opponent's evaluation context, or `None` when there is no opponent.
    opponent: Option<&'a EvalCtx<'a>>,
    /// The stage edges, for the screen-edge distance triggers.
    stage: StageView,
}

impl EvalEnv<'_> {
    /// An environment with no opponent and the default stage: every
    /// opponent-dependent trigger reads the safe default `0`. Used by the
    /// out-of-tick [`Character::change_state`] seam, which has no opponent
    /// threaded.
    fn self_only() -> Self {
        Self {
            opponent: None,
            stage: StageView::default(),
        }
    }
}

/// Upper bound on `ChangeState` transitions resolved within a single tick.
///
/// MUGEN re-enters the destination state in the same tick after a
/// `ChangeState`. A buggy or cyclic state graph (`A → B → A → …`) could loop
/// forever; the executor caps the number of transitions per tick and warns when
/// the cap is hit, degrading safely rather than hanging.
const MAX_TRANSITIONS_PER_TICK: u32 = 512;

/// World Y coordinate of the floor (ground plane) a grounded player stands on.
///
/// MUGEN's world Y axis increases **downward** and the floor is `Y = 0`:
/// negative Y is *above* the ground (airborne), positive Y would be *below* the
/// floor, which a player is never allowed to reach. After integrating velocity
/// each tick the executor clamps `pos.y` to this value
/// ([`Character::integrate_position`]) so a falling character settles on the
/// ground instead of sinking, letting the data-driven land transition in
/// `common1` (air [Statedef 50] checks `Vel Y > 0 && Pos Y >= 0` to
/// `ChangeState` to the Jump Land state 52) fire and complete the
/// jump → land → stand loop.
///
/// Kept as a named constant so a future per-stage floor / `zoffset` can override
/// it without hunting for a magic literal.
const GROUND_Y: f32 = 0.0;

/// State number of common1's **AirJump Start** state ([Statedef 45]).
///
/// MUGEN's air-jump (double jump) is triggered by an *engine built-in*, not by a
/// CNS controller: when the player presses up again in the air (with control,
/// below the air-jump limit and above the air-jump height) the engine changes the
/// character into state 45. The built-in here ([`Character::update_air_jump`])
/// performs ONLY that engine-side transition into 45; the character's `common1`
/// `[Statedef 45]` is what sets the air-jump velocity from
/// `const(velocity.airjump.*)` and then proceeds to the jump-up state 50. Kept as
/// a named constant so the magic state number has a single documented home.
const AIRJUMP_START_STATE: i32 = 45;

/// A request to play one sound, emitted by a `PlaySnd` controller during a tick.
///
/// `fp-character` is a *pure simulation* crate: it never touches the audio
/// device or any file format. Instead, each `PlaySnd` that fires pushes a
/// [`SoundRequest`] onto [`TickReport::sound_requests`], and a downstream player
/// (the `fp-audio` mixer in Phase 8) consumes the report and performs the actual
/// playback. This keeps the executor dependency-free and deterministic.
///
/// The fields mirror MUGEN's `PlaySnd` parameters. The `value` parameter is a
/// `group, sample` pair into the character's `.snd` file (or the common/fight
/// sound file when the group token is `F`-prefixed — see [`common`]).
///
/// [`common`]: SoundRequest::common
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundRequest {
    /// The sound *group* number (the first half of the `value` pair). When the
    /// authored group token carried a leading `F` flag it is stripped before
    /// parsing and [`common`](SoundRequest::common) is set; the integer stored
    /// here is the group number with that flag removed.
    pub group: i32,
    /// The sound *sample* number within [`group`](SoundRequest::group) (the
    /// second half of the `value` pair).
    pub sample: i32,
    /// The playback channel. MUGEN's `PlaySnd` default is `-1` ("play on the
    /// next free channel"); channel `0` is the reserved voice channel that only
    /// holds one sound at a time. See the `PlaySnd` controller's defaults for
    /// the full rationale.
    pub channel: i32,
    /// Output volume scale as a percentage (MUGEN's `volumescale`). Defaults to
    /// `100` (unattenuated) when the parameter is absent.
    pub volume_scale: i32,
    /// Whether the sound should loop. Set from the `loop` parameter
    /// (`1`/`-1`/`true` → looping); defaults to `false`.
    pub looping: bool,
    /// `true` when the `PlaySnd` group token was `F`-prefixed, meaning the sound
    /// comes from the **common / fight** sound file (`fight.snd`) rather than the
    /// character's own `.snd`. An `S` or any other (or no) leading letter leaves
    /// this `false` (the character's own `.snd`).
    pub common: bool,
}

/// A deferred operation a `Target*` controller wants applied to this
/// character's **target** (the opponent it established a hit on).
///
/// `fp-character` ticks one character at a time and only ever borrows the
/// opponent immutably, so a `Target*` controller (which must *mutate* the
/// opponent) cannot apply its effect inline. Instead it pushes the matching
/// `TargetOp` onto [`TickReport::target_ops`] — exactly mirroring how `PlaySnd`
/// defers a [`SoundRequest`] — and a downstream owner of both characters
/// (`fp-engine`, task P8b) applies each op to the opponent after the tick. This
/// keeps the executor single-entity, deterministic, and panic-free.
///
/// Each variant carries the parameters of its MUGEN controller. Velocity /
/// position fields are `(x, y)` pairs in MUGEN's facing-relative convention; the
/// applier (`fp-engine`) is responsible for any facing mirroring, just as
/// `fp-character` does for its own velocity/position controllers.
#[derive(Debug, Clone, PartialEq)]
pub enum TargetOp {
    /// `TargetState`: force the target into the given state number (`value`).
    ///
    /// Used by throws to drive the victim through the thrown-animation states
    /// (KFM state 820). The applier performs the target's state entry.
    State(i32),
    /// `TargetBind`: hold the target at a position relative to this character for
    /// `time` ticks. `pos` is the `(x, y)` offset (`TargetBind`'s `pos` param);
    /// `time` is the bind duration in ticks (MUGEN default `1`).
    ///
    /// Used by throws to pin the victim to the thrower each tick (KFM state 810).
    Bind {
        /// Bind duration in ticks (`TargetBind`'s `time`).
        time: i32,
        /// `(x, y)` bind offset relative to this character (`TargetBind`'s `pos`).
        pos: (f32, f32),
    },
    /// `TargetLifeAdd`: add `value` to the target's life (negative = damage).
    /// `kill` mirrors MUGEN's `kill` flag: when `false`, the target's life is
    /// floored at `1` (the hit cannot be lethal); when `true` it may reach `0`.
    ///
    /// Used by throws to apply the throw damage to the victim (KFM state 810).
    LifeAdd {
        /// Amount added to the target's life (negative subtracts / damages).
        value: i32,
        /// Whether this add may reduce the target to `0` life (`true`) or must
        /// leave at least `1` (`false`).
        kill: bool,
    },
    /// `TargetFacing`: set the target's facing relative to this character.
    /// `1` = the target faces the **same** way as this character, `-1` = the
    /// **opposite** way (MUGEN's `TargetFacing` value convention).
    ///
    /// Used by throws to orient the victim toward the thrower (KFM state 810).
    Facing(i32),
    /// `TargetVelSet`: set the target's velocity to `(x, y)` (`TargetVelSet`).
    VelSet((f32, f32)),
    /// `TargetVelAdd`: add `(x, y)` to the target's velocity (`TargetVelAdd`).
    VelAdd((f32, f32)),
    /// `TargetPowerAdd`: add `value` to the target's power / super meter
    /// (`TargetPowerAdd`). The applier clamps the result into the target's
    /// `[0, power_max]` range.
    PowerAdd(i32),
}

/// A summary of what one [`Character::tick`] did, returned for diagnostics and
/// tests.
///
/// All counters are best-effort and never affect gameplay; they exist so a
/// caller (or a test) can assert that the expected work happened without
/// reaching into private executor state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TickReport {
    /// Number of controllers whose dispatch ran (gating passed and `persistent`
    /// allowed it to fire) this tick.
    pub controllers_fired: u32,
    /// Number of `ChangeState` transitions performed this tick.
    pub transitions: u32,
    /// `true` if the per-tick transition cap was hit and processing was stopped
    /// early to avoid an infinite loop.
    pub transition_cap_hit: bool,
    /// `true` if the character was frozen by hit-pause this tick: normal state
    /// and physics processing was skipped and the pause counter decremented. No
    /// controllers fire and no transitions happen on a hit-paused tick.
    pub hitpaused: bool,
    /// Sound-play requests emitted by `PlaySnd` controllers this tick, in fire
    /// order. Empty on a tick with no `PlaySnd` (a fresh [`TickReport`] is built
    /// per tick, so this never carries requests across ticks). Consumed by the
    /// downstream audio player; `fp-character` itself produces no sound.
    pub sound_requests: Vec<SoundRequest>,
    /// Deferred operations emitted by `Target*` controllers this tick, in fire
    /// order, to be applied to this character's target (the opponent). Empty on a
    /// tick with no firing `Target*` controller, and (like `sound_requests`)
    /// never carried across ticks because a fresh [`TickReport`] is built per
    /// tick. `fp-character` only *describes* these; a downstream owner of both
    /// characters (`fp-engine`, task P8b) applies each [`TargetOp`] to the
    /// opponent after the tick.
    pub target_ops: Vec<TargetOp>,
}

impl Character {
    /// Advances this character by one 60Hz tick against its loaded state graph.
    ///
    /// Processes the special states `-3`, `-2`, `-1` and then the current state,
    /// gating each controller on `triggerall` (AND) and the numbered trigger
    /// groups (OR, with CB6 contiguity), honoring `persistent`/`ignorehitpause`,
    /// performing state entry and `ChangeState` transitions, applying the
    /// statedef `physics`, and advancing time-in-state and the animation cursor.
    ///
    /// Returns a [`TickReport`] describing what happened. Never panics: unknown
    /// states and controllers degrade to safe no-ops, and a cyclic state graph
    /// is bounded by an internal per-tick transition cap (`512`), after which
    /// processing stops and [`TickReport::transition_cap_hit`] is set.
    ///
    /// `opponent` is the other player this character's triggers can read across
    /// (`P2Dist`, `p2, life`, …); pass `None` for a single-character / no-opponent
    /// tick, in which case the opponent-dependent triggers report the safe default
    /// `0`. `stage` supplies the screen edges the screen-edge distance triggers
    /// (`FrontEdgeDist`, …) read; use [`StageView::default`] when there is no
    /// stage context.
    pub fn tick(
        &mut self,
        loaded: &LoadedCharacter,
        opponent: Option<&Character>,
        stage: StageView,
    ) -> TickReport {
        self.tick_with(&loaded.states, &loaded.air, opponent, stage)
    }

    /// The executor core, parameterized over just the data it needs: the
    /// compiled state graph and the animation set.
    ///
    /// [`Character::tick`] is the public entry point and delegates here. This
    /// split keeps the executor independent of the (binary-only)
    /// [`SffFile`](fp_formats::sff::SffFile), so unit tests can drive the state
    /// machine from a hand-built state map and AIR file without a sprite asset.
    pub fn tick_with(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        air: &AirFile,
        opponent: Option<&Character>,
        stage: StageView,
    ) -> TickReport {
        let mut report = TickReport::default();

        // Build the opponent's cross-entity context ONCE for this whole tick. Its
        // own opponent is `None` (a single level of `p2, ...` is enough for KFM /
        // common1; the opponent's nested redirects bottom out). The opponent is
        // borrowed immutably here and is never mutated during this character's
        // tick, so this shared borrow coexists with `&mut self` below: each eval
        // site reborrows `&*self` into a short-lived `EvalCtx` that drops before
        // any mutation.
        let opp_ctx: Option<EvalCtx> =
            opponent.map(|o| EvalCtx::new(o, None, stage));
        let env = EvalEnv {
            opponent: opp_ctx.as_ref(),
            stage,
        };

        // Hit-pause gate (task 6.5): while frozen by a connecting hit, the engine
        // holds the character still for the paused tick — it does NOT advance the
        // animation, the state `Time` counter, or physics (velocity/position is not
        // integrated). The ONLY controllers permitted to run during the freeze are
        // those flagged `ignorehitpause`; a controller without that flag is skipped
        // for the duration of the pause. The pause counter is decremented by one
        // each frozen tick, so a freshly-set `hitpause = N` lasts exactly N ticks
        // and normal advancement resumes on the tick it reaches 0.
        //
        // SIMPLIFICATION (deferred, tracked as CB30): we model the freeze as a
        // single symmetric per-character pause. MUGEN's finer distinction between
        // the attacker's `hitpause` and the defender's `hitshake` — and the precise
        // shake-then-knockback timing nuance — is not modeled; both participants
        // simply freeze for their respective `pausetime` and the defender's shake
        // timer counts down alongside. The shake timer (the defender's visual
        // jitter during the pause) is decremented in lockstep with the pause.
        if self.hitpause > 0 {
            self.hitpause -= 1;
            if self.shaketime > 0 {
                self.shaketime -= 1;
            }
            report.hitpaused = true;
            // Decrement the NotHitBy/HitBy invuln windows. While hit-paused, only
            // slots flagged `ignorehitpause` keep counting (the others freeze,
            // like every other per-tick timer) — passing `hitpaused = true`.
            self.invuln.tick(true);
            // Run ONLY the `ignorehitpause`-flagged controllers, in the same
            // special-state-then-current-state order as a normal tick. Everything
            // else (anim/time/physics advance, non-flagged controllers) is frozen.
            self.run_ignorehitpause_only(states, env, &mut report);
            return report;
        }
        if self.shaketime > 0 {
            self.shaketime -= 1;
        }
        // Not hit-paused: both invuln slots count down this tick.
        self.invuln.tick(false);

        // Process the special states first, in MUGEN order, then the current
        // state. The current state number is re-read after each special state in
        // case one of them changed it via ChangeState.
        for special in [-3, -2, -1] {
            self.run_state(states, special, env, &mut report);
        }

        // Then the current numbered state. ChangeState within it re-enters the
        // destination in the same tick (bounded by run_current_with_transitions).
        self.run_current_with_transitions(states, env, &mut report);

        // ---- Air-jump (double jump) engine built-in (audit P14) -------------
        // Runs AFTER the authored states so a character's own `[State -1]`
        // specials/attacks keep priority: this is an engine fallback, exactly
        // like the locomotion / auto-land built-ins. It tracks an air-jump count
        // and the rising edge of `holdup` on `Character`, both of which CNS
        // controllers cannot express, so it lives here in Rust rather than as a
        // loader-injected controller.
        self.update_air_jump(states, &mut report);

        // ---- Per-tick physics, integration, time, and animation advance -----
        // MUGEN order: controllers set velocity, then `physics` modifies it
        // (friction/gravity), then the world position is integrated from the
        // (facing-relative) velocity, then time/animation advance.
        self.apply_physics();
        self.integrate_position();
        self.advance_time();
        self.advance_animation(air);

        report
    }

    /// Forces this character into `target` exactly as a `ChangeState` would:
    /// records `prev_state_no`, resets time-in-state and the `persistent`
    /// bookkeeping, and applies the destination statedef's entry parameters
    /// (`type`/`movetype`/`physics`/`anim`/`ctrl`/`velset`).
    ///
    /// This is the public seam hit resolution ([`resolve_attack`](crate::resolve_attack))
    /// uses to put a defender into its get-hit state. An unknown destination
    /// still updates the cursor (so `StateNo` reads the requested number) but
    /// applies no entry parameters — never panics.
    ///
    /// The destination statedef's entry expressions (`anim`/`ctrl`/`poweradd`) are
    /// evaluated with **no opponent** in view and the default stage: this seam is
    /// used outside the per-tick eval loop (it has no opponent threaded), and the
    /// entry expressions KFM / common1 use here are self-only. A cross-entity
    /// entry expression would read the safe default `0` rather than misfire.
    pub fn change_state(&mut self, states: &HashMap<i32, CompiledState>, target: i32) {
        self.enter_state(states, target, EvalEnv::self_only());
    }

    /// The MUGEN **air-jump** (double / multi jump) engine built-in
    /// (faithfulness audit P14): grounded reset, fresh-up-press edge detection,
    /// and the engine-side transition into the AirJump Start state
    /// ([`AIRJUMP_START_STATE`], common1's [Statedef 45]).
    ///
    /// Called once per tick from [`tick_with`](Self::tick_with), **after** the
    /// authored states have run, so a character's own specials/attacks keep
    /// priority (this is an engine fallback, like the locomotion / auto-land
    /// built-ins). Air-jump is **not** expressible as a CNS controller because it
    /// needs engine state — an air-jump *count* and *rising-edge* detection of the
    /// up direction — so it is implemented here in Rust.
    ///
    /// # Behavior
    ///
    /// 1. **Grounded reset.** Whenever the character is not airborne
    ///    (`state_type != StateType::Air`) the air-jump count is reset to `0`.
    ///    This is the faithful reset point: a fresh ground jump (which only leaves
    ///    the ground by entering an `A`-type state) therefore always starts with
    ///    the full allowance, so the canonical *jump → air-jump → land → jump →
    ///    air-jump* sequence works repeatedly.
    /// 2. **Fresh up-press.** The up direction (`holdup`) is *held*, not edged, by
    ///    the command source. Firing on the held state would burn every air jump
    ///    on consecutive frames, so the built-in computes a rising edge
    ///    `holdup_active && !up_held_prev` and only that fresh press qualifies.
    ///    The current `holdup` active state is stored in
    ///    [`up_held_prev`](Character::up_held_prev) for the next tick **every**
    ///    tick (even when no air jump fires), so the edge tracks correctly.
    /// 3. **Air-jump transition.** When the character is airborne, has control,
    ///    has a fresh up-press, has not used up its allowance
    ///    (`air_jump_count < airjump_num`), and is high enough above the floor
    ///    (`pos.y <= -airjump_height`, since the floor is `Y = 0` and up is
    ///    negative-Y), it changes to [`AIRJUMP_START_STATE`] and increments the
    ///    count. A character whose `airjump_num` is `0` never air-jumps (the whole
    ///    built-in is gated on `airjump_num > 0`).
    ///
    /// The `holdup` command name is queried case-insensitively through the
    /// character's [`CommandSource`]; with the default [`NoCommands`] source it is
    /// never active and the built-in is a safe no-op. Never panics.
    fn update_air_jump(&mut self, states: &HashMap<i32, CompiledState>, report: &mut TickReport) {
        // (1) Grounded reset: any non-air state restores the full allowance.
        if self.state_type != StateType::Air {
            self.air_jump_count = 0;
        }

        // (2) Fresh up-press = rising edge of the held `holdup` direction.
        let up_active = self.commands.is_active("holdup");
        let fresh_up_press = up_active && !self.up_held_prev;
        // Record the current held state for next tick's edge regardless of
        // whether an air jump fires.
        self.up_held_prev = up_active;

        // A character with no air-jump allowance never air-jumps.
        let airjump_num = self.constants.movement.airjump_num;
        if airjump_num <= 0 {
            return;
        }

        // (3) Air-jump transition gate.
        let airborne = self.state_type == StateType::Air;
        // Up is negative-Y and the floor is `GROUND_Y` (0); the character is high
        // enough when it has risen at least `airjump_height` above the floor.
        let high_enough = self.pos.y <= GROUND_Y - self.constants.movement.airjump_height;
        if airborne
            && self.ctrl
            && fresh_up_press
            && self.air_jump_count < airjump_num
            && high_enough
        {
            self.air_jump_count += 1;
            self.enter_state(states, AIRJUMP_START_STATE, EvalEnv::self_only());
            // Count this engine-side transition like a `ChangeState`.
            report.transitions += 1;
        }
    }

    /// Runs every controller of the state numbered `state_no` (if it exists),
    /// in file order, applying gating and `persistent` semantics. Used for the
    /// special states `-3`/`-2`/`-1`, which do not themselves transition the
    /// current numbered state but may `ChangeState` it.
    fn run_state(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        state_no: i32,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        let Some(state) = states.get(&state_no) else {
            // No such special state (e.g. a character without a [Statedef -3]):
            // nothing to do, never an error.
            return;
        };
        // MUGEN scans a command state (-1, and likewise -2/-3) top-down and stops
        // at the first controller that changes the current numbered state: the
        // first matching `ChangeState` wins and the remaining controllers are
        // skipped this tick. This is what gives a character's authored `[State -1]`
        // specials/run/attacks priority over the engine's built-in locomotion
        // controllers appended after them (task 7.3) — once a special's
        // `ChangeState` fires, the built-ins below it never run. Mirrors the same
        // break in `run_current_with_transitions`.
        let entry_state = self.state_no;
        let num = state.controllers.len();
        for idx in 0..num {
            if self.state_no != entry_state {
                break;
            }
            // Re-fetch the state each iteration: the special-state list itself is
            // stable, but defensively re-borrow in case a controller mutated it.
            let Some(state) = states.get(&state_no) else {
                return;
            };
            let Some(ctrl) = state.controllers.get(idx) else {
                return;
            };
            let ctrl = ctrl.clone();
            self.run_controller(states, &ctrl, idx, env, report);
        }
    }

    /// Runs the current numbered state's controllers, following `ChangeState`
    /// transitions within the same tick up to `MAX_TRANSITIONS_PER_TICK`.
    fn run_current_with_transitions(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        let mut guard = 0u32;
        loop {
            let current = self.state_no;
            let transitions_before = report.transitions;

            let Some(state) = states.get(&current) else {
                // Unknown current state: degrade safely (warn once per tick).
                tracing::debug!("tick: current state {current} not found; skipping controllers");
                return;
            };
            let num = state.controllers.len();

            for idx in 0..num {
                // The state can change mid-list; stop processing the old state's
                // remaining controllers once a transition has fired.
                if self.state_no != current {
                    break;
                }
                let Some(state) = states.get(&current) else {
                    break;
                };
                let Some(ctrl) = state.controllers.get(idx) else {
                    break;
                };
                let ctrl = ctrl.clone();
                self.run_controller(states, &ctrl, idx, env, report);
            }

            // We are done with this state unless a ChangeState moved us to a
            // *different* numbered state, in which case we re-enter the loop to
            // process the destination this same tick. A self-transition
            // (ChangeState into `current`) leaves `state_no == current` and so
            // also exits here — correct, since looping on it would never settle.
            //
            // The earlier `report.transitions == transitions_before` clause was
            // redundant: a no-transition pass cannot change `state_no`, so
            // "no transition" always implies `state_no == current`. The
            // debug_assert below pins that transition-count invariant — we only
            // fall through to loop again when at least one real transition (to a
            // different state) was counted this iteration, so the per-tick guard
            // counts genuine transitions.
            if self.state_no == current {
                return;
            }
            debug_assert!(
                report.transitions > transitions_before,
                "looping requires a counted transition; state_no moved {current} -> {} \
                 but transitions did not advance ({transitions_before})",
                self.state_no
            );

            guard += 1;
            if guard >= MAX_TRANSITIONS_PER_TICK {
                tracing::warn!(
                    "tick: transition cap ({MAX_TRANSITIONS_PER_TICK}) hit at state {}; \
                     stopping to avoid an infinite loop",
                    self.state_no
                );
                report.transition_cap_hit = true;
                return;
            }
        }
    }

    /// Runs **only** the controllers flagged `ignorehitpause` during a hit-pause
    /// freeze, in MUGEN's special-state-then-current-state order (`-3`, `-2`,
    /// `-1`, then the current numbered state).
    ///
    /// This is the hit-pause exception (task 6.5): a paused character is otherwise
    /// completely frozen (no anim/time/physics advance, no normal controllers),
    /// but a controller that asserts `ignorehitpause = 1` still evaluates its
    /// triggers and dispatches if it qualifies. The same gating and `persistent`
    /// semantics as a normal tick apply; the only difference is the
    /// [`Self::ignorehitpause_flag`] pre-filter that skips every non-flagged
    /// controller.
    ///
    /// Unlike a normal tick, this does **not** follow `ChangeState` re-entry
    /// across states within the frozen tick: a hit-paused character should not be
    /// driving its own state transitions, and the dispatch of a `ChangeState`
    /// (should an `ignorehitpause` controller carry one) still updates the cursor —
    /// but we do not re-process the destination this frozen tick. Each special
    /// state and the current state are scanned once, top-to-bottom. Never panics.
    fn run_ignorehitpause_only(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        for state_no in [-3, -2, -1, self.state_no] {
            let Some(state) = states.get(&state_no) else {
                continue;
            };
            let num = state.controllers.len();
            for idx in 0..num {
                let Some(state) = states.get(&state_no) else {
                    break;
                };
                let Some(ctrl) = state.controllers.get(idx) else {
                    break;
                };
                let ctrl = ctrl.clone();
                if !self.ignorehitpause_flag(&ctrl, env) {
                    // A controller without `ignorehitpause` is skipped for the
                    // duration of the freeze.
                    continue;
                }
                self.run_controller(states, &ctrl, idx, env, report);
            }
        }
    }

    /// Evaluates a controller's `ignorehitpause` universal parameter.
    ///
    /// Returns `true` only when the controller carries an `ignorehitpause`
    /// expression that evaluates to a non-zero (truthy) value. A controller with
    /// no `ignorehitpause` line, or one whose expression evaluates to `0`,
    /// returns `false` — matching MUGEN's default of `0` (the controller is paused
    /// during hit-pause). A fallback (failed-compile) expression evaluates to `0`
    /// and so returns `false`. Never panics.
    fn ignorehitpause_flag(&self, ctrl: &CompiledController, env: EvalEnv) -> bool {
        match &ctrl.ignorehitpause {
            Some(expr) => self.eval_bool(expr, env),
            None => false,
        }
    }

    /// Evaluates one controller's gating and `persistent` policy and, if it
    /// qualifies to fire this tick, dispatches it.
    fn run_controller(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        ctrl: &CompiledController,
        idx: usize,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        if !self.gating_passes(ctrl, env) {
            return;
        }

        // The controller qualified (gating passed). Apply `persistent` to decide
        // whether it actually fires on this qualifying tick.
        //
        // Key the firing count by the controller's OWNING state number
        // (`ctrl.state_number`), not the live `self.state_no`. While a special
        // state (-3/-2/-1) runs, `self.state_no` is still the *current* numbered
        // state, so keying by it would make a special-state controller and a
        // current-state controller that share an index collide on one persistent
        // count. Keying by the owning state number keeps each controller's
        // per-entry count independent. (The full `(state_number, idx)` pair is
        // still needed because two controllers in the same state share a
        // state_number but differ by index.)
        let key = (ctrl.state_number, idx);
        let qualifying_count = self.fire_counts.entry(key).or_insert(0);
        *qualifying_count += 1;
        let count = *qualifying_count;

        if !persistent_allows(self.persistent_value(ctrl, env), count) {
            return;
        }

        report.controllers_fired += 1;
        self.dispatch(states, ctrl, env, report);
    }

    /// Returns `true` if the controller's gating passes: all `triggerall`
    /// conditions are true (AND) **and** at least one numbered trigger group is
    /// fully true (OR across groups), after applying the CB6 contiguity rule.
    fn gating_passes(&self, ctrl: &CompiledController, env: EvalEnv) -> bool {
        // triggerall: every condition must be true.
        for cond in &ctrl.triggerall {
            if !self.eval_bool(cond, env) {
                return false;
            }
        }

        // No numbered groups at all: MUGEN requires at least one trigger1, so a
        // controller with only triggerall (and no trigger1) does not fire.
        if ctrl.triggers.is_empty() {
            return false;
        }

        // CB6: consider groups in ascending number from 1, stopping at the first
        // gap. A controller fires if any *contiguous* group is fully true.
        for group in contiguous_groups(&ctrl.triggers) {
            if self.group_is_true(group, env) {
                return true;
            }
        }
        false
    }

    /// Returns `true` if every condition in a numbered group is true (AND).
    fn group_is_true(&self, group: &CompiledTriggerGroup, env: EvalEnv) -> bool {
        // An empty group (no conditions) cannot be satisfied.
        !group.conditions.is_empty() && group.conditions.iter().all(|c| self.eval_bool(c, env))
    }

    /// Builds the per-eval cross-entity context: this character viewed together
    /// with `env`'s opponent and stage.
    ///
    /// The returned [`EvalCtx`] reborrows `&*self` (`me`) and lives only as long
    /// as the caller keeps it — every eval helper builds one, runs a single
    /// [`eval`], and drops it before any `&mut self` mutation. The opponent
    /// reference comes from `env` (built once for the whole tick), so this is a
    /// cheap reborrow + struct build, no allocation and no `unsafe`.
    fn eval_ctx<'a>(&'a self, env: EvalEnv<'a>) -> EvalCtx<'a> {
        EvalCtx::new(self, env.opponent, env.stage)
    }

    /// Evaluates a compiled expression against this character (with its opponent /
    /// stage view) as a boolean.
    ///
    /// A fallback (const-`0`) expression is always false, so a controller whose
    /// trigger failed to compile can never fire.
    fn eval_bool(&self, expr: &CompiledExpr, env: EvalEnv) -> bool {
        let ctx = self.eval_ctx(env);
        eval(&expr.expr, &ctx).as_bool()
    }

    /// Evaluates a compiled expression to a [`Value`] against this character (with
    /// its opponent / stage view).
    fn eval_value(&self, expr: &CompiledExpr, env: EvalEnv) -> Value {
        let ctx = self.eval_ctx(env);
        eval(&expr.expr, &ctx)
    }

    /// Evaluates component `i` of a multi-component parameter, returning `None`
    /// when the parameter has no such component.
    ///
    /// This is the component accessor every controller uses to read a parameter:
    /// a scalar parameter is read with `i == 0`; the second value of an `x, y`
    /// pair is read with `i == 1`. A missing component returns `None` so the
    /// caller can substitute its own documented default. Never panics.
    fn eval_param_component(
        &self,
        param: &CompiledParam,
        i: usize,
        env: EvalEnv,
    ) -> Option<Value> {
        param.component(i).map(|expr| self.eval_value(expr, env))
    }

    /// Evaluates a parameter's scalar value: its first (index `0`) component.
    ///
    /// Most controllers read a single value (`value`, `x`, `y`, …); this is the
    /// shorthand for `eval_param_component(param, 0)`. Returns `None` only for
    /// the (in practice impossible) empty parameter.
    fn eval_param(&self, param: &CompiledParam, env: EvalEnv) -> Option<Value> {
        self.eval_param_component(param, 0, env)
    }

    /// Evaluates every component of a parameter, in order, into [`Value`]s.
    ///
    /// Replaces the old `eval_components` raw-source re-split: the loader already
    /// split the parameter on top-level commas and compiled each component, so
    /// this simply evaluates the pre-compiled components against `self`. An empty
    /// or whitespace-only authored component is the const-`0` fallback and
    /// evaluates to `0`. Never panics.
    fn eval_param_components(&self, param: &CompiledParam, env: EvalEnv) -> Vec<Value> {
        param
            .components
            .iter()
            .map(|expr| self.eval_value(expr, env))
            .collect()
    }

    /// Resolves the controller's `persistent` value: the compiled expression if
    /// present, otherwise MUGEN's default of `1` (re-fire every qualifying tick).
    fn persistent_value(&self, ctrl: &CompiledController, env: EvalEnv) -> i32 {
        match &ctrl.persistent {
            Some(expr) => self.eval_value(expr, env).to_int(),
            None => 1,
        }
    }

    /// Dispatches a controller that has qualified to fire this tick.
    ///
    /// Handles the core MOVEMENT/CONTROL controllers: `ChangeState`, `VelSet`,
    /// `VelAdd`, `CtrlSet`, `Null` (task 5.3) plus `ChangeAnim`/`ChangeAnim2`,
    /// `PosSet`, `PosAdd`, `VarSet`, `VarAdd`, `VarRangeSet`, `StateTypeSet`,
    /// `Turn`, and `PlaySnd` (task 5.4; 8.3a makes `PlaySnd` emit a
    /// [`SoundRequest`]). Task 6.2 adds the `HitDef` controller, which builds a
    /// [`fp_combat::HitDef`] into [`active_hitdef`](Character::active_hitdef).
    /// Task 6.6 adds `PowerAdd`/`PowerSet`, which mutate the super meter
    /// (clamped to `[0, power_max]`). Audit P3+P11 adds `SelfState` (a
    /// self-`ChangeState` in this model; see [`ctrl_self_state`](Self::ctrl_self_state))
    /// and `VelMul` (component-wise velocity multiply). Audit P8a adds the
    /// `Target*` controllers (`TargetState`, `TargetBind`, `TargetLifeAdd`,
    /// `TargetFacing`, `TargetVelSet`, `TargetVelAdd`, `TargetPowerAdd`), which —
    /// like `PlaySnd` — *defer* their effect by pushing a [`TargetOp`] onto
    /// [`TickReport::target_ops`] for a downstream applier (`fp-engine`); they are
    /// safe no-ops when this character has no target. Audit P9 adds `NotHitBy` /
    /// `HitBy`, which set the character's attack-attribute invulnerability slots
    /// (see [`ctrl_invuln`](Self::ctrl_invuln) and [`crate::invuln`]).
    /// Every other type is a safe no-op, debug-logged and deferred to a later task.
    fn dispatch(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        ctrl: &CompiledController,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        let kind = ctrl.controller_type.as_deref().unwrap_or("");
        if kind.eq_ignore_ascii_case("ChangeState") {
            self.ctrl_change_state(states, ctrl, env, report);
        } else if kind.eq_ignore_ascii_case("SelfState") {
            self.ctrl_self_state(states, ctrl, env, report);
        } else if kind.eq_ignore_ascii_case("VelSet") {
            self.ctrl_vel_set(ctrl, env);
        } else if kind.eq_ignore_ascii_case("VelAdd") {
            self.ctrl_vel_add(ctrl, env);
        } else if kind.eq_ignore_ascii_case("VelMul") {
            self.ctrl_vel_mul(ctrl, env);
        } else if kind.eq_ignore_ascii_case("CtrlSet") {
            self.ctrl_ctrl_set(ctrl, env);
        } else if kind.eq_ignore_ascii_case("PosSet") {
            self.ctrl_pos_set(ctrl, env);
        } else if kind.eq_ignore_ascii_case("PosAdd") {
            self.ctrl_pos_add(ctrl, env);
        } else if kind.eq_ignore_ascii_case("ChangeAnim")
            || kind.eq_ignore_ascii_case("ChangeAnim2")
        {
            // ChangeAnim2 aliases ChangeAnim here. (In MUGEN, ChangeAnim2 selects
            // the *opponent's* anim table during a custom-state throw; with a
            // single entity there is no distinct table yet, so it behaves as
            // ChangeAnim.)
            self.ctrl_change_anim(ctrl, env);
        } else if kind.eq_ignore_ascii_case("VarSet") {
            self.ctrl_var_set(ctrl, env);
        } else if kind.eq_ignore_ascii_case("VarAdd") {
            self.ctrl_var_add(ctrl, env);
        } else if kind.eq_ignore_ascii_case("VarRangeSet") {
            self.ctrl_var_range_set(ctrl, env);
        } else if kind.eq_ignore_ascii_case("PowerAdd") {
            self.ctrl_power_add(ctrl, env);
        } else if kind.eq_ignore_ascii_case("PowerSet") {
            self.ctrl_power_set(ctrl, env);
        } else if kind.eq_ignore_ascii_case("AttackMulSet") {
            self.ctrl_attack_mul_set(ctrl, env);
        } else if kind.eq_ignore_ascii_case("DefenceMulSet") {
            self.ctrl_defence_mul_set(ctrl, env);
        } else if kind.eq_ignore_ascii_case("StateTypeSet") {
            self.ctrl_state_type_set(ctrl);
        } else if kind.eq_ignore_ascii_case("Turn") {
            self.ctrl_turn();
        } else if kind.eq_ignore_ascii_case("PlaySnd") {
            self.ctrl_play_snd(ctrl, env, report);
        } else if kind.eq_ignore_ascii_case("HitDef") {
            self.ctrl_hit_def(ctrl, env);
        } else if kind.eq_ignore_ascii_case("NotHitBy") {
            self.ctrl_invuln(ctrl, env, crate::invuln::InvulnMode::NotHitBy);
        } else if kind.eq_ignore_ascii_case("HitBy") {
            self.ctrl_invuln(ctrl, env, crate::invuln::InvulnMode::HitBy);
        } else if kind.eq_ignore_ascii_case("TargetState") {
            self.ctrl_target_state(ctrl, env, report);
        } else if kind.eq_ignore_ascii_case("TargetBind") {
            self.ctrl_target_bind(ctrl, env, report);
        } else if kind.eq_ignore_ascii_case("TargetLifeAdd") {
            self.ctrl_target_life_add(ctrl, env, report);
        } else if kind.eq_ignore_ascii_case("TargetFacing") {
            self.ctrl_target_facing(ctrl, env, report);
        } else if kind.eq_ignore_ascii_case("TargetVelSet") {
            self.ctrl_target_vel_set(ctrl, env, report);
        } else if kind.eq_ignore_ascii_case("TargetVelAdd") {
            self.ctrl_target_vel_add(ctrl, env, report);
        } else if kind.eq_ignore_ascii_case("TargetPowerAdd") {
            self.ctrl_target_power_add(ctrl, env, report);
        } else if kind.eq_ignore_ascii_case("Null") {
            // Null intentionally does nothing.
        } else {
            // Unrecognized in this task → safe no-op, deferred to a later task.
            tracing::debug!(
                "tick: unhandled controller type {kind:?} in state {} (deferred)",
                ctrl.state_number
            );
        }
    }

    // ---- Controller implementations ---------------------------------------

    /// `ChangeState`: transition to the state named by the `value` parameter,
    /// performing state entry. Optionally sets `ctrl` if the controller carries
    /// a `ctrl` parameter.
    fn ctrl_change_state(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        ctrl: &CompiledController,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) else {
            tracing::debug!(
                "tick: ChangeState in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        let target = value.to_int();
        // A self-transition still counts as a re-entry in MUGEN (resets time).
        self.enter_state(states, target, env);
        report.transitions += 1;

        // ChangeState's optional `ctrl` parameter overrides the statedef ctrl.
        if let Some(ctrl_val) = ctrl.params.get("ctrl").and_then(|p| self.eval_param(p, env)) {
            self.ctrl = ctrl_val.as_bool();
        }
    }

    /// `SelfState`: change the character back to one of its **own** states,
    /// named by the `value` parameter, performing state entry exactly as
    /// [`ctrl_change_state`](Self::ctrl_change_state) does (the destination
    /// statedef header — `anim`/`ctrl`/`poweradd`/`velset` — applies and the
    /// optional `ctrl`/`anim`-bearing entry path runs through the same
    /// opponent-aware [`EvalEnv`]). The optional `ctrl` parameter overrides the
    /// destination statedef's control flag, just like `ChangeState`.
    ///
    /// In MUGEN, `SelfState` differs from `ChangeState` only when the player is
    /// currently in a *custom state* imposed by an opponent (e.g. mid-throw via
    /// `TargetState`): `SelfState` returns control of the state machine to the
    /// player's OWN states, whereas `ChangeState` would change the state *within*
    /// the opponent's custom-state table. We do **not** yet model custom-state
    /// ownership (there are no throws/`TargetState` in this flat 1-v-1 model), so
    /// here `SelfState` is exactly a self-`ChangeState`. The own-vs-custom-state
    /// distinction is intentionally deferred to the throw/`TargetState` work
    /// (faithfulness audit P8); when that lands, this controller must instead
    /// detach from the opponent's state table before entering `value`.
    fn ctrl_self_state(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        ctrl: &CompiledController,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        // Identical mechanics to ChangeState in this model: value + optional
        // ctrl override, via the shared enter_state path (so the destination
        // statedef's entry anim/ctrl/etc. apply). The only future divergence is
        // detaching from an opponent-imposed custom state, which is deferred.
        self.ctrl_change_state(states, ctrl, env, report);
    }

    /// `VelMul`: multiply the current velocity component-wise by the `x`/`y`
    /// parameters (`vel.x *= x`, `vel.y *= y`). An **absent** axis multiplies by
    /// `1.0` (that component is left unchanged), matching MUGEN. A missing or
    /// garbage value is a safe no-op for that axis; this never panics.
    fn ctrl_vel_mul(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        if let Some(v) = ctrl.params.get("x").and_then(|p| self.eval_param(p, env)) {
            self.vel.x *= v.to_float();
        }
        if let Some(v) = ctrl.params.get("y").and_then(|p| self.eval_param(p, env)) {
            self.vel.y *= v.to_float();
        }
    }

    /// `VelSet`: set x/y velocity components from the `x`/`y` parameters. A
    /// missing component leaves that axis unchanged.
    fn ctrl_vel_set(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        if let Some(v) = ctrl.params.get("x").and_then(|p| self.eval_param(p, env)) {
            self.vel.x = v.to_float();
        }
        if let Some(v) = ctrl.params.get("y").and_then(|p| self.eval_param(p, env)) {
            self.vel.y = v.to_float();
        }
    }

    /// `VelAdd`: add to the x/y velocity components from the `x`/`y` parameters.
    /// A missing component adds nothing on that axis.
    fn ctrl_vel_add(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        if let Some(v) = ctrl.params.get("x").and_then(|p| self.eval_param(p, env)) {
            self.vel.x += v.to_float();
        }
        if let Some(v) = ctrl.params.get("y").and_then(|p| self.eval_param(p, env)) {
            self.vel.y += v.to_float();
        }
    }

    /// `CtrlSet`: set the player control flag from the `value` parameter.
    fn ctrl_ctrl_set(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        if let Some(v) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) {
            self.ctrl = v.as_bool();
        }
    }

    /// `PosSet`: set the x/y position components from the `x`/`y` parameters. A
    /// missing component leaves that axis unchanged.
    ///
    /// `PosSet` operates on the **absolute** stage position: the `x` value is
    /// taken as-is and is **not** mirrored by facing (matching the `Pos X`
    /// trigger, which also reports the absolute stage position). Only
    /// facing-relative motion (velocity integration and [`PosAdd`](Self::ctrl_pos_add))
    /// applies the facing sign.
    fn ctrl_pos_set(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        if let Some(v) = ctrl.params.get("x").and_then(|p| self.eval_param(p, env)) {
            self.pos.x = v.to_float();
        }
        if let Some(v) = ctrl.params.get("y").and_then(|p| self.eval_param(p, env)) {
            self.pos.y = v.to_float();
        }
    }

    /// `PosAdd`: add to the x/y position components from the `x`/`y` parameters.
    /// A missing component adds nothing on that axis.
    ///
    /// `PosAdd` is **facing-relative on X** (MUGEN semantics): the `x` delta is
    /// mirrored by the facing sign (`pos.x += dx * facing_sign`), so a positive
    /// `x` always nudges the character *forward* regardless of which way it
    /// faces. The Y delta is never mirrored. (Contrast [`PosSet`](Self::ctrl_pos_set),
    /// which is absolute.)
    fn ctrl_pos_add(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        if let Some(v) = ctrl.params.get("x").and_then(|p| self.eval_param(p, env)) {
            self.pos.x += v.to_float() * self.facing.sign() as f32;
        }
        if let Some(v) = ctrl.params.get("y").and_then(|p| self.eval_param(p, env)) {
            self.pos.y += v.to_float();
        }
    }

    /// `ChangeAnim`: switch to the animation named by the `value` parameter and
    /// reset the animation cursor.
    ///
    /// The element index and element-time reset to the start of the new action
    /// (MUGEN restarts a `ChangeAnim` at element 1). An optional `elem`
    /// parameter selects a one-based starting element; it is stored zero-based
    /// and clamped to `>= 0` (the per-tick animation advance clamps it into the
    /// action's range, so an out-of-range value never panics). A missing `value`
    /// is a safe no-op.
    fn ctrl_change_anim(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) else {
            tracing::debug!(
                "tick: ChangeAnim in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        self.anim = value.to_int();
        // MUGEN's optional `elem` is one-based; store it zero-based. Default to
        // the first element when absent.
        let start_elem = match ctrl.params.get("elem").and_then(|p| self.eval_param(p, env)) {
            Some(v) => v.to_int().saturating_sub(1).max(0),
            None => 0,
        };
        self.anim_elem = start_elem;
        self.anim_elem_time = 0;
    }

    /// `VarSet`: assign a single variable to the value of an expression.
    ///
    /// Supports the MUGEN parameter forms (case-insensitive keys):
    /// - `var(i) = expr` (key `var(i)`) → integer bank,
    /// - `fvar(i) = expr` → float bank,
    /// - `sysvar(i) = expr` → system integer bank,
    /// - `sysfvar(i) = expr` → system float bank,
    /// - `v = i` + `value = expr` → integer bank,
    /// - `fv = i` + `value = expr` → float bank.
    ///
    /// An out-of-range index or an unrecognized form is a safe no-op.
    fn ctrl_var_set(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        // Indexed-key forms: `var(i)`, `fvar(i)`, `sysvar(i)`, `sysfvar(i)`.
        for (key, param) in &ctrl.params {
            if let Some((bank, index)) = parse_var_bank_key(key) {
                let value = self.eval_param(param, env).unwrap_or(Value::DEFAULT);
                self.assign_var(bank, index, value);
                // A VarSet sets exactly one variable; the first matching key wins.
                return;
            }
        }
        // `v`/`fv` + `value` form.
        if let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) {
            if let Some(index) = ctrl.params.get("v").and_then(|p| self.eval_param(p, env)) {
                self.assign_var(VarBank::Int, index.to_int(), value);
            } else if let Some(index) = ctrl.params.get("fv").and_then(|p| self.eval_param(p, env)) {
                self.assign_var(VarBank::Float, index.to_int(), value);
            } else {
                tracing::debug!(
                    "tick: VarSet in state {} has `value` but no `v`/`fv` index; ignored",
                    ctrl.state_number
                );
            }
        }
    }

    /// `VarAdd`: add an expression's value to a single variable.
    ///
    /// Accepts the same parameter forms as [`Self::ctrl_var_set`]. An
    /// out-of-range index or unrecognized form is a safe no-op.
    fn ctrl_var_add(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        for (key, param) in &ctrl.params {
            if let Some((bank, index)) = parse_var_bank_key(key) {
                let delta = self.eval_param(param, env).unwrap_or(Value::DEFAULT);
                self.add_var(bank, index, delta);
                return;
            }
        }
        if let Some(delta) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) {
            if let Some(index) = ctrl.params.get("v").and_then(|p| self.eval_param(p, env)) {
                self.add_var(VarBank::Int, index.to_int(), delta);
            } else if let Some(index) = ctrl.params.get("fv").and_then(|p| self.eval_param(p, env)) {
                self.add_var(VarBank::Float, index.to_int(), delta);
            } else {
                tracing::debug!(
                    "tick: VarAdd in state {} has `value` but no `v`/`fv` index; ignored",
                    ctrl.state_number
                );
            }
        }
    }

    /// `PowerAdd`: add the `value` expression to the super meter, clamping the
    /// result to `[0, power_max]`.
    ///
    /// Mirrors MUGEN's `PowerAdd` state controller. A missing `value` is a
    /// safe debug-logged no-op (adds nothing). A garbage value can never panic:
    /// the addition saturates and the result is clamped.
    fn ctrl_power_add(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) else {
            tracing::debug!(
                "tick: PowerAdd in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        self.add_power_clamped(value.to_int());
    }

    /// `PowerSet`: set the super meter to the `value` expression, clamping the
    /// result to `[0, power_max]`.
    ///
    /// Mirrors MUGEN's `PowerSet` state controller. A missing `value` is a
    /// safe debug-logged no-op (leaves power unchanged). A garbage value can
    /// never panic: the result is clamped into range.
    fn ctrl_power_set(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) else {
            tracing::debug!(
                "tick: PowerSet in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        self.set_power_clamped(value.to_int());
    }

    /// `AttackMulSet`: set the runtime attack multiplier (damage this character
    /// *deals* is scaled by it in `resolve_attack`; default `1.0`). A missing
    /// `value` is a safe debug-logged no-op; never panics.
    fn ctrl_attack_mul_set(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) else {
            tracing::debug!(
                "tick: AttackMulSet in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        self.attack_mul = value.to_float();
    }

    /// `DefenceMulSet`: set the runtime defence multiplier (damage this character
    /// *receives* is scaled by it in `resolve_attack`; default `1.0`). A missing
    /// `value` is a safe debug-logged no-op; never panics.
    fn ctrl_defence_mul_set(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) else {
            tracing::debug!(
                "tick: DefenceMulSet in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        self.defence_mul = value.to_float();
    }

    /// `VarRangeSet`: set a contiguous range of variables to one value.
    ///
    /// Parameters (case-insensitive): `value = expr` sets the integer bank,
    /// `fvalue = expr` sets the float bank; `first`/`last` bound the inclusive
    /// index range (both default to covering the whole bank when absent — MUGEN
    /// defaults `first` to `0` and `last` to the bank's maximum index). Indices
    /// outside the bank are skipped; the controller never panics.
    fn ctrl_var_range_set(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        let first = ctrl
            .params
            .get("first")
            .and_then(|p| self.eval_param(p, env))
            .map_or(0, |v| v.to_int());
        // `value` targets the int bank; `fvalue` targets the float bank.
        if let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) {
            let last = ctrl
                .params
                .get("last")
                .and_then(|p| self.eval_param(p, env))
                .map_or(NUM_VARS as i32 - 1, |v| v.to_int());
            for index in first..=last {
                self.assign_var(VarBank::Int, index, value);
            }
        }
        if let Some(value) = ctrl.params.get("fvalue").and_then(|p| self.eval_param(p, env)) {
            let last = ctrl
                .params
                .get("last")
                .and_then(|p| self.eval_param(p, env))
                .map_or(NUM_FVARS as i32 - 1, |v| v.to_int());
            for index in first..=last {
                self.assign_var(VarBank::Float, index, value);
            }
        }
    }

    /// `StateTypeSet`: override the state/move-type/physics categories without a
    /// state transition.
    ///
    /// Reads `statetype`/`movetype`/`physics` from the controller's params as
    /// bare letter tokens (the param value's raw source text, since the letter is
    /// an identifier rather than a number). An absent or unrecognized token
    /// leaves that category unchanged.
    fn ctrl_state_type_set(&mut self, ctrl: &CompiledController) {
        // These are bare letter tokens (`S`/`C`/`A`/`L`/`I`/`H`/`N`), read from
        // the parameter's raw source rather than evaluated as numbers.
        if let Some(param) = ctrl.params.get("statetype") {
            if let Some(t) = StateType::from_token(param.raw().trim()) {
                if t != StateType::Unchanged {
                    self.state_type = t;
                }
            }
        }
        if let Some(param) = ctrl.params.get("movetype") {
            if let Some(m) = MoveType::from_token(param.raw().trim()) {
                if m != MoveType::Unchanged {
                    self.move_type = m;
                }
            }
        }
        if let Some(param) = ctrl.params.get("physics") {
            if let Some(p) = Physics::from_token(param.raw().trim()) {
                if p != Physics::Unchanged {
                    self.physics = p;
                }
            }
        }
    }

    /// `Turn`: flip the character's facing (right ↔ left).
    fn ctrl_turn(&mut self) {
        self.facing = match self.facing {
            Facing::Right => Facing::Left,
            Facing::Left => Facing::Right,
        };
    }

    /// `PlaySnd`: emit a [`SoundRequest`] onto `report.sound_requests`.
    ///
    /// `fp-character` produces *no* audio — it only describes the request. The
    /// downstream player (`fp-audio`, Phase 8) consumes
    /// [`TickReport::sound_requests`] and performs playback.
    ///
    /// Parameters (MUGEN `PlaySnd` semantics):
    ///
    /// - `value = group, sample`. The `group` token may carry a single leading
    ///   letter *flag*: `F` (case-insensitive) selects the common / fight sound
    ///   file (sets [`SoundRequest::common`]) and is stripped before parsing the
    ///   group integer; `S` (or any other unknown letter) means the character's
    ///   own `.snd` (`common = false`) but its digits are still parsed. The
    ///   `value` is read from the **raw** source because the leading-letter form
    ///   (`F0`) is not arithmetic and would not survive the expression compiler.
    /// - `channel` (i32): the playback channel. **Default `-1`** — MUGEN's
    ///   documented `PlaySnd` default, meaning "play on the next free channel".
    ///   (Channel `0` is the reserved single-slot voice channel, so it is *not*
    ///   the no-op default.) The KB (`03-engine-architecture.md`) does not
    ///   override this.
    /// - `volumescale` (i32): output volume percentage. Default `100`.
    /// - `loop` (bool-ish): `1` / `-1` / `true` (case-insensitive) → looping.
    ///   Default `false`.
    ///
    /// Robust to bad content: a missing `value`, or a `value` whose group or
    /// sample cannot be parsed as an integer, logs at `debug` and pushes **no**
    /// request. Never panics, unwraps, or expects.
    fn ctrl_play_snd(&mut self, ctrl: &CompiledController, env: EvalEnv, report: &mut TickReport) {
        // `value = group, sample`. Read the raw source: the group may be
        // `F`/`S`-prefixed (non-arithmetic), so it cannot go through the VM.
        let Some(raw) = raw_param(ctrl, "value") else {
            tracing::debug!(
                "tick: PlaySnd in state {} has no `value`; no sound requested",
                ctrl.state_number
            );
            return;
        };
        let mut parts = raw.split(',');
        let group_tok = parts.next().unwrap_or("").trim();
        let sample_tok = parts.next().unwrap_or("").trim();

        // Strip an optional single leading letter flag from the group token:
        // `F`/`f` → common/fight sound file; any other letter (`S`, …) → own
        // .snd. The remaining text must parse as the integer group number.
        let (common, group_digits) = match group_tok.chars().next() {
            Some(c) if c.eq_ignore_ascii_case(&'F') => (true, group_tok[c.len_utf8()..].trim()),
            Some(c) if c.is_ascii_alphabetic() => (false, group_tok[c.len_utf8()..].trim()),
            _ => (false, group_tok),
        };

        let Ok(group) = group_digits.parse::<i32>() else {
            tracing::debug!(
                "tick: PlaySnd in state {} has unparseable group {group_tok:?}; no sound requested",
                ctrl.state_number
            );
            return;
        };
        let Ok(sample) = sample_tok.parse::<i32>() else {
            tracing::debug!(
                "tick: PlaySnd in state {} has unparseable sample {sample_tok:?}; \
                 no sound requested",
                ctrl.state_number
            );
            return;
        };

        // Optional numeric params evaluate against `self`; absent → MUGEN default.
        let channel = ctrl
            .params
            .get("channel")
            .and_then(|p| self.eval_param(p, env))
            .map_or(-1, |v| v.to_int());
        let volume_scale = ctrl
            .params
            .get("volumescale")
            .and_then(|p| self.eval_param(p, env))
            .map_or(100, |v| v.to_int());

        // `loop` is bool-ish: 1 / -1 / "true" all mean looping. Read the raw
        // token so a textual `true` is honored alongside the numeric forms.
        let looping = raw_param(ctrl, "loop").is_some_and(parse_loop_flag);

        report.sound_requests.push(SoundRequest {
            group,
            sample,
            channel,
            volume_scale,
            looping,
            common,
        });
        tracing::debug!(
            "tick: PlaySnd group={group} sample={sample} channel={channel} \
             volscale={volume_scale} loop={looping} common={common} in state {}",
            ctrl.state_number
        );
    }

    // ---- Target controllers (deferred ops) --------------------------------
    //
    // Each `Target*` controller mutates this character's *target* (the opponent
    // it established a hit on). The executor ticks a single character and only
    // borrows the opponent immutably, so these controllers cannot apply their
    // effect inline. Instead — exactly mirroring `PlaySnd`'s deferred
    // `SoundRequest` — they push a [`TargetOp`] onto `report.target_ops`, and
    // `fp-engine` (task P8b) applies each op to the opponent after the tick.
    //
    // When [`has_target`](Character::has_target) is `false` (no hit has been
    // established) every `Target*` controller is a safe, debug-logged no-op that
    // pushes nothing — matching MUGEN, where a `Target*` with no targets does
    // nothing. None of these ever panic, unwrap, or expect.

    /// `TargetState`: emit a [`TargetOp::State`] to send the target into the state
    /// named by `value`. Throws use this to drive the victim through the
    /// thrown-animation states (KFM state 820). A missing `value`, or no target,
    /// pushes nothing.
    fn ctrl_target_state(
        &self,
        ctrl: &CompiledController,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        if !self.has_target {
            tracing::debug!(
                "tick: TargetState in state {} with no target; no-op",
                ctrl.state_number
            );
            return;
        }
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) else {
            tracing::debug!(
                "tick: TargetState in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        report.target_ops.push(TargetOp::State(value.to_int()));
    }

    /// `TargetBind`: emit a [`TargetOp::Bind`] to hold the target at a position
    /// relative to this character for `time` ticks. Throws use this to pin the
    /// victim to the thrower each tick (KFM state 810).
    ///
    /// Params: `time` (i32, MUGEN default `1`) and `pos = x, y` (the bind offset;
    /// each axis defaults to `0.0` when absent). No target → pushes nothing.
    fn ctrl_target_bind(&self, ctrl: &CompiledController, env: EvalEnv, report: &mut TickReport) {
        if !self.has_target {
            tracing::debug!(
                "tick: TargetBind in state {} with no target; no-op",
                ctrl.state_number
            );
            return;
        }
        let time = ctrl
            .params
            .get("time")
            .and_then(|p| self.eval_param(p, env))
            .map_or(1, |v| v.to_int());
        let pos = match ctrl.params.get("pos") {
            Some(param) => {
                let comps = self.eval_param_components(param, env);
                (
                    comps.first().map_or(0.0, |v| v.to_float()),
                    comps.get(1).map_or(0.0, |v| v.to_float()),
                )
            }
            None => (0.0, 0.0),
        };
        report.target_ops.push(TargetOp::Bind { time, pos });
    }

    /// `TargetLifeAdd`: emit a [`TargetOp::LifeAdd`] to add `value` to the
    /// target's life (negative = damage). Throws use this to apply throw damage to
    /// the victim (KFM state 810).
    ///
    /// Params: `value` (i32, required — absent pushes nothing) and `kill`
    /// (bool-ish, MUGEN default `true`): when `false` the add must leave the
    /// target at `>= 1` life. No target → pushes nothing.
    fn ctrl_target_life_add(
        &self,
        ctrl: &CompiledController,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        if !self.has_target {
            tracing::debug!(
                "tick: TargetLifeAdd in state {} with no target; no-op",
                ctrl.state_number
            );
            return;
        }
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) else {
            tracing::debug!(
                "tick: TargetLifeAdd in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        // MUGEN's `kill` defaults to 1 (true): a TargetLifeAdd may be lethal
        // unless explicitly told not to kill.
        let kill = ctrl
            .params
            .get("kill")
            .and_then(|p| self.eval_param(p, env))
            .is_none_or(Value::as_bool);
        report.target_ops.push(TargetOp::LifeAdd {
            value: value.to_int(),
            kill,
        });
    }

    /// `TargetFacing`: emit a [`TargetOp::Facing`] to orient the target relative
    /// to this character (`1` = same facing, `-1` = opposite). Throws use this to
    /// face the victim toward the thrower (KFM state 810). A missing `value`, or
    /// no target, pushes nothing.
    fn ctrl_target_facing(
        &self,
        ctrl: &CompiledController,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        if !self.has_target {
            tracing::debug!(
                "tick: TargetFacing in state {} with no target; no-op",
                ctrl.state_number
            );
            return;
        }
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) else {
            tracing::debug!(
                "tick: TargetFacing in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        report.target_ops.push(TargetOp::Facing(value.to_int()));
    }

    /// `TargetVelSet`: emit a [`TargetOp::VelSet`] to set the target's velocity to
    /// `(x, y)`. An absent axis defaults to `0.0`. No target → pushes nothing.
    fn ctrl_target_vel_set(
        &self,
        ctrl: &CompiledController,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        if !self.has_target {
            tracing::debug!(
                "tick: TargetVelSet in state {} with no target; no-op",
                ctrl.state_number
            );
            return;
        }
        let xy = self.target_vel_xy(ctrl, env);
        report.target_ops.push(TargetOp::VelSet(xy));
    }

    /// `TargetVelAdd`: emit a [`TargetOp::VelAdd`] to add `(x, y)` to the target's
    /// velocity. An absent axis defaults to `0.0`. No target → pushes nothing.
    fn ctrl_target_vel_add(
        &self,
        ctrl: &CompiledController,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        if !self.has_target {
            tracing::debug!(
                "tick: TargetVelAdd in state {} with no target; no-op",
                ctrl.state_number
            );
            return;
        }
        let xy = self.target_vel_xy(ctrl, env);
        report.target_ops.push(TargetOp::VelAdd(xy));
    }

    /// Shared `x`/`y` reader for [`TargetVelSet`](Self::ctrl_target_vel_set) and
    /// [`TargetVelAdd`](Self::ctrl_target_vel_add): each axis evaluates against
    /// `self` and defaults to `0.0` when its param is absent.
    fn target_vel_xy(&self, ctrl: &CompiledController, env: EvalEnv) -> (f32, f32) {
        let x = ctrl
            .params
            .get("x")
            .and_then(|p| self.eval_param(p, env))
            .map_or(0.0, |v| v.to_float());
        let y = ctrl
            .params
            .get("y")
            .and_then(|p| self.eval_param(p, env))
            .map_or(0.0, |v| v.to_float());
        (x, y)
    }

    /// `TargetPowerAdd`: emit a [`TargetOp::PowerAdd`] to add `value` to the
    /// target's power meter. A missing `value`, or no target, pushes nothing.
    fn ctrl_target_power_add(
        &self,
        ctrl: &CompiledController,
        env: EvalEnv,
        report: &mut TickReport,
    ) {
        if !self.has_target {
            tracing::debug!(
                "tick: TargetPowerAdd in state {} with no target; no-op",
                ctrl.state_number
            );
            return;
        }
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p, env)) else {
            tracing::debug!(
                "tick: TargetPowerAdd in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        report.target_ops.push(TargetOp::PowerAdd(value.to_int()));
    }

    /// `HitDef`: build a [`fp_combat::HitDef`] from the controller's parameters
    /// and store it as this character's [`active_hitdef`](Character::active_hitdef).
    ///
    /// MUGEN's `HitDef` carries two *kinds* of parameter:
    ///
    /// - **String / enum** params (`attr`, `hitflag`, `guardflag`, `ground.type`,
    ///   `animtype`, `air.animtype`, and the spark / sound ids which may carry an
    ///   `S` prefix) are read from the controller's **raw parameter source**
    ///   ([`CompiledParam::raw`]) and parsed with [`fp_combat::AttackAttr::parse`]
    ///   / [`fp_combat::HitFlags::parse`] / [`fp_combat::AnimType::parse`] / a
    ///   small local type parser. Compiling these as numeric expressions would be
    ///   wrong (`S, NA` is not arithmetic). `air.animtype` follows the MUGEN rule
    ///   that it defaults to the parsed `animtype` when its key is absent.
    ///   (CB27: `air.type` is **not** parsed — there is no `air_type` field; MUGEN
    ///   defaults a HitDef's `air.type` to its `ground.type`, which is the only hit
    ///   type modelled here.)
    /// - **Numeric** params (`damage`, `ground.velocity`, `air.velocity`,
    ///   `guard.velocity`, `pausetime`, `p1stateno`, `p2stateno`, the hit-times,
    ///   `fall`, `priority`, `id`, `chainid`, `fall.yvelocity`) are obtained by
    ///   **evaluating** the compiled parameter expression(s) against `self` (the
    ///   attacker), so authored expressions like `damage = ceil(var(1)*1.5), 0`
    ///   resolve correctly. Multi-component params (`x, y` or `hit, guard`) are
    ///   split on commas and each component is compiled and evaluated on its own.
    ///
    /// Any unspecified parameter falls back to [`fp_combat::HitDef::default`]'s
    /// MUGEN-faithful value. This never panics: a malformed string parses to its
    /// documented safe default and a malformed expression evaluates to `0`.
    fn ctrl_hit_def(&mut self, ctrl: &CompiledController, env: EvalEnv) {
        let mut hd = fp_combat::HitDef::default();

        // ---- String / enum params (read from raw source) ------------------
        if let Some(src) = raw_param(ctrl, "attr") {
            hd.attr = fp_combat::AttackAttr::parse(src);
        }
        if let Some(src) = raw_param(ctrl, "hitflag") {
            hd.hitflag = fp_combat::HitFlags::parse(src);
        }
        if let Some(src) = raw_param(ctrl, "guardflag") {
            hd.guardflag = fp_combat::HitFlags::parse(src);
        }
        if let Some(src) = raw_param(ctrl, "ground.type") {
            hd.ground_type = parse_hit_type(src);
        }
        // `animtype` selects the get-hit *reaction animation* (read back via
        // `GetHitVar(animtype)`); `air.animtype` is the airborne variant. MUGEN
        // rule: when `air.animtype` is absent it defaults to whatever `animtype`
        // was set to, so parse `animtype` first and seed BOTH from it, then let an
        // explicit `air.animtype` key override the air slot.
        if let Some(src) = raw_param(ctrl, "animtype") {
            hd.animtype = fp_combat::AnimType::parse(src);
            // Keep the air default tracking the ground value until/unless an
            // explicit `air.animtype` overrides it below.
            hd.air_animtype = hd.animtype;
        }
        if let Some(src) = raw_param(ctrl, "air.animtype") {
            hd.air_animtype = fp_combat::AnimType::parse(src);
        }

        // Spark id. `sparkno` may carry a leading `S` (use the character's own
        // AIR set rather than the common set). The `S` prefix is not modelled in
        // `fp_combat::HitResources`, so we strip it and keep the numeric id; an
        // absent / non-numeric id keeps the default (`-1`).
        if let Some(src) = raw_param(ctrl, "sparkno") {
            hd.resources.sparkno = parse_resource_id(src, hd.resources.sparkno);
        }
        // Hit / guard sounds. These are a `group, sample` pair (the sample was
        // dropped by the old single-`i32` model). Unlike `PlaySnd`, these default
        // to the common / fight sound file; a leading `S`/`s` flag selects the
        // character's own `.snd` instead. Parsing is owned by
        // `fp_combat::SoundId::parse`; `-1`, empty, or garbage → `None` (no sound).
        // When the param is present we always overwrite the default so an authored
        // `-1` clears it to `None`.
        if let Some(src) = raw_param(ctrl, "hitsound") {
            hd.resources.hitsound = fp_combat::SoundId::parse(src);
        }
        if let Some(src) = raw_param(ctrl, "guardsound") {
            hd.resources.guardsound = fp_combat::SoundId::parse(src);
        }

        // ---- Numeric params (evaluated against self / the attacker) --------
        // Each parameter was already split on top-level commas and compiled into
        // its component list by the loader (6.2b); the executor reads component
        // `i` directly via the [`CompiledParam`] accessor — no re-splitting.
        //
        // `damage = hit [, guard]`. A missing guard component mirrors the hit
        // value in MUGEN; we keep it simple and leave guard at its default (0)
        // when absent, matching `HitDef::default()`.
        if let Some(param) = ctrl.params.get("damage") {
            if let Some(hit) = self.eval_param_component(param, 0, env) {
                hd.damage.hit = hit.to_int();
            }
            if let Some(guard) = self.eval_param_component(param, 1, env) {
                hd.damage.guard = guard.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("ground.velocity") {
            let comps = self.eval_param_components(param, env);
            hd.ground_velocity = pair_to_vec2(&comps, hd.ground_velocity);
        }
        if let Some(param) = ctrl.params.get("air.velocity") {
            let comps = self.eval_param_components(param, env);
            hd.air_velocity = pair_to_vec2(&comps, hd.air_velocity);
        }
        if let Some(param) = ctrl.params.get("guard.velocity") {
            // Single X pushback (Y unused).
            if let Some(x) = self.eval_param_component(param, 0, env) {
                hd.guard_velocity = x.to_float();
            }
        }
        if let Some(param) = ctrl.params.get("pausetime") {
            if let Some(p1) = self.eval_param_component(param, 0, env) {
                hd.pausetime.p1 = p1.to_int();
            }
            if let Some(p2) = self.eval_param_component(param, 1, env) {
                hd.pausetime.p2 = p2.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("ground.hittime") {
            if let Some(v) = self.eval_param_component(param, 0, env) {
                hd.hittimes.ground = v.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("air.hittime") {
            if let Some(v) = self.eval_param_component(param, 0, env) {
                hd.hittimes.air = v.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("guard.hittime") {
            if let Some(v) = self.eval_param_component(param, 0, env) {
                hd.hittimes.guard = v.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("p1stateno") {
            if let Some(v) = self.eval_param_component(param, 0, env) {
                hd.p1stateno = Some(v.to_int());
            }
        }
        if let Some(param) = ctrl.params.get("p2stateno") {
            if let Some(v) = self.eval_param_component(param, 0, env) {
                hd.p2stateno = Some(v.to_int());
            }
        }
        if let Some(param) = ctrl.params.get("fall") {
            if let Some(v) = self.eval_param_component(param, 0, env) {
                hd.fall = v.as_bool();
            }
        }
        if let Some(param) = ctrl.params.get("fall.yvelocity") {
            if let Some(v) = self.eval_param_component(param, 0, env) {
                hd.fall_yvelocity = v.to_float();
            }
        }
        if let Some(param) = ctrl.params.get("priority") {
            // `priority = value [, type]`. The numeric value is component 0; the
            // optional type token is a string/enum read from the raw source.
            if let Some(v) = self.eval_param_component(param, 0, env) {
                hd.priority.value = v.to_int();
            }
            if let Some(kind) = parse_priority_type(param.raw()) {
                hd.priority.kind = kind;
            }
        }
        if let Some(param) = ctrl.params.get("id") {
            if let Some(v) = self.eval_param_component(param, 0, env) {
                hd.id = v.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("chainid") {
            if let Some(v) = self.eval_param_component(param, 0, env) {
                hd.chainid = v.to_int();
            }
        }

        tracing::debug!(
            "tick: HitDef in state {} -> attr {:?}, damage {:?}",
            ctrl.state_number,
            hd.attr,
            hd.damage
        );
        self.active_hitdef = Some(hd);
    }

    /// `NotHitBy` / `HitBy`: install an attack-attribute invulnerability window
    /// (faithfulness audit P9).
    ///
    /// Both controllers share this implementation, differing only in the
    /// [`InvulnMode`](crate::invuln::InvulnMode) passed in (`NotHitBy` = exclude
    /// the listed attributes, `HitBy` = admit *only* the listed attributes).
    ///
    /// Parameters (MUGEN semantics):
    ///
    /// - `value` → **slot 1**, `value2` → **slot 2**: each an attack-attribute
    ///   string (a state-type letter group `S`/`C`/`A` followed by 2-char
    ///   attack-class pairs, e.g. `SCA` or `, NT,ST,HT`). Read from the **raw**
    ///   source (it is not arithmetic) and parsed with
    ///   [`AttackAttrSet::parse`](crate::invuln::AttackAttrSet::parse). A `*` or
    ///   empty value is the "all attributes" wildcard.
    /// - `time` (i32, evaluated): how many ticks the window stays active.
    ///   **Default `1`** — MUGEN's documented default (the window covers just the
    ///   current tick, the common `value = SCA / time = 1` per-frame form).
    /// - `ignorehitpause`: when set, the slot keeps counting down during a
    ///   hit-pause freeze; otherwise it freezes like every other per-tick timer.
    ///   Read from the controller's universal `ignorehitpause` flag.
    ///
    /// A slot whose parameter is **present** is always (re)written, even if the
    /// other slot's parameter is absent (the absent slot is left untouched — so a
    /// `NotHitBy value2 = ...` does not clear a still-active slot 1). MUGEN
    /// re-arms a slot each time the controller fires; a `time` of `0` or less sets
    /// an immediately-inactive slot (blocks nothing). Never panics: a missing
    /// `value` simply leaves slot 1 untouched; a malformed attr string parses to
    /// its documented safe set (see [`crate::invuln`]).
    fn ctrl_invuln(
        &mut self,
        ctrl: &CompiledController,
        env: EvalEnv,
        mode: crate::invuln::InvulnMode,
    ) {
        // `time` (ticks): evaluated, MUGEN default 1.
        let time = ctrl
            .params
            .get("time")
            .and_then(|p| self.eval_param(p, env))
            .map_or(1, |v| v.to_int());

        // `ignorehitpause` keeps the slot counting during a hit-pause freeze.
        let ignore_hitpause = self.ignorehitpause_flag(ctrl, env);

        // Slot 1 from `value`, slot 2 from `value2`. Only (re)write a slot whose
        // raw source is present; an absent value leaves that slot untouched.
        if let Some(src) = raw_param(ctrl, "value") {
            self.invuln.slot1 = crate::invuln::InvulnSlot {
                attrs: crate::invuln::AttackAttrSet::parse(src),
                mode,
                time_remaining: time,
                ignore_hitpause,
            };
        }
        if let Some(src) = raw_param(ctrl, "value2") {
            self.invuln.slot2 = crate::invuln::InvulnSlot {
                attrs: crate::invuln::AttackAttrSet::parse(src),
                mode,
                time_remaining: time,
                ignore_hitpause,
            };
        }
    }

    // ---- Variable-bank helpers --------------------------------------------

    /// Assigns `value` to variable `index` of `bank`, narrowing/widening to the
    /// bank's element type. An out-of-range index is a debug-logged no-op.
    fn assign_var(&mut self, bank: VarBank, index: i32, value: Value) {
        let Ok(i) = usize::try_from(index) else {
            tracing::debug!("tick: var assign with negative index {index}; ignored");
            return;
        };
        match bank {
            VarBank::Int => {
                if let Some(slot) = self.vars.get_mut(i) {
                    *slot = value.to_int();
                }
            }
            VarBank::Float => {
                if let Some(slot) = self.fvars.get_mut(i) {
                    *slot = value.to_float();
                }
            }
            VarBank::SysInt => {
                if let Some(slot) = self.sysvars.get_mut(i) {
                    *slot = value.to_int();
                }
            }
            VarBank::SysFloat => {
                if let Some(slot) = self.sysfvars.get_mut(i) {
                    *slot = value.to_float();
                }
            }
        }
    }

    /// Adds `delta` to variable `index` of `bank`. An out-of-range index is a
    /// debug-logged no-op.
    fn add_var(&mut self, bank: VarBank, index: i32, delta: Value) {
        let Ok(i) = usize::try_from(index) else {
            tracing::debug!("tick: var add with negative index {index}; ignored");
            return;
        };
        match bank {
            VarBank::Int => {
                if let Some(slot) = self.vars.get_mut(i) {
                    *slot = slot.wrapping_add(delta.to_int());
                }
            }
            VarBank::Float => {
                if let Some(slot) = self.fvars.get_mut(i) {
                    *slot += delta.to_float();
                }
            }
            VarBank::SysInt => {
                if let Some(slot) = self.sysvars.get_mut(i) {
                    *slot = slot.wrapping_add(delta.to_int());
                }
            }
            VarBank::SysFloat => {
                if let Some(slot) = self.sysfvars.get_mut(i) {
                    *slot += delta.to_float();
                }
            }
        }
    }

    // ---- Power (super meter) ----------------------------------------------

    /// Sets the power (super meter) to `value`, clamped to `[0, power_max]`.
    ///
    /// All power mutations route through here so the meter is never left outside
    /// its valid range. A `power_max` that is somehow negative (malformed data)
    /// collapses the range to `0`, yielding `power == 0` rather than a panic.
    fn set_power_clamped(&mut self, value: i32) {
        let max = self.power_max.max(0);
        self.power = value.clamp(0, max);
    }

    /// Adds `delta` (which may be negative) to the power meter, clamping the
    /// result to `[0, power_max]`. Uses saturating arithmetic so a garbage
    /// `delta` near `i32::MAX`/`i32::MIN` cannot overflow before the clamp.
    fn add_power_clamped(&mut self, delta: i32) {
        self.set_power_clamped(self.power.saturating_add(delta));
    }

    // ---- State entry -------------------------------------------------------

    /// Performs a state transition into `target`: records the previous state,
    /// resets time-in-state, clears the per-entry `persistent` bookkeeping, and
    /// applies the destination statedef's entry parameters.
    ///
    /// An unknown destination still updates the cursor (so triggers reading
    /// `StateNo` see the requested number) but applies no entry parameters and
    /// warns — never panics.
    fn enter_state(&mut self, states: &HashMap<i32, CompiledState>, target: i32, env: EvalEnv) {
        self.prev_state_no = self.state_no;
        self.state_no = target;
        self.state_time = 0;
        // `persistent` is per-state-entry: clear the firing counts so the
        // destination state's controllers start fresh. Counts for OTHER states
        // (the special -3/-2/-1 states) are also cleared, which is correct: they
        // re-qualify each tick anyway and we re-key by the new state number.
        self.fire_counts.clear();

        let Some(state) = states.get(&target) else {
            tracing::debug!("tick: ChangeState to unknown state {target}; cursor updated only");
            return;
        };
        self.apply_state_entry(state, env);
    }

    /// Applies a statedef's entry parameters: `type`/`movetype`/`physics`
    /// (letter tokens), `anim`/`ctrl` (compiled expressions), `velset`
    /// (`x, y`), and `poweradd` (compiled expression, added to the super meter
    /// once per entry and clamped to `[0, power_max]`). An unrecognized or
    /// absent value leaves the field unchanged (MUGEN's "unchanged" semantics);
    /// an absent `poweradd` adds nothing.
    fn apply_state_entry(&mut self, state: &CompiledState, env: EvalEnv) {
        if let Some(token) = state.state_type.as_deref() {
            if let Some(t) = StateType::from_token(token) {
                if t != StateType::Unchanged {
                    self.state_type = t;
                }
            }
        }
        if let Some(token) = state.movetype.as_deref() {
            if let Some(m) = MoveType::from_token(token) {
                if m != MoveType::Unchanged {
                    self.move_type = m;
                }
            }
        }
        if let Some(token) = state.physics.as_deref() {
            if let Some(p) = Physics::from_token(token) {
                if p != Physics::Unchanged {
                    self.physics = p;
                }
            }
        }
        if let Some(anim_expr) = &state.anim {
            self.anim = self.eval_value(anim_expr, env).to_int();
            // A new animation restarts at the first element.
            self.anim_elem = 0;
            self.anim_elem_time = 0;
        }
        if let Some(ctrl_expr) = &state.ctrl {
            self.ctrl = self.eval_value(ctrl_expr, env).as_bool();
        }
        if let Some(velset) = &state.velset {
            if let Some((x, y)) = parse_velset(velset) {
                self.vel.x = x;
                self.vel.y = y;
            }
        }
        // `poweradd`: add to the super meter once, on entry. This is how MUGEN
        // attack states fill the power bar toward the super threshold (e.g.
        // KFM's `[Statedef 200] poweradd = 10`). Clamped to `[0, power_max]`.
        if let Some(poweradd_expr) = &state.poweradd {
            let delta = self.eval_value(poweradd_expr, env).to_int();
            self.add_power_clamped(delta);
        }
    }

    // ---- Per-tick physics / time / animation -------------------------------

    /// Applies the statedef `physics` to velocity for this tick: stand/crouch
    /// physics multiply x-velocity by the matching friction coefficient; air
    /// physics adds gravity (`yaccel`) to y-velocity; none/unchanged do nothing.
    fn apply_physics(&mut self) {
        let mv = &self.constants.movement;
        match self.physics {
            Physics::Stand => self.vel.x *= mv.stand_friction,
            Physics::Crouch => self.vel.x *= mv.crouch_friction,
            // Y increases downward, so gravity (a downward acceleration) is a
            // positive addition to y-velocity.
            Physics::Air => self.vel.y += mv.yaccel,
            Physics::None | Physics::Unchanged => {}
        }
    }

    /// Integrates the world position from the (facing-relative) velocity for this
    /// tick: `world pos.x += vel.x * facing_sign`, `world pos.y += vel.y`, then
    /// clamps `pos.y` to the ground plane ([`GROUND_Y`]).
    ///
    /// MUGEN state-controller velocities are **facing-relative** (`+x` = the way
    /// the character faces), so the stored `vel.x` is mirrored by the facing sign
    /// (`+1` right, `-1` left) only here, when advancing the absolute stage
    /// position. The stored velocity itself is left untouched (the `Vel X`
    /// trigger keeps returning the facing-relative value), and the Y axis is
    /// never mirrored. A facing-right character with `vel.x = +V` moves `+x`; a
    /// facing-left character with the *same* stored `vel.x = +V` moves `-x`.
    ///
    /// **Ground clamp.** Y increases downward and the floor is [`GROUND_Y`]
    /// (`0`); positive Y is below the floor, which a player may never reach. After
    /// integrating, `pos.y` is held at `min(pos.y, GROUND_Y)` every tick so a
    /// falling character (positive `vel.y`) settles *on* the ground instead of
    /// sinking. Crucially only the **position** is clamped — `vel.y` is left
    /// untouched so `common1`'s land transition (air [Statedef 50]:
    /// `Vel Y > 0 && Pos Y >= 0` → `ChangeState` 52) still observes the downward
    /// velocity on the landing frame and Jump Land (state 52) gets to run its own
    /// `VelSet`/`PosSet` to settle. Upward motion (negative Y) is unaffected by
    /// `min(_, 0)`, and a grounded character already at `GROUND_Y` is unchanged.
    fn integrate_position(&mut self) {
        self.pos.x += self.vel.x * self.facing.sign() as f32;
        self.pos.y += self.vel.y;
        // Hold at the floor: clamp position only, never velocity, so the
        // data-driven land trigger still sees `Vel Y > 0` on the landing frame.
        self.pos.y = self.pos.y.min(GROUND_Y);
    }

    /// Advances time-in-state by one tick.
    fn advance_time(&mut self) {
        self.state_time = self.state_time.saturating_add(1);
    }

    /// Advances the animation cursor by one tick using the AIR action's frame
    /// durations.
    ///
    /// The current frame holds for its `ticks` duration; when elapsed, the
    /// cursor moves to the next element, looping back to the action's
    /// `loopstart` at the end. A frame with `ticks <= 0` is treated as
    /// hold-forever (MUGEN's `-1`): the element never advances. `anim_time` is
    /// maintained as the ticks remaining until the action finishes (negative for
    /// a looping action that has passed its end), matching the `AnimTime`
    /// trigger contract. An unknown animation degrades to a no-op.
    fn advance_animation(&mut self, air: &AirFile) {
        let Some(action) = air.action(self.anim) else {
            // Unknown animation: nothing to advance (safe no-op). Drop any stale
            // per-element table so `AnimElemTime(n)` falls back to the legacy
            // scalar rather than indexing a table for a different action.
            if self.anim_table_action != Some(self.anim) {
                self.anim_elem_start_offsets.clear();
                self.anim_table_action = Some(self.anim);
            }
            return;
        };
        if action.frames.is_empty() {
            if self.anim_table_action != Some(self.anim) {
                self.anim_elem_start_offsets.clear();
                self.anim_table_action = Some(self.anim);
            }
            return;
        }

        // (Re)build the per-element start-offset table whenever the active action
        // number changes (or on first entry). `start_offset[i]` is the cumulative
        // sum of the `ticks` of elements `0..i` — element 0 starts at 0 — so
        // `AnimElemTime(n)` can compute time-since-element-n for ANY element of
        // the action (see `Character::anim_elem_time_for`). Hold-forever frames
        // (`ticks <= 0`) contribute 0 to later offsets: such a frame never ends,
        // so no later element ever begins this iteration anyway.
        if self.anim_table_action != Some(self.anim) {
            self.rebuild_anim_elem_offsets(action);
        }

        // Clamp the element index into range defensively (it can only go out of
        // range via external mutation, but never panic).
        let mut elem = clamp_index(self.anim_elem, action.frames.len());
        self.anim_elem_time = self.anim_elem_time.saturating_add(1);

        // Advance through as many elements as this tick's elapsed time allows.
        // A hold-forever frame (ticks <= 0) never advances; a frame whose time
        // is not yet up stops the loop.
        while let Some(frame) = action.frames.get(elem) {
            let dur = frame.ticks;
            // Hold-forever element, or this element's time not yet up: stop.
            if dur <= 0 || self.anim_elem_time < dur {
                break;
            }
            // This element's time is up; move to the next, looping at the end.
            self.anim_elem_time = 0;
            elem += 1;
            if elem >= action.frames.len() {
                elem = clamp_index_usize(action.loopstart, action.frames.len());
            }
        }

        self.anim_elem = i32::try_from(elem).unwrap_or(0);
        self.anim_time = remaining_anim_time(action, elem, self.anim_elem_time);
    }

    /// Builds the per-element cumulative start-offset table on the character from
    /// an AIR action's frame durations and records the action it was built for.
    ///
    /// `start_offset[i] = sum(ticks of elements 0..i)`, so element 0 starts at
    /// `0` and each later element starts after the cumulative duration of the
    /// elements before it. Negative durations (`-1` = hold-forever) are treated
    /// as contributing `0`: a hold-forever element never ends, so the offsets of
    /// elements after it are only meaningful in the (impossible) case the cursor
    /// reaches them, and a `0` contribution keeps the running sum monotonic and
    /// panic-free. This is the offset table read by
    /// [`Character::anim_elem_time_for`] to answer `AnimElemTime(n)` for any `n`.
    fn rebuild_anim_elem_offsets(&mut self, action: &fp_formats::air::AnimAction) {
        self.anim_elem_start_offsets.clear();
        self.anim_elem_start_offsets.reserve(action.frames.len());
        let mut cumulative: i32 = 0;
        for frame in &action.frames {
            self.anim_elem_start_offsets.push(cumulative);
            // Hold-forever (`ticks <= 0`) contributes 0 to later start offsets.
            cumulative = cumulative.saturating_add(frame.ticks.max(0));
        }
        self.anim_table_action = Some(self.anim);
    }
}

/// Returns the contiguous prefix of numbered trigger groups starting at
/// `trigger1`, stopping at the first gap (CB6).
///
/// MUGEN numbers groups from 1. Groups are sorted by number; the prefix
/// `1, 2, 3, …` is taken until a number is missing. With `1, 2, 4` the result
/// is `[1, 2]` (group 4 and anything after it is dropped). A set that does not
/// start at `1` yields an empty slice (no `trigger1` → cannot fire).
fn contiguous_groups(groups: &[CompiledTriggerGroup]) -> Vec<&CompiledTriggerGroup> {
    // Sort references by group number so file order does not matter.
    let mut sorted: Vec<&CompiledTriggerGroup> = groups.iter().collect();
    sorted.sort_by_key(|g| g.number);

    let mut out: Vec<&CompiledTriggerGroup> = Vec::new();
    let mut expected: u32 = 1;
    for g in sorted {
        if g.number < expected {
            // Duplicate number (already consumed as `expected - 1`): the CNS
            // parser ANDs same-number lines into one group, so this is rare, but
            // skip defensively without breaking contiguity.
            continue;
        }
        if g.number == expected {
            out.push(g);
            expected += 1;
        } else {
            // Gap: stop here (CB6).
            break;
        }
    }
    out
}

/// Decides whether a controller fires on its `count`-th qualifying tick given
/// its `persistent` value.
///
/// - `persistent == 0`: fire only on the **first** qualifying tick of the state
///   entry (once per entry).
/// - `persistent == 1` (the MUGEN default): fire on **every** qualifying tick.
/// - `persistent == n` (`n > 1`): fire on every `n`th qualifying tick
///   (`count == n, 2n, …`).
/// - `persistent < 0`: treated as `1` (defensive; MUGEN does not define
///   negative values).
fn persistent_allows(persistent: i32, count: i32) -> bool {
    match persistent {
        0 => count == 1,
        1 => true,
        n if n > 1 => count % n == 0,
        // Negative / unexpected → behave like the default.
        _ => true,
    }
}

/// Which variable bank a `VarSet`/`VarAdd` target refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarBank {
    /// Integer bank (`var(i)`).
    Int,
    /// Float bank (`fvar(i)`).
    Float,
    /// System integer bank (`sysvar(i)`).
    SysInt,
    /// System float bank (`sysfvar(i)`).
    SysFloat,
}

/// Parses a `var(i)`-style controller parameter key into its bank and index.
///
/// Recognizes (the key is already lowercased by the CNS parser):
/// `var(i)`, `fvar(i)`, `sysvar(i)`, `sysfvar(i)`. The index is the integer
/// between the parentheses. Returns `None` for any other key (so the caller
/// falls through to the `v`/`fv` + `value` form).
fn parse_var_bank_key(key: &str) -> Option<(VarBank, i32)> {
    let key = key.trim();
    // Order matters: check the longer `sysfvar`/`sysvar`/`fvar` prefixes before
    // the `var` prefix so `sysvar(0)` is not mis-read as bank `var`.
    let (bank, rest) = if let Some(rest) = key.strip_prefix("sysfvar") {
        (VarBank::SysFloat, rest)
    } else if let Some(rest) = key.strip_prefix("sysvar") {
        (VarBank::SysInt, rest)
    } else if let Some(rest) = key.strip_prefix("fvar") {
        (VarBank::Float, rest)
    } else if let Some(rest) = key.strip_prefix("var") {
        (VarBank::Int, rest)
    } else {
        return None;
    };
    // `rest` must be `(<digits>)` (whitespace tolerated inside).
    let inner = rest.trim().strip_prefix('(')?.strip_suffix(')')?.trim();
    let index = inner.parse::<i32>().ok()?;
    Some((bank, index))
}

/// Returns the verbatim raw source of a controller parameter (case-insensitive
/// key lookup), or `None` if the parameter is absent.
///
/// Used by the `HitDef` controller to read string / enum parameters (`attr`,
/// `hitflag`, …) that must be parsed as text rather than evaluated as
/// arithmetic. Parameter keys are stored lowercased by the loader, so the
/// common case is a direct lookup; the fallback scan tolerates any stray
/// mixed-case key without panicking.
fn raw_param<'a>(ctrl: &'a CompiledController, key: &str) -> Option<&'a str> {
    if let Some(param) = ctrl.params.get(key) {
        return Some(param.raw());
    }
    ctrl.params
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.raw())
}

/// Parses a MUGEN `ground.type` / `air.type` token (`High`/`Low`/`Trip`/`None`,
/// case-insensitive) into a [`fp_combat::HitType`], defaulting to
/// [`fp_combat::HitType::High`] (MUGEN's default) on an unrecognized token.
fn parse_hit_type(raw: &str) -> fp_combat::HitType {
    let t = raw.trim();
    if t.eq_ignore_ascii_case("High") {
        fp_combat::HitType::High
    } else if t.eq_ignore_ascii_case("Low") {
        fp_combat::HitType::Low
    } else if t.eq_ignore_ascii_case("Trip") {
        fp_combat::HitType::Trip
    } else if t.eq_ignore_ascii_case("None") {
        fp_combat::HitType::None
    } else {
        tracing::debug!("HitDef: unrecognized hit type {raw:?}; defaulting to High");
        fp_combat::HitType::High
    }
}

/// Parses the optional `priority` *type* token (`Hit`/`Miss`/`Dodge`,
/// case-insensitive), which follows the numeric priority value. Returns `None`
/// when no type token is present (the caller keeps the default), and warns to
/// `debug` on an unrecognized token (also `None`).
fn parse_priority_type(raw: &str) -> Option<fp_combat::PriorityType> {
    // `priority = value, type`: the type is the second comma-separated token.
    let token = raw.split(',').nth(1)?.trim();
    if token.is_empty() {
        return None;
    }
    if token.eq_ignore_ascii_case("Hit") {
        Some(fp_combat::PriorityType::Hit)
    } else if token.eq_ignore_ascii_case("Miss") {
        Some(fp_combat::PriorityType::Miss)
    } else if token.eq_ignore_ascii_case("Dodge") {
        Some(fp_combat::PriorityType::Dodge)
    } else {
        tracing::debug!("HitDef: unrecognized priority type {token:?}; keeping default");
        None
    }
}

/// Parses a spark / sound resource id from its raw source, tolerating a leading
/// `S` prefix (MUGEN's "use my own AIR/SND set" marker, not yet modelled). The
/// numeric id is taken from the first comma-separated component; an absent or
/// non-numeric id keeps `fallback` (the field's current default).
fn parse_resource_id(raw: &str, fallback: i32) -> i32 {
    let first = raw.split(',').next().unwrap_or("").trim();
    // Strip an optional leading `S` / `s` prefix.
    let digits = first
        .strip_prefix(['S', 's'])
        .map(str::trim)
        .unwrap_or(first);
    digits.parse::<i32>().unwrap_or(fallback)
}

/// Interprets a `PlaySnd` `loop` flag token as a boolean.
///
/// MUGEN treats the loop flag as bool-ish: `1`, `-1`, and the textual `true`
/// (case-insensitive) all enable looping; `0`, `false`, empty, or anything else
/// disables it. Only the first comma-separated token is considered. Never panics.
fn parse_loop_flag(raw: &str) -> bool {
    let token = raw.split(',').next().unwrap_or("").trim();
    if token.eq_ignore_ascii_case("true") {
        return true;
    }
    matches!(token.parse::<i32>(), Ok(1) | Ok(-1))
}

/// Maps the first two evaluated components to a [`Vec2`], falling back to the
/// corresponding component of `default` when a component is missing.
fn pair_to_vec2(comps: &[Value], default: Vec2<f32>) -> Vec2<f32> {
    let x = comps.first().map_or(default.x, |v| v.to_float());
    let y = comps.get(1).map_or(default.y, |v| v.to_float());
    Vec2::new(x, y)
}

/// Parses a `velset` value (`"x, y"`) into `(x, y)`. A missing or non-numeric
/// component defaults to `0.0`; returns `None` only when the string has no
/// parseable first component (the caller then leaves velocity unchanged).
fn parse_velset(raw: &str) -> Option<(f32, f32)> {
    let mut parts = raw.split(',').map(str::trim);
    let x = parts.next().and_then(|p| p.parse::<f32>().ok())?;
    let y = parts.next().and_then(|p| p.parse::<f32>().ok()).unwrap_or(0.0);
    Some((x, y))
}

/// Clamps a possibly-out-of-range signed element index into `0..len`, returning
/// `0` when `len` is `0` (the caller guards against empty actions first).
fn clamp_index(index: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let max = len - 1;
    if index < 0 {
        0
    } else {
        (index as usize).min(max)
    }
}

/// Clamps a `usize` loop-start index into `0..len`.
fn clamp_index_usize(index: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        index.min(len - 1)
    }
}

/// Computes the `AnimTime`-style remaining ticks until the action finishes,
/// given the current element index and elapsed time within it.
///
/// MUGEN's `AnimTime` is the (negative) number of ticks left until the last
/// frame's display ends; on the final tick of a finite animation it reads `0`.
/// For a looping or hold-forever action the value can be `0`/positive. This is a
/// best-effort reconstruction sufficient for the executor and the `AnimTime`
/// trigger: it sums the remaining durations from the current element to the end.
fn remaining_anim_time(
    action: &fp_formats::air::AnimAction,
    elem: usize,
    elem_time: i32,
) -> i32 {
    let frames = &action.frames;
    let Some(current) = frames.get(elem) else {
        return 0;
    };
    // A hold-forever current frame never finishes.
    if current.ticks <= 0 {
        return 0;
    }
    // Remaining in the current element, then the full durations of the rest.
    let mut remaining = (current.ticks - elem_time).max(0);
    for f in &frames[elem + 1..] {
        if f.ticks <= 0 {
            // A hold-forever later frame means the action never finishes.
            return 0;
        }
        remaining = remaining.saturating_add(f.ticks);
    }
    // MUGEN reports AnimTime as negative (ticks-until-end), 0 on the last tick.
    -remaining
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::{
        CompiledExpr, CompiledParam, CompiledState, CompiledTriggerGroup, LoadedCharacter,
    };
    use crate::{
        ActiveCommands, CharacterConstants, MovementConstants, MoveType, NoCommands, Physics,
        StateType,
    };
    use fp_core::Vec2;
    use fp_formats::air::{AirFile, AnimAction, AnimFrame, BlendMode};
    use fp_formats::cns::CnsFile;
    use fp_vm::EvalContext;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// A synthetic state graph + animation set, the minimal pair the executor
    /// core ([`Character::tick_with`]) needs. Avoids constructing a real
    /// [`LoadedCharacter`], which would require a binary `SffFile`.
    struct Synth {
        states: HashMap<i32, CompiledState>,
        air: AirFile,
    }

    impl Synth {
        fn tick(&self, ch: &mut Character) -> TickReport {
            // The single-character synthetic harness: no opponent, default stage.
            ch.tick_with(&self.states, &self.air, None, StageView::default())
        }
    }

    // ---- Synthetic builders ------------------------------------------------

    /// Builds a compiled controller from a type and a set of trigger groups /
    /// params, compiling each expression. `groups` is a list of
    /// `(number, &[condition_src])`; `params` is `(name, src)`.
    fn ctrl(
        state_number: i32,
        kind: &str,
        triggerall: &[&str],
        groups: &[(u32, &[&str])],
        persistent: Option<&str>,
        params: &[(&str, &str)],
    ) -> CompiledController {
        CompiledController {
            state_number,
            label: String::new(),
            controller_type: Some(kind.to_string()),
            triggerall: triggerall.iter().map(|s| CompiledExpr::compile(s)).collect(),
            triggers: groups
                .iter()
                .map(|(n, conds)| CompiledTriggerGroup {
                    number: *n,
                    conditions: conds.iter().map(|s| CompiledExpr::compile(s)).collect(),
                })
                .collect(),
            persistent: persistent.map(CompiledExpr::compile),
            ignorehitpause: None,
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), CompiledParam::compile(v)))
                .collect(),
        }
    }

    /// The string-valued entry parameters of a synthetic statedef, bundled to
    /// keep the [`state`] builder under clippy's argument limit. Field order
    /// mirrors a MUGEN `[Statedef]` header: type, movetype, physics, anim, ctrl,
    /// velset, poweradd.
    #[derive(Clone, Copy, Default)]
    struct Entry<'a> {
        st: Option<&'a str>,
        mv: Option<&'a str>,
        ph: Option<&'a str>,
        anim: Option<&'a str>,
        ctrl: Option<&'a str>,
        velset: Option<&'a str>,
        poweradd: Option<&'a str>,
    }

    /// Builds a compiled state with the given entry params and controllers.
    fn state(number: i32, e: Entry<'_>, controllers: Vec<CompiledController>) -> CompiledState {
        CompiledState {
            number,
            state_type: e.st.map(str::to_string),
            movetype: e.mv.map(str::to_string),
            physics: e.ph.map(str::to_string),
            anim: e.anim.map(CompiledExpr::compile),
            ctrl: e.ctrl.map(CompiledExpr::compile),
            velset: e.velset.map(str::to_string),
            poweradd: e.poweradd.map(CompiledExpr::compile),
            controllers,
        }
    }

    /// Shorthand: a stand state with `type=S, physics=N` and no other entry
    /// params — the common case for controller-dispatch tests where physics and
    /// entry values are irrelevant.
    fn stand_n(number: i32, controllers: Vec<CompiledController>) -> CompiledState {
        state(
            number,
            Entry {
                st: Some("S"),
                ph: Some("N"),
                ..Entry::default()
            },
            controllers,
        )
    }

    /// A tiny single-action AIR file: action 0 with `n` frames each holding
    /// `ticks` ticks, looping at frame 0.
    fn tiny_air(action: i32, frames_ticks: &[i32]) -> AirFile {
        let frames: Vec<AnimFrame> = frames_ticks
            .iter()
            .map(|&t| AnimFrame {
                sprite: fp_core::SpriteId::new(0, 0),
                offset: Vec2::new(0, 0),
                ticks: t,
                flip_h: false,
                flip_v: false,
                blend: BlendMode::Normal,
                clsn1: Vec::new(),
                clsn2: Vec::new(),
            })
            .collect();
        let mut actions = HashMap::new();
        actions.insert(
            action,
            AnimAction {
                action_number: action,
                frames,
                loopstart: 0,
            },
        );
        AirFile { actions }
    }

    /// Builds a synthetic state graph + AIR pair from a list of compiled states
    /// and an AIR file.
    fn loaded(states: Vec<CompiledState>, air: AirFile) -> Synth {
        let mut map = HashMap::new();
        for s in states {
            map.insert(s.number, s);
        }
        Synth { states: map, air }
    }

    // ---- AC1: trigger-group gating + CB6 contiguity ------------------------

    #[test]
    fn triggerall_and_trigger_group_or_gate_correctly() {
        // Controller fires only when triggerall is true AND one numbered group is
        // fully true. Build a ChangeState gated on (triggerall: ctrl) and groups:
        // trigger1 = "Time > 100" (false), trigger2 = "StateNo = 0" (true).
        let walk = ctrl(
            0,
            "ChangeState",
            &["ctrl"],
            &[(1, &["Time > 100"]), (2, &["StateNo = 0"])],
            None,
            &[("value", "20")],
        );
        let st0 = state(0, Entry { st: Some("S"), mv: Some("I"), ph: Some("S"), anim: Some("0"), ..Entry::default() }, vec![walk]);
        let st20 = state(20, Entry { st: Some("S"), ph: Some("S"), anim: Some("20"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st0, st20], tiny_air(0, &[5, 5]));

        let mut ch = Character::new();
        ch.state_no = 0;
        ch.ctrl = true; // triggerall passes
        let report = lc.tick(&mut ch);
        // group 2 is true → transition fires.
        assert_eq!(report.transitions, 1);
        assert_eq!(ch.state_no, 20);
        assert_eq!(ch.prev_state_no, 0);
        assert_eq!(ch.state_time, 1); // reset then advanced one tick

        // With ctrl false, triggerall fails → no transition.
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.ctrl = false;
        let r2 = lc.tick(&mut ch2);
        assert_eq!(r2.transitions, 0);
        assert_eq!(ch2.state_no, 0);
    }

    #[test]
    fn cb6_contiguity_gap_drops_later_groups() {
        // Groups trigger1 (false), trigger2 (false), trigger4 (TRUE) with NO
        // trigger3. CB6 drops trigger4, so the controller must NOT fire even
        // though trigger4 is true.
        let c = ctrl(
            0,
            "ChangeState",
            &[],
            &[
                (1, &["0"]), // false
                (2, &["0"]), // false
                (4, &["1"]), // true, but post-gap → dead
            ],
            None,
            &[("value", "20")],
        );
        let st0 = stand_n(0, vec![c]);
        let st20 = stand_n(20, vec![]);
        let lc = loaded(vec![st0, st20], tiny_air(0, &[5]));

        let mut ch = Character::new();
        ch.state_no = 0;
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 0, "trigger4 after a gap must be dead (CB6)");
        assert_eq!(ch.state_no, 0);

        // Sanity: with trigger3 present (closing the gap) AND true, trigger4-style
        // group now fires. Make group 3 the true one.
        let c2 = ctrl(
            0,
            "ChangeState",
            &[],
            &[(1, &["0"]), (2, &["0"]), (3, &["1"])],
            None,
            &[("value", "20")],
        );
        let st0b = stand_n(0, vec![c2]);
        let lc2 = loaded(vec![st0b, stand_n(20, vec![])], tiny_air(0, &[5]));
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        assert_eq!(lc2.tick(&mut ch2).transitions, 1);
        assert_eq!(ch2.state_no, 20);
    }

    #[test]
    fn missing_trigger1_never_fires() {
        // A controller whose only group is trigger2 (no trigger1) cannot fire:
        // contiguity requires a trigger1 to start.
        let c = ctrl(0, "ChangeState", &[], &[(2, &["1"])], None, &[("value", "20")]);
        let lc = loaded(
            vec![
                stand_n(0, vec![c]),
                stand_n(20, vec![]),
            ],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        assert_eq!(lc.tick(&mut ch).transitions, 0);
        assert_eq!(ch.state_no, 0);
    }

    // ---- AC1: ChangeState transition updates state_no/prev/time ------------

    #[test]
    fn change_state_updates_cursor_and_resets_time() {
        let c = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "20")]);
        let st0 = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![c]);
        // Destination sets anim 20 and ctrl 1 on entry.
        let st20 = state(20, Entry { st: Some("A"), mv: Some("A"), ph: Some("A"), anim: Some("20"), ctrl: Some("1"), velset: Some("3, -5"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st0, st20], {
            // Two actions: 0 and 20.
            let mut air = tiny_air(0, &[5]);
            air.actions.insert(
                20,
                AnimAction { action_number: 20, frames: tiny_air(20, &[7]).actions.remove(&20).unwrap().frames, loopstart: 0 },
            );
            air
        });

        let mut ch = Character::new();
        ch.state_no = 0;
        ch.prev_state_no = -99;
        ch.state_time = 42;
        let report = lc.tick(&mut ch);

        assert_eq!(report.transitions, 1);
        assert_eq!(ch.state_no, 20);
        assert_eq!(ch.prev_state_no, 0);
        // state_time was reset to 0 on entry, then advanced one tick.
        assert_eq!(ch.state_time, 1);
        // Entry applied: type A, movetype A, physics A, anim 20, ctrl true.
        assert_eq!(ch.state_type, StateType::Air);
        assert_eq!(ch.move_type, MoveType::Attack);
        assert_eq!(ch.physics, Physics::Air);
        assert_eq!(ch.anim, 20);
        assert!(ch.ctrl);
        // velset 3,-5 applied; then air gravity (yaccel) added to y this tick.
        assert!((ch.vel.x - 3.0).abs() < 1e-6);
        let expected_y = -5.0 + CharacterConstants::default().movement.yaccel;
        assert!((ch.vel.y - expected_y).abs() < 1e-6);
    }

    // ---- AC2/AC3: velset + physics application -----------------------------

    #[test]
    fn velset_then_stand_friction_applies_each_tick() {
        // State 0: stand physics, velset 10,0. First tick: enter (velset 10),
        // then friction *0.85. Next tick: friction again.
        let st0 = state(0, Entry { st: Some("S"), mv: Some("I"), ph: Some("S"), anim: Some("0"), velset: Some("10, 0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st0], tiny_air(0, &[5]));
        let mut ch = Character::new();
        // Force an entry by transitioning into 0 via the executor's enter path:
        // start in a different state so entry runs. Simpler: set state_no=0 and
        // manually apply entry by ticking after a ChangeState. Here we instead
        // pre-seed velocity and rely on per-tick physics (no entry this tick).
        ch.state_no = 0;
        ch.physics = Physics::Stand;
        ch.vel = Vec2::new(10.0, 0.0);
        ch.constants = CharacterConstants::default();
        lc.tick(&mut ch);
        let f = CharacterConstants::default().movement.stand_friction;
        assert!((ch.vel.x - 10.0 * f).abs() < 1e-6, "stand friction applied");

        // Crouch physics uses the crouch coefficient.
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.physics = Physics::Crouch;
        ch2.vel = Vec2::new(8.0, 0.0);
        lc.tick(&mut ch2);
        let cf = CharacterConstants::default().movement.crouch_friction;
        assert!((ch2.vel.x - 8.0 * cf).abs() < 1e-6, "crouch friction applied");
    }

    #[test]
    fn air_physics_adds_gravity_and_none_does_nothing() {
        let st = state(0, Entry { st: Some("A"), ph: Some("A"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::Air;
        ch.vel = Vec2::new(1.0, -8.0);
        lc.tick(&mut ch);
        let g = CharacterConstants::default().movement.yaccel;
        assert!((ch.vel.y - (-8.0 + g)).abs() < 1e-6, "gravity added to y");
        assert!((ch.vel.x - 1.0).abs() < 1e-6, "air physics leaves x alone");

        // None physics: velocity untouched.
        let stn = stand_n(0, vec![]);
        let lcn = loaded(vec![stn], tiny_air(0, &[5]));
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.physics = Physics::None;
        ch2.vel = Vec2::new(2.0, 3.0);
        lcn.tick(&mut ch2);
        assert!((ch2.vel.x - 2.0).abs() < 1e-6);
        assert!((ch2.vel.y - 3.0).abs() < 1e-6);
    }

    // ---- A.P15: ground-plane Y clamp (falling characters land) -------------

    /// A falling character (downward = positive `vel.y`) integrates toward the
    /// floor and is clamped at [`GROUND_Y`] (`0`): `pos.y` never goes positive
    /// (below the floor), no matter how many ticks pass, and — critically — the
    /// clamp leaves `vel.y` untouched so the land trigger can still observe the
    /// downward velocity on the landing frame.
    #[test]
    fn falling_character_clamps_at_ground_and_vel_y_preserved() {
        // Physics::None so gravity does not alter vel.y: this isolates the
        // position clamp. Start above the floor (negative Y) with downward vel.
        let st = stand_n(0, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.pos = Vec2::new(0.0, -3.0); // 3 units above the floor
        ch.vel = Vec2::new(0.0, 2.0); // falling (downward)

        // Tick once: pos.y = -3 + 2 = -1, still airborne, not yet clamped.
        lc.tick(&mut ch);
        assert!((ch.pos.y - (-1.0)).abs() < 1e-6, "still airborne, got {}", ch.pos.y);
        assert!((ch.vel.y - 2.0).abs() < 1e-6, "vel.y untouched by integration");

        // Tick again: pos.y would integrate to -1 + 2 = +1 (below floor); the
        // clamp must hold it AT the floor (0), and must NOT modify vel.y.
        lc.tick(&mut ch);
        assert!((ch.pos.y - GROUND_Y).abs() < 1e-6, "clamped to floor, got {}", ch.pos.y);
        assert!(ch.pos.y <= GROUND_Y, "pos.y never positive (below floor)");
        assert!((ch.vel.y - 2.0).abs() < 1e-6, "clamp must NOT touch vel.y (land-trigger timing)");

        // Keep ticking with the same downward velocity: it stays pinned at 0.
        for _ in 0..10 {
            lc.tick(&mut ch);
            assert!(ch.pos.y <= GROUND_Y, "stays at/above floor, got {}", ch.pos.y);
        }
        assert!((ch.pos.y - GROUND_Y).abs() < 1e-6, "settled exactly at the floor");
    }

    /// Upward motion (negative Y, above the floor) is unaffected by the
    /// `min(_, GROUND_Y)` clamp, and a grounded character already at the floor
    /// with zero vertical velocity stays put.
    #[test]
    fn upward_motion_unaffected_and_grounded_unchanged() {
        let st = stand_n(0, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));

        // Rising character: pos.y goes more negative, never touched by the clamp.
        let mut up = Character::new();
        up.state_no = 0;
        up.physics = Physics::None;
        up.pos = Vec2::new(0.0, 0.0);
        up.vel = Vec2::new(0.0, -5.0); // upward
        lc.tick(&mut up);
        assert!((up.pos.y - (-5.0)).abs() < 1e-6, "upward motion passes through clamp");

        // Grounded character at the floor, no vertical velocity: stays at 0.
        let mut grounded = Character::new();
        grounded.state_no = 0;
        grounded.physics = Physics::None;
        grounded.pos = Vec2::new(0.0, GROUND_Y);
        grounded.vel = Vec2::new(0.0, 0.0);
        lc.tick(&mut grounded);
        assert!((grounded.pos.y - GROUND_Y).abs() < 1e-6, "grounded char unmoved by min(_,0)");
    }

    /// Gated real-KFM integration test (skips silently when the
    /// `test-assets/kfm` fixture is absent), in two parts:
    ///
    /// **Part A — the jump arc lands via the clamp.** Enter the jump-up air
    /// state 50 with the P4 jump velocity (negative y = upward, from
    /// `velocity.jump.up`) under air physics, exactly as Statedef 40's
    /// `AnimTime=0` VelSet → ChangeState 50 leaves the character. Gravity
    /// (`yaccel`) pulls it back down and the ground clamp must settle `pos.y`
    /// **exactly at the floor** (`GROUND_Y`) without ever sinking below it —
    /// the headline behavior this task adds (before the clamp the character sank
    /// forever).
    ///
    /// **Part B — common1's data land transition completes the loop.** Drive the
    /// air-fall state 5040, whose `[State 5040, 6]` carries common1's land rule
    /// (`Vel Y > 0 && Pos Y >= 0` → ChangeState 52). With a downward velocity
    /// and air physics, the clamp lands the character at `Pos Y = 0`, the land
    /// trigger fires (it can only fire because the clamp leaves `vel.y > 0`
    /// intact at the landing frame), and common1 carries the character into the
    /// grounded Jump Land state 52 (`type=S`, which then settles velocity and
    /// proceeds to Stand 0). This proves the falling → land → grounded loop the
    /// task targets.
    #[test]
    fn real_kfm_jump_lands_and_common1_land_transition_completes() {
        let def = test_asset("kfm/kfm.def");
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

        // ---- Part A: the jump arc returns to the floor and is clamped there --
        let mut jumper = Character::with_constants(lc.constants);
        jumper.facing = Facing::Right;
        jumper.pos = Vec2::new(0.0, GROUND_Y);
        jumper.change_state(&lc.states, 50); // air jump-up (sets type/physics=A)
        jumper.vel = Vec2::new(0.0, lc.constants.velocity.jump_up);
        assert!(jumper.vel.y < 0.0, "jump y must be upward (negative), got {}", jumper.vel.y);

        let mut peaked_airborne = false;
        let mut returned_to_ground = false;
        for _ in 0..240 {
            let _ = jumper.tick(&lc, None, StageView::default());
            // The character must NEVER sink below the floor (the bug this fixes).
            assert!(
                jumper.pos.y <= GROUND_Y + 1e-4,
                "character must never sink below the floor, got pos.y = {}",
                jumper.pos.y
            );
            if jumper.pos.y < -1.0 {
                peaked_airborne = true; // genuinely left the ground on the way up
            }
            if peaked_airborne && (jumper.pos.y - GROUND_Y).abs() < 1e-4 {
                returned_to_ground = true;
                break;
            }
        }
        assert!(peaked_airborne, "the jump should lift the character off the floor");
        assert!(
            returned_to_ground,
            "the falling character should settle back AT the floor (Pos Y = 0), \
             not sink past it; ended at pos.y = {}",
            jumper.pos.y
        );

        // ---- Part B: common1's `Vel Y > 0 && Pos Y >= 0` → 52 land transition -
        // State 5040 (air-fall) carries the land rule in this common1 fixture.
        let mut faller = Character::with_constants(lc.constants);
        faller.facing = Facing::Right;
        faller.pos = Vec2::new(0.0, -40.0); // start airborne, above the floor
        faller.change_state(&lc.states, 5040);
        faller.physics = Physics::Air; // gravity (yaccel) accelerates the fall
        faller.vel = Vec2::new(0.0, 1.0); // already moving downward

        let mut reached_land_state = false;
        for _ in 0..240 {
            let _ = faller.tick(&lc, None, StageView::default());
            assert!(
                faller.pos.y <= GROUND_Y + 1e-4,
                "faller must never sink below the floor, got pos.y = {}",
                faller.pos.y
            );
            // The data land transition carries the character into Jump Land (52),
            // a grounded stand-type state, or onward to Stand (0).
            if faller.state_no == 52 || faller.state_no == 0 {
                reached_land_state = true;
                break;
            }
        }
        assert!(
            reached_land_state,
            "common1's `Vel Y > 0 && Pos Y >= 0` land transition should carry the \
             landed character into a grounded state (52 Jump Land → Stand 0); \
             ended in state {} at pos.y = {}",
            faller.state_no, faller.pos.y
        );
        assert!(
            (faller.pos.y - GROUND_Y).abs() < 1e-3,
            "after landing, pos.y is settled at the floor; got {}",
            faller.pos.y
        );
    }

    /// The floor constant is the world origin (`Y = 0`) — pinned so a future
    /// stage `zoffset`/floor change is a deliberate, test-visible edit rather
    /// than a silent magic-literal drift. Y increases downward, so the clamp
    /// `min(pos.y, GROUND_Y)` keeps a player at or above this line.
    #[test]
    fn ground_y_constant_is_floor_zero() {
        assert!((GROUND_Y - 0.0).abs() < f32::EPSILON, "floor is the world origin Y=0");
    }

    // ---- A.P14: air-jump (double jump) engine built-in ---------------------

    /// Builds the synthetic state graph for the air-jump tests: an airborne idle
    /// state `0` (`type=A`, `physics=N` so the character holds its position and
    /// the height check is deterministic) plus the AirJump Start state `45`
    /// (also `type=A`, so the character stays airborne after the engine
    /// transition). Neither carries controllers — the air-jump transition is the
    /// engine built-in, not a CNS controller.
    fn air_jump_synth() -> Synth {
        let air_idle = state(
            0,
            Entry { st: Some("A"), ph: Some("N"), anim: Some("0"), ..Entry::default() },
            vec![],
        );
        let airjump_start = state(
            AIRJUMP_START_STATE,
            Entry { st: Some("A"), ph: Some("N"), anim: Some("0"), ..Entry::default() },
            vec![],
        );
        loaded(vec![air_idle, airjump_start], tiny_air(0, &[5]))
    }

    /// An airborne, in-control character with the given `airjump.num`, positioned
    /// well above the air-jump height, starting in the airborne idle state `0`.
    fn airborne_ctrl_char(airjump_num: i32, airjump_height: f32) -> Character {
        let consts = CharacterConstants {
            movement: MovementConstants {
                airjump_num,
                airjump_height,
                ..MovementConstants::default()
            },
            ..CharacterConstants::default()
        };
        let mut ch = Character::with_constants(consts);
        ch.state_no = 0;
        ch.state_type = StateType::Air;
        ch.physics = Physics::None;
        ch.ctrl = true;
        // High above the floor (up is negative Y) so the height gate passes.
        ch.pos = Vec2::new(0.0, -100.0);
        ch
    }

    /// AC2/AC3: an airborne ctrl char with a **fresh** up-press, count < num and
    /// above the height, transitions to state 45 and increments the count.
    #[test]
    fn air_jump_fresh_up_press_transitions_to_45_and_increments() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(1, 35.0);
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let report = synth.tick(&mut ch);
        assert_eq!(ch.state_no, AIRJUMP_START_STATE, "fresh up-press in the air → state 45");
        assert_eq!(ch.air_jump_count, 1, "the air-jump count is incremented");
        assert!(report.transitions >= 1, "the engine air-jump counts as a transition");
    }

    /// AC2/AC3: **holding** up does not burn a second air-jump — the second tick
    /// has no rising edge, so a char with `airjump.num = 2` still has one left.
    #[test]
    fn air_jump_held_up_does_not_burn_a_second_jump() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(2, 35.0);
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));

        // Tick 1: fresh press → one air-jump (into 45).
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 1, "first fresh press = one air-jump");
        assert_eq!(ch.state_no, AIRJUMP_START_STATE);

        // Put the character back into the airborne idle state but KEEP up held;
        // no new rising edge, so no further air-jump despite num = 2.
        ch.change_state(&synth.states, 0);
        ch.ctrl = true;
        ch.pos = Vec2::new(0.0, -100.0);
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 1, "held up (no rising edge) must NOT air-jump again");
        assert_eq!(ch.state_no, 0, "stays in the airborne idle state");
    }

    /// AC3: a fresh **release-then-press** of up does burn the second air-jump
    /// (the rising edge fires again), and then `count == num` blocks any further
    /// air-jump even with another fresh press.
    #[test]
    fn air_jump_count_equals_num_blocks_further_jumps() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(1, 35.0);

        // First fresh press → the single allowed air-jump.
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 1);

        // Release up (clears the held state), back to airborne idle.
        ch.set_command_source(Box::new(NoCommands));
        ch.change_state(&synth.states, 0);
        ch.ctrl = true;
        ch.pos = Vec2::new(0.0, -100.0);
        let _ = synth.tick(&mut ch); // up not held → up_held_prev = false

        // Fresh press again — but the allowance (num = 1) is already spent.
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 1, "count == num blocks further air-jumps");
        assert_eq!(ch.state_no, 0, "no transition to 45 once the allowance is spent");
    }

    /// AC2/AC3: below `airjump.height` (too close to the floor) blocks the
    /// air-jump even with a fresh up-press and an available allowance.
    #[test]
    fn air_jump_below_height_is_blocked() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(1, 35.0);
        // Only 10px above the floor; the gate needs pos.y <= -35.
        ch.pos = Vec2::new(0.0, -10.0);
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 0, "below airjump.height: no air-jump");
        assert_eq!(ch.state_no, 0, "stays airborne idle when too low");
    }

    /// AC2/AC3: a character with `airjump.num = 0` never air-jumps, regardless of
    /// a fresh up-press while airborne with control and above any height.
    #[test]
    fn air_jump_num_zero_never_triggers() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(0, 0.0);
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 0, "airjump.num = 0: counter never moves");
        assert_eq!(ch.state_no, 0, "airjump.num = 0: never transitions to 45");
    }

    /// AC2/AC3/AC5 (synthetic): landing resets the count, so the canonical
    /// jump → air-jump → land → jump → air-jump sequence works again. The
    /// grounded reset fires on any non-air state at tick start.
    #[test]
    fn air_jump_landing_resets_allowance() {
        let synth = air_jump_synth();
        // Add a grounded stand state 11 to "land" into.
        let mut states = synth.states;
        states.insert(
            11,
            stand_n(11, vec![]),
        );
        let synth = Synth { states, air: synth.air };

        let mut ch = airborne_ctrl_char(1, 35.0);

        // First airborne sequence: fresh up-press → air-jump (count 1).
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 1, "first air-jump used");

        // Land: enter a grounded (type=S) state. The next tick's grounded reset
        // restores the allowance.
        ch.change_state(&synth.states, 11);
        ch.set_command_source(Box::new(NoCommands)); // release up while landing
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 0, "grounded tick resets the air-jump count");

        // Fresh ground jump back into the air, then a fresh up-press air-jumps
        // again (the allowance was restored by landing).
        ch.change_state(&synth.states, 0);
        ch.state_type = StateType::Air;
        ch.ctrl = true;
        ch.pos = Vec2::new(0.0, -100.0);
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 1, "after landing, a fresh ground jump restores the air-jump");
        assert_eq!(ch.state_no, AIRJUMP_START_STATE, "air-jump works again after landing");
    }

    // =====================================================================
    // Proctor (task A.P14): complementary air-jump edge cases, layered on
    // top of Forge's executor tests. These exercise the gate conditions
    // individually (ctrl, airborne, height boundary, fresh-edge tracking on
    // the early-return path) plus the multi-jump and never-panic guarantees.
    // =====================================================================

    /// A **grounded** character (`StateType` != Air) with a fresh up-press, an
    /// available allowance, control, and "above the height" never air-jumps: the
    /// built-in is gated on being airborne. (A grounded up-press is a ground jump,
    /// handled by the locomotion built-in, not the air-jump built-in.) The
    /// grounded reset also keeps the count pinned at 0.
    #[test]
    fn air_jump_grounded_never_triggers() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(1, 35.0);
        // Force the character GROUNDED despite being high in the air: the gate is
        // `state_type == Air`, not a position check, so this isolates that gate.
        ch.state_type = StateType::Standing;
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 0, "a grounded character never air-jumps");
        assert_eq!(ch.state_no, 0, "no transition to 45 while grounded");
    }

    /// Without `ctrl`, an airborne character with a fresh up-press, an available
    /// allowance, and above the height does NOT air-jump: MUGEN gates the air-jump
    /// on the player having control (you cannot air-jump out of hitstun / a
    /// no-control air state).
    #[test]
    fn air_jump_without_ctrl_is_blocked() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(1, 35.0);
        ch.ctrl = false; // no control
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 0, "no ctrl: the air-jump is blocked");
        assert_eq!(ch.state_no, 0, "no ctrl: no transition to 45");
    }

    /// The height gate is `pos.y <= -airjump_height`, so a character sitting
    /// **exactly** at the boundary (`pos.y == -airjump_height`) is permitted to
    /// air-jump (the comparison is inclusive). One pixel closer to the floor
    /// (`pos.y == -airjump_height + 1`) is blocked. This pins the exact boundary.
    #[test]
    fn air_jump_height_boundary_is_inclusive() {
        let synth = air_jump_synth();

        // Exactly at the boundary: -35.0 with airjump_height = 35 → permitted.
        let mut at = airborne_ctrl_char(1, 35.0);
        at.pos = Vec2::new(0.0, -35.0);
        at.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut at);
        assert_eq!(at.air_jump_count, 1, "pos.y == -airjump_height is high enough (inclusive)");
        assert_eq!(at.state_no, AIRJUMP_START_STATE);

        // One pixel below the boundary (closer to the floor): blocked.
        let mut below = airborne_ctrl_char(1, 35.0);
        below.pos = Vec2::new(0.0, -34.0);
        below.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut below);
        assert_eq!(below.air_jump_count, 0, "one pixel below the boundary is too low");
        assert_eq!(below.state_no, 0);
    }

    /// `airjump.height = 0` (the default) imposes no minimum height: an airborne
    /// character one pixel off the floor can air-jump immediately. Combined with
    /// the gated-on-`airjump_num` guard this proves height and count are
    /// independent gates.
    #[test]
    fn air_jump_zero_height_permits_immediately() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(1, 0.0);
        ch.pos = Vec2::new(0.0, -1.0); // barely off the floor
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 1, "airjump.height = 0: any airborne height qualifies");
        assert_eq!(ch.state_no, AIRJUMP_START_STATE);
    }

    /// A negative `airjump.num` (messy content) is treated exactly like `0`: the
    /// built-in is gated on `airjump_num > 0`, so a negative value never
    /// air-jumps and never panics. (The loader stores whatever integer it reads;
    /// the executor's gate must tolerate a nonsense value.)
    #[test]
    fn air_jump_negative_num_never_triggers() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(-3, 35.0);
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 0, "negative airjump.num behaves like 0 (no air jump)");
        assert_eq!(ch.state_no, 0);
    }

    /// `airjump.num = 0` still tracks the fresh-press edge (`up_held_prev`) so the
    /// built-in's early return does not desync edge detection. This guards the
    /// ordering in `update_air_jump`: the held state is recorded BEFORE the
    /// `airjump_num <= 0` early return, so a later change to a positive allowance
    /// (e.g. a state controller) sees a correct edge rather than a stale one.
    #[test]
    fn air_jump_num_zero_still_tracks_up_held_prev() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(0, 0.0);
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        assert!(!ch.up_held_prev, "starts un-held");
        let _ = synth.tick(&mut ch);
        assert!(
            ch.up_held_prev,
            "even with airjump.num = 0, the held-up state is recorded for next tick's edge"
        );
        assert_eq!(ch.air_jump_count, 0, "and still no air-jump");
    }

    /// A multi-jump character (`airjump.num = 2`) air-jumps **twice** across two
    /// distinct fresh up-presses (release between them), then is blocked on the
    /// third press — proving the count gate (`air_jump_count < airjump_num`)
    /// allows exactly `airjump_num` air-jumps per airborne stretch.
    #[test]
    fn air_jump_double_allows_exactly_two() {
        let synth = air_jump_synth();
        let mut ch = airborne_ctrl_char(2, 35.0);

        // Press 1 (fresh): first air-jump.
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 1, "first fresh press → air-jump 1");

        // Release up, stay airborne (back to idle, still high, still ctrl).
        ch.set_command_source(Box::new(NoCommands));
        ch.change_state(&synth.states, 0);
        ch.ctrl = true;
        ch.pos = Vec2::new(0.0, -100.0);
        let _ = synth.tick(&mut ch); // up not held → up_held_prev cleared
        assert_eq!(ch.air_jump_count, 1, "release does not change the count");

        // Press 2 (fresh again): second air-jump (allowance is 2).
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 2, "second fresh press → air-jump 2");
        assert_eq!(ch.state_no, AIRJUMP_START_STATE);

        // Release + press 3 (fresh): blocked, count == num.
        ch.set_command_source(Box::new(NoCommands));
        ch.change_state(&synth.states, 0);
        ch.ctrl = true;
        ch.pos = Vec2::new(0.0, -100.0);
        let _ = synth.tick(&mut ch);
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
        let _ = synth.tick(&mut ch);
        assert_eq!(ch.air_jump_count, 2, "third press blocked: count (2) == num (2)");
        assert_eq!(ch.state_no, 0, "no third air-jump");
    }

    /// A default-constructed [`Character`] (the [`NoCommands`] source, no
    /// air-jump allowance) ticks through the air-jump built-in without panicking
    /// and never air-jumps — the engine-wide "never crash on bad/absent content"
    /// guarantee applied to this built-in.
    #[test]
    fn air_jump_default_character_is_safe_noop() {
        let synth = air_jump_synth();
        let mut ch = Character::new(); // NoCommands, airjump_num = 0
        ch.state_no = 0;
        ch.state_type = StateType::Air;
        ch.physics = Physics::None;
        ch.ctrl = true;
        ch.pos = Vec2::new(0.0, -100.0);
        // Tick several times: no holdup source, no allowance → pure no-op.
        for _ in 0..5 {
            let _ = synth.tick(&mut ch);
        }
        assert_eq!(ch.air_jump_count, 0, "default character never air-jumps");
        assert_eq!(ch.state_no, 0, "and never transitions to 45");
    }

    /// Gated real-KFM air-jump integration test (skips silently when the
    /// `test-assets/kfm` fixture is absent). KFM authors `airjump.num = 1`, so a
    /// grounded jump (state 50) followed, while airborne and above the air-jump
    /// height, by a fresh up-press drives the engine air-jump built-in into
    /// AirJump Start (state 45) **exactly once**; a second held/fresh press is
    /// then blocked by the spent allowance.
    #[test]
    fn real_kfm_air_jump_reaches_state_45_once() {
        let def = test_asset("kfm/kfm.def");
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
        // KFM authors airjump.num = 1.
        assert!(
            lc.constants.movement.airjump_num >= 1,
            "KFM should author airjump.num >= 1; got {}",
            lc.constants.movement.airjump_num
        );

        let mut ch = Character::with_constants(lc.constants);
        ch.facing = Facing::Right;
        // Enter the jump-up state 50 with the authored jump velocity and rise.
        // The player has control while airborne (MUGEN grants ctrl in jumpstart
        // before reaching 50); model that directly.
        ch.change_state(&lc.states, 50);
        ch.ctrl = true;
        ch.vel = Vec2::new(0.0, lc.constants.velocity.jump_up);
        // Up was NOT held before the air-jump press, so the edge is fresh.
        ch.up_held_prev = false;

        let mut reached_45 = 0u32;
        let mut prev_state = ch.state_no;
        let mut pressing_up = false;
        for _ in 0..240 {
            // Start holding up only once the character is airborne, above the
            // air-jump height, and in control — a clean fresh up-press (the
            // rising edge) at that point, exactly as a player taps up again at
            // the apex. Holding it thereafter must NOT burn a second jump.
            let high_enough =
                ch.pos.y <= GROUND_Y - lc.constants.movement.airjump_height - 1.0;
            if !pressing_up && ch.state_type == StateType::Air && high_enough && ch.ctrl {
                ch.set_command_source(Box::new(ActiveCommands::from_names(["holdup"])));
                pressing_up = true;
            }
            let _ = ch.tick(&lc, None, StageView::default());
            // Count distinct *entries* into AirJump Start (45), not ticks spent
            // in it, so a multi-tick stay in 45 is one air-jump.
            if ch.state_no == AIRJUMP_START_STATE && prev_state != AIRJUMP_START_STATE {
                reached_45 += 1;
            }
            prev_state = ch.state_no;
        }
        assert_eq!(
            reached_45, 1,
            "KFM air-jump (airjump.num = 1) should reach AirJump Start (45) exactly once; \
             held up must not burn a second jump"
        );
        // The allowance is 1; the count must not exceed airjump.num.
        assert!(
            ch.air_jump_count <= lc.constants.movement.airjump_num,
            "air-jump count {} must not exceed airjump.num {}",
            ch.air_jump_count,
            lc.constants.movement.airjump_num
        );
    }

    /// Synthetic full jump arc driven by **air gravity** (no real fixtures
    /// required), proving AC2's "downward velocity integrates toward the floor
    /// and is clamped at `GROUND_Y`" under the real physics path: `apply_physics`
    /// adds `yaccel` to `vel.y` each tick, then `integrate_position` advances and
    /// clamps. The character launches upward (negative `vel.y`), peaks, gravity
    /// reverses it, and the clamp settles it **exactly** at the floor without
    /// ever sinking below it.
    #[test]
    fn synthetic_gravity_fall_integrates_toward_floor_and_clamps() {
        // Air state so `Physics::Air` adds gravity (yaccel) to vel.y each tick.
        let air = state(
            0,
            Entry { st: Some("A"), ph: Some("A"), anim: Some("0"), ..Entry::default() },
            vec![],
        );
        let lc = loaded(vec![air], tiny_air(0, &[5]));
        let yaccel = CharacterConstants::default().movement.yaccel;
        assert!(yaccel > 0.0, "downward gravity must be positive (Y increases downward)");

        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::Air;
        ch.pos = Vec2::new(0.0, GROUND_Y); // start on the floor
        ch.vel = Vec2::new(0.0, -8.4); // launch upward (negative Y)

        let mut peaked_airborne = false;
        let mut settled = false;
        let mut min_y = 0.0_f32; // most-negative (highest) point reached
        for _ in 0..200 {
            lc.tick(&mut ch);
            // The defining behavior: the player is NEVER below the floor.
            assert!(
                ch.pos.y <= GROUND_Y + 1e-5,
                "character must never sink below the floor, got pos.y = {}",
                ch.pos.y
            );
            min_y = min_y.min(ch.pos.y);
            if ch.pos.y < -1.0 {
                peaked_airborne = true;
            }
            if peaked_airborne && (ch.pos.y - GROUND_Y).abs() < 1e-5 {
                settled = true;
                break;
            }
        }
        assert!(peaked_airborne, "gravity test should lift the character (min_y = {min_y})");
        assert!(
            settled,
            "gravity should pull the falling character back to rest AT the floor, \
             not sink past it; ended at pos.y = {}",
            ch.pos.y
        );
        // vel.y is left for the state machine to settle: still positive (downward)
        // at the landing frame — the clamp touched position only.
        assert!(
            ch.vel.y > 0.0,
            "clamp leaves vel.y downward for the land trigger; got {}",
            ch.vel.y
        );
    }

    /// Boundary: a character arriving **exactly** on the floor (`pos.y == 0`)
    /// with a downward velocity is held at the floor every tick, and the clamp
    /// never nudges `pos.y` positive nor zeroes `vel.y`. This is the precise
    /// landing-frame condition common1's land rule (`Vel Y > 0 && Pos Y >= 0`)
    /// observes: position pinned at `0`, velocity still downward.
    #[test]
    fn landing_frame_at_floor_holds_position_and_keeps_vel_y() {
        let st = stand_n(0, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None; // isolate the clamp from gravity
        ch.pos = Vec2::new(0.0, GROUND_Y); // already exactly on the floor
        ch.vel = Vec2::new(0.0, 3.0); // still moving downward at the landing frame

        for _ in 0..5 {
            lc.tick(&mut ch);
            assert!(
                (ch.pos.y - GROUND_Y).abs() < 1e-6,
                "held exactly at the floor, got {}",
                ch.pos.y
            );
            assert!(ch.pos.y <= GROUND_Y, "never below the floor");
            assert!(
                (ch.vel.y - 3.0).abs() < 1e-6,
                "clamp must not zero vel.y (land trigger needs Vel Y > 0); got {}",
                ch.vel.y
            );
        }
    }

    // ---- AC1: persistent semantics -----------------------------------------

    #[test]
    fn persistent_zero_fires_once_per_entry() {
        // persistent=0 VelAdd: should fire on the first qualifying tick only,
        // even though its trigger is true every tick.
        let c = ctrl(0, "VelAdd", &[], &[(1, &["1"])], Some("0"), &[("x", "1")]);
        let lc = loaded(
            vec![stand_n(0, vec![c])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(0.0, 0.0);
        lc.tick(&mut ch); // fires: x += 1
        lc.tick(&mut ch); // does NOT fire (once per entry)
        lc.tick(&mut ch); // does NOT fire
        assert!((ch.vel.x - 1.0).abs() < 1e-6, "persistent=0 fires once, got {}", ch.vel.x);
    }

    #[test]
    fn persistent_default_fires_every_tick() {
        // No persistent param → default 1 → fires every qualifying tick.
        let c = ctrl(0, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "1")]);
        let lc = loaded(
            vec![stand_n(0, vec![c])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        lc.tick(&mut ch);
        lc.tick(&mut ch);
        assert!((ch.vel.x - 3.0).abs() < 1e-6, "default persistent fires every tick");
    }

    #[test]
    fn persistent_n_fires_every_nth_tick() {
        // persistent=2 → fires on the 2nd, 4th, … qualifying tick.
        let c = ctrl(0, "VelAdd", &[], &[(1, &["1"])], Some("2"), &[("x", "1")]);
        let lc = loaded(
            vec![stand_n(0, vec![c])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch); // count 1: 1 % 2 != 0 → no fire
        assert!((ch.vel.x - 0.0).abs() < 1e-6);
        lc.tick(&mut ch); // count 2: fire
        assert!((ch.vel.x - 1.0).abs() < 1e-6);
        lc.tick(&mut ch); // count 3: no fire
        lc.tick(&mut ch); // count 4: fire
        assert!((ch.vel.x - 2.0).abs() < 1e-6);
    }

    // ---- AC3: animation element/time advance from AIR durations ------------

    #[test]
    fn animation_advances_and_loops_from_air_durations() {
        // Action 0: two frames, each holding 2 ticks; loops at 0.
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[2, 2]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.anim_elem = 0;
        ch.anim_elem_time = 0;

        lc.tick(&mut ch); // t=1 in elem 0
        assert_eq!(ch.anim_elem, 0);
        assert_eq!(ch.anim_elem_time, 1);
        lc.tick(&mut ch); // t=2 → reaches dur, advance to elem 1, reset time
        assert_eq!(ch.anim_elem, 1);
        assert_eq!(ch.anim_elem_time, 0);
        lc.tick(&mut ch); // elem 1, t=1
        assert_eq!(ch.anim_elem, 1);
        assert_eq!(ch.anim_elem_time, 1);
        lc.tick(&mut ch); // elem 1 done → loop back to 0
        assert_eq!(ch.anim_elem, 0);
        assert_eq!(ch.anim_elem_time, 0);
    }

    #[test]
    fn hold_forever_frame_never_advances() {
        // A single frame with ticks = -1 holds forever.
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[-1]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        for _ in 0..10 {
            lc.tick(&mut ch);
        }
        assert_eq!(ch.anim_elem, 0, "hold-forever frame stays on element 0");
    }

    // ---- AC4: controller dispatch coverage + safe no-op fallthrough --------

    #[test]
    fn vel_set_and_ctrl_set_dispatch() {
        let vset = ctrl(0, "VelSet", &[], &[(1, &["1"])], None, &[("x", "4"), ("y", "-2")]);
        let cset = ctrl(0, "CtrlSet", &[], &[(1, &["1"])], None, &[("value", "1")]);
        let st = stand_n(0, vec![vset, cset]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.ctrl = false;
        lc.tick(&mut ch);
        assert!((ch.vel.x - 4.0).abs() < 1e-6);
        assert!((ch.vel.y - (-2.0)).abs() < 1e-6);
        assert!(ch.ctrl, "CtrlSet value=1 enabled control");
    }

    #[test]
    fn null_and_unknown_controllers_are_safe_noops() {
        // Null + an unrecognized controller both run without effect or panic.
        let null = ctrl(0, "Null", &[], &[(1, &["1"])], None, &[]);
        let bogus = ctrl(0, "TotallyMadeUpController", &[], &[(1, &["1"])], None, &[("x", "9")]);
        let st = stand_n(0, vec![null, bogus]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(7.0, 7.0);
        let report = lc.tick(&mut ch);
        // Both qualified and "fired" (dispatch ran), but neither changed velocity.
        assert_eq!(report.controllers_fired, 2);
        assert!((ch.vel.x - 7.0).abs() < 1e-6);
        assert!((ch.vel.y - 7.0).abs() < 1e-6);
        assert_eq!(ch.state_no, 0);
    }

    // ---- AC1: special-state order (-3/-2/-1 run before current) ------------

    #[test]
    fn special_states_run_before_current() {
        // -2 has a VelAdd x+=10 (always). Current state 0 has a VelAdd x+=1.
        // Both should fire in one tick: -2 first, then current.
        let s_neg2 = ctrl(-2, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "10")]);
        let s_cur = ctrl(0, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "1")]);
        let lc = loaded(
            vec![
                stand_n(-2, vec![s_neg2]),
                stand_n(0, vec![s_cur]),
            ],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(0.0, 0.0);
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 2);
        assert!((ch.vel.x - 11.0).abs() < 1e-6, "both -2 and current fired");
    }

    #[test]
    fn special_state_minus1_stops_at_first_changestate() {
        // Two always-true ChangeStates in [Statedef -1]: the first (an authored
        // special analog) must win and the second (an engine built-in analog
        // appended after it, task 7.3 part B) must NOT also fire. MUGEN scans -1
        // top-down and stops at the first state change. Regression test for the
        // 7.3-fix priority guarantee (without it, the second would redirect 100->200).
        let first = ctrl(-1, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "100")]);
        let second = ctrl(-1, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "200")]);
        let lc = loaded(
            vec![
                stand_n(-1, vec![first, second]),
                stand_n(0, vec![]),
                stand_n(100, vec![]),
                stand_n(200, vec![]),
            ],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let _ = lc.tick(&mut ch);
        assert_eq!(
            ch.state_no, 100,
            "first -1 ChangeState wins; the second must be skipped after the state change"
        );
    }

    // ---- AC4: never panics on unknown states / cyclic graph ----------------

    #[test]
    fn unknown_current_state_does_not_panic() {
        let lc = loaded(
            vec![stand_n(0, vec![])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 999; // not in the graph
        // Must not panic; cursor stays, time/anim advance harmlessly.
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 0);
        assert_eq!(ch.state_no, 999);
    }

    #[test]
    fn cyclic_change_state_is_bounded() {
        // A ↔ B infinite ChangeState loop must hit the cap and stop, not hang.
        let a = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "1")]);
        let b = ctrl(1, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "0")]);
        let lc = loaded(
            vec![
                stand_n(0, vec![a]),
                stand_n(1, vec![b]),
            ],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        let report = lc.tick(&mut ch);
        assert!(report.transition_cap_hit, "cyclic graph must hit the cap");
        // The character is left in a valid state (0 or 1), never panicking.
        assert!(ch.state_no == 0 || ch.state_no == 1);
    }

    // ---- helper-fn unit coverage ------------------------------------------

    #[test]
    fn contiguous_groups_respects_gaps() {
        let mk = |n: u32| CompiledTriggerGroup {
            number: n,
            conditions: vec![CompiledExpr::compile("1")],
        };
        // 1,2,4 → [1,2]
        let g = vec![mk(1), mk(2), mk(4)];
        let kept: Vec<u32> = contiguous_groups(&g).iter().map(|x| x.number).collect();
        assert_eq!(kept, vec![1, 2]);
        // 2,3 (no 1) → []
        let g2 = vec![mk(2), mk(3)];
        assert!(contiguous_groups(&g2).is_empty());
        // out-of-order 3,1,2 → [1,2,3]
        let g3 = vec![mk(3), mk(1), mk(2)];
        let kept3: Vec<u32> = contiguous_groups(&g3).iter().map(|x| x.number).collect();
        assert_eq!(kept3, vec![1, 2, 3]);
    }

    #[test]
    fn persistent_allows_matrix() {
        // once-per-entry
        assert!(persistent_allows(0, 1));
        assert!(!persistent_allows(0, 2));
        // every tick
        assert!(persistent_allows(1, 1));
        assert!(persistent_allows(1, 7));
        // every nth
        assert!(!persistent_allows(3, 1));
        assert!(!persistent_allows(3, 2));
        assert!(persistent_allows(3, 3));
        assert!(persistent_allows(3, 6));
        // negative → default (every tick)
        assert!(persistent_allows(-5, 4));
    }

    #[test]
    fn parse_velset_handles_scalar_and_pair_and_garbage() {
        assert_eq!(parse_velset("3, -5"), Some((3.0, -5.0)));
        assert_eq!(parse_velset("10"), Some((10.0, 0.0)));
        // Non-numeric first component → None (leave velocity unchanged).
        assert_eq!(parse_velset("garbage"), None);
        // Non-numeric second component → y defaults to 0.
        assert_eq!(parse_velset("4, nope"), Some((4.0, 0.0)));
    }

    // ---- AC2: CnsFile → CompiledState entry params round-trip --------------

    #[test]
    fn entry_params_from_real_cns_text() {
        // Parse a statedef through the real CNS parser, compile it, and verify
        // the executor applies its entry params on a ChangeState into it.
        let cns = CnsFile::from_str(
            "[Statedef 0]\ntype = S\nphysics = S\nanim = 0\nctrl = 1\n\
             [State 0, go]\ntype = ChangeState\ntrigger1 = Time >= 0\nvalue = 100\n\
             [Statedef 100]\ntype = A\nmovetype = A\nphysics = A\nanim = 100\nvelset = 0, -8\n",
        )
        .unwrap();
        let s0 = CompiledState::from_parsed(&cns.statedefs[0]);
        let s100 = CompiledState::from_parsed(&cns.statedefs[1]);
        let lc = loaded(vec![s0, s100], {
            let mut air = tiny_air(0, &[5]);
            air.actions.insert(100, AnimAction { action_number: 100, frames: tiny_air(100, &[5]).actions.remove(&100).unwrap().frames, loopstart: 0 });
            air
        });
        let mut ch = Character::new();
        ch.state_no = 0;
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 1);
        assert_eq!(ch.state_no, 100);
        assert_eq!(ch.state_type, StateType::Air);
        assert_eq!(ch.move_type, MoveType::Attack);
        assert_eq!(ch.physics, Physics::Air);
        assert_eq!(ch.anim, 100);
        // velset 0,-8 then air gravity added.
        let g = CharacterConstants::default().movement.yaccel;
        assert!((ch.vel.y - (-8.0 + g)).abs() < 1e-6);
    }

    // ---- AC1: command-gated transition (the canonical KFM walk pattern) ----

    #[test]
    fn command_gated_change_state() {
        let walk = ctrl(
            0,
            "ChangeState",
            &["ctrl"],
            &[(1, &["command = \"holdfwd\""])],
            None,
            &[("value", "20")],
        );
        let lc = loaded(
            vec![
                state(0, Entry { st: Some("S"), mv: Some("I"), ph: Some("S"), anim: Some("0"), ..Entry::default() }, vec![walk]),
                state(20, Entry { st: Some("S"), ph: Some("S"), anim: Some("20"), ..Entry::default() }, vec![]),
            ],
            {
                let mut air = tiny_air(0, &[5]);
                air.actions.insert(20, AnimAction { action_number: 20, frames: tiny_air(20, &[5]).actions.remove(&20).unwrap().frames, loopstart: 0 });
                air
            },
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.ctrl = true;
        // No command active → no transition.
        assert_eq!(lc.tick(&mut ch).transitions, 0);
        assert_eq!(ch.state_no, 0);
        // holdfwd active → transition to 20.
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdfwd"])));
        assert_eq!(lc.tick(&mut ch).transitions, 1);
        assert_eq!(ch.state_no, 20);
    }

    // ---- AC5: gated real-KFM tick (skips when test-assets absent) ----------

    fn test_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join(rel)
    }

    #[test]
    fn real_kfm_ticks_without_panicking() {
        let def = test_asset("kfm/kfm.def");
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
        // Start KFM in its stand state (common1 [Statedef 0]).
        let mut ch = Character::with_constants(lc.constants);
        ch.state_no = 0;
        ch.anim = 0;
        ch.ctrl = true;
        // Tick a few frames; must never panic and must keep a valid cursor.
        for _ in 0..30 {
            let _ = ch.tick(&lc, None, StageView::default());
            // state_time and anim cursors stay non-negative / in-range-ish.
            assert!(ch.state_time >= 0);
            assert!(ch.anim_elem >= 0);
        }
        // Constants were read from kfm.cns: KFM authors these values.
        assert_eq!(lc.constants.size.ground_front, 16);
        assert_eq!(lc.constants.size.height, 60);
        assert!((lc.constants.velocity.walk_fwd.x - 2.4).abs() < 1e-4);
        assert!((lc.constants.movement.yaccel - 0.44).abs() < 1e-4);
        assert!((lc.constants.movement.stand_friction - 0.85).abs() < 1e-4);
    }

    // =====================================================================
    // Task 6.6: Power gain — Statedef `poweradd`-on-entry + PowerAdd/PowerSet
    // controllers. The super meter must actually fill so gated supers
    // (`power >= 1000`) become reachable. All synthetic except the gated
    // real-KFM test at the end of this block.
    // =====================================================================

    /// AC1: entering a state with `poweradd = 10` raises power by 10, and a
    /// re-entry adds again (the add is once-per-entry, not once-ever). Drives
    /// the real `enter_state` path directly so the assertion is about entry,
    /// not per-tick controller scheduling.
    #[test]
    fn poweradd_on_entry_adds_once_per_entry() {
        // State 0: no poweradd. State 1: poweradd=10. Each entry into state 1
        // bumps power by 10; entering state 0 adds nothing.
        let st0 = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let st1 = state(
            1,
            Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), poweradd: Some("10"), ..Entry::default() },
            vec![],
        );
        let lc = loaded(vec![st0, st1], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        assert_eq!(ch.power, 0);

        ch.enter_state(&lc.states, 1, EvalEnv::self_only());
        assert_eq!(ch.power, 10, "first entry into state 1 added 10");

        ch.enter_state(&lc.states, 0, EvalEnv::self_only());
        assert_eq!(ch.power, 10, "state 0 has no poweradd");

        ch.enter_state(&lc.states, 1, EvalEnv::self_only());
        assert_eq!(ch.power, 20, "re-entry adds another 10");
    }

    /// AC1/AC3: `poweradd`-on-entry clamps at `power_max` and never exceeds it,
    /// even with a huge authored value.
    #[test]
    fn poweradd_on_entry_clamps_at_power_max() {
        let go = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "5")]);
        let dest = state(
            5,
            Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), poweradd: Some("999999"), ..Entry::default() },
            vec![],
        );
        let lc = loaded(
            vec![state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![go]), dest],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power_max = 1000;
        ch.power = 990;
        lc.tick(&mut ch);
        assert_eq!(ch.state_no, 5);
        assert_eq!(ch.power, 1000, "clamped to power_max");
    }

    /// AC1: a state with NO `poweradd` adds nothing on entry.
    #[test]
    fn entry_without_poweradd_leaves_power_unchanged() {
        let go = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "5")]);
        let dest = state(5, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(
            vec![state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![go]), dest],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power = 250;
        lc.tick(&mut ch);
        assert_eq!(ch.state_no, 5);
        assert_eq!(ch.power, 250, "no poweradd -> power unchanged");
    }

    /// AC2/AC3: `PowerAdd` controller adds `value` and clamps at `power_max`.
    #[test]
    fn power_add_controller_adds_and_clamps_high() {
        let add = ctrl(0, "PowerAdd", &[], &[(1, &["1"])], Some("0"), &[("value", "300")]);
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![add]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power_max = 1000;
        ch.power = 900;
        lc.tick(&mut ch);
        assert_eq!(ch.power, 1000, "900 + 300 clamped to power_max 1000");
    }

    /// AC2/AC3: `PowerAdd` with a negative `value` clamps at `0` (never goes
    /// below the floor).
    #[test]
    fn power_add_controller_clamps_low() {
        let add = ctrl(0, "PowerAdd", &[], &[(1, &["1"])], Some("0"), &[("value", "-500")]);
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![add]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power = 200;
        lc.tick(&mut ch);
        assert_eq!(ch.power, 0, "200 - 500 clamped to floor 0");
    }

    /// AC2/AC3: `PowerSet` controller assigns `value` and clamps at both ends.
    #[test]
    fn power_set_controller_sets_and_clamps() {
        // Set above power_max -> clamps high.
        let set_hi = ctrl(0, "PowerSet", &[], &[(1, &["1"])], Some("0"), &[("value", "5000")]);
        let st_hi = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![set_hi]);
        let lc_hi = loaded(vec![st_hi], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power_max = 3000;
        ch.power = 100;
        lc_hi.tick(&mut ch);
        assert_eq!(ch.power, 3000, "PowerSet 5000 clamped to power_max 3000");

        // Set below 0 -> clamps low.
        let set_lo = ctrl(0, "PowerSet", &[], &[(1, &["1"])], Some("0"), &[("value", "-7")]);
        let st_lo = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![set_lo]);
        let lc_lo = loaded(vec![st_lo], tiny_air(0, &[5]));
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.power = 500;
        lc_lo.tick(&mut ch2);
        assert_eq!(ch2.power, 0, "PowerSet -7 clamped to floor 0");
    }

    /// A.P19: AttackMulSet / DefenceMulSet set the runtime damage multipliers;
    /// a missing `value` is a safe no-op (multiplier unchanged).
    #[test]
    fn attack_defence_mul_set_controllers() {
        let mk = |kind: &str, params: &[(&str, &str)]| {
            let c = ctrl(0, kind, &[], &[(1, &["1"])], Some("0"), params);
            let st = state(
                0,
                Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() },
                vec![c],
            );
            loaded(vec![st], tiny_air(0, &[5]))
        };

        let mut ch = Character::new();
        ch.state_no = 0;
        mk("AttackMulSet", &[("value", "2.5")]).tick(&mut ch);
        assert!((ch.attack_mul - 2.5).abs() < 1e-6, "AttackMulSet sets attack_mul");

        let mut ch2 = Character::new();
        ch2.state_no = 0;
        mk("DefenceMulSet", &[("value", "0.5")]).tick(&mut ch2);
        assert!((ch2.defence_mul - 0.5).abs() < 1e-6, "DefenceMulSet sets defence_mul");

        // No `value` -> no-op, multiplier stays the default 1.0.
        let mut ch3 = Character::new();
        ch3.state_no = 0;
        mk("AttackMulSet", &[]).tick(&mut ch3);
        assert!((ch3.attack_mul - 1.0).abs() < 1e-6, "no value -> attack_mul unchanged");
    }

    /// AC2/AC3: `PowerAdd`/`PowerSet` with a missing `value` is a safe no-op
    /// (power unchanged, no panic).
    #[test]
    fn power_controllers_missing_value_is_noop() {
        let add = ctrl(0, "PowerAdd", &[], &[(1, &["1"])], Some("0"), &[]);
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![add]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power = 333;
        lc.tick(&mut ch);
        assert_eq!(ch.power, 333, "PowerAdd with no value adds nothing");
    }

    // ---- 6.6 (Proctor): additional edge/error-path coverage layered on top of
    // Forge's power tests. ----

    /// AC1/AC3: a NEGATIVE `poweradd` on entry drains the meter and clamps at the
    /// `0` floor (poweradd can subtract; it never underflows below 0).
    #[test]
    fn poweradd_on_entry_negative_clamps_at_floor() {
        let drain = state(
            7,
            Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), poweradd: Some("-1000"), ..Entry::default() },
            vec![],
        );
        let lc = loaded(
            vec![state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]), drain],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power = 300;
        ch.enter_state(&lc.states, 7, EvalEnv::self_only());
        assert_eq!(ch.power, 0, "300 + (-1000) clamps to floor 0");
    }

    /// AC1: `poweradd` is evaluated as an EXPRESSION on entry, not a literal —
    /// `poweradd = 30 + 20` adds 50. Confirms `eval_value` runs the compiled expr.
    #[test]
    fn poweradd_on_entry_evaluates_expression() {
        let st = state(
            3,
            Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), poweradd: Some("30 + 20"), ..Entry::default() },
            vec![],
        );
        let lc = loaded(
            vec![state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]), st],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power = 0;
        ch.enter_state(&lc.states, 3, EvalEnv::self_only());
        assert_eq!(ch.power, 50, "poweradd expression `30 + 20` adds 50");
    }

    /// AC3: a malformed `poweradd` (const-0 fallback) on entry adds nothing and
    /// never panics — the fallback evaluates to 0.
    #[test]
    fn poweradd_on_entry_malformed_is_noop() {
        // `Entry.poweradd` is compiled via CompiledExpr::compile; `1 +` is the
        // const-0 fallback, so entry adds 0.
        let st = state(
            4,
            Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), poweradd: Some("1 +"), ..Entry::default() },
            vec![],
        );
        let lc = loaded(
            vec![state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]), st],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power = 123;
        ch.enter_state(&lc.states, 4, EvalEnv::self_only());
        assert_eq!(ch.power, 123, "malformed poweradd (const-0) adds nothing");
    }

    /// AC3: a non-positive `power_max` (malformed character data) collapses the
    /// valid range to `{0}`. Any poweradd / PowerAdd / PowerSet leaves power at 0
    /// rather than panicking. Exercises `set_power_clamped`'s `max(0)` guard.
    #[test]
    fn power_max_non_positive_keeps_power_at_zero() {
        // poweradd-on-entry with power_max = 0.
        let st = state(
            2,
            Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), poweradd: Some("500"), ..Entry::default() },
            vec![],
        );
        let lc = loaded(
            vec![state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]), st],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power_max = 0;
        ch.power = 0;
        ch.enter_state(&lc.states, 2, EvalEnv::self_only());
        assert_eq!(ch.power, 0, "power_max=0 -> power pinned to 0 on entry");

        // PowerSet with a negative power_max also pins to 0 (never panics).
        let set = ctrl(0, "PowerSet", &[], &[(1, &["1"])], Some("0"), &[("value", "900")]);
        let lc2 = loaded(
            vec![state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![set])],
            tiny_air(0, &[5]),
        );
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.power_max = -5;
        ch2.power = 0;
        lc2.tick(&mut ch2);
        assert_eq!(ch2.power, 0, "negative power_max -> power pinned to 0");
    }

    /// AC3: a garbage (saturating) huge `PowerAdd` value can never overflow before
    /// the clamp — `add_power_clamped` uses saturating arithmetic. Near-i32::MAX
    /// add starting from a positive power clamps at power_max, no panic.
    #[test]
    fn power_add_controller_saturates_huge_value() {
        let add = ctrl(0, "PowerAdd", &[], &[(1, &["1"])], Some("0"), &[("value", "2147483647")]);
        let st = stand_n(0, vec![add]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power_max = 1000;
        ch.power = 2000; // already above max (e.g. stale data) — clamp brings it down
        lc.tick(&mut ch);
        assert_eq!(ch.power, 1000, "huge add saturates then clamps to power_max");
    }

    /// AC2: `PowerAdd`/`PowerSet` with a malformed `value` (const-0 fallback) is a
    /// safe operation — PowerAdd adds 0 (no-op), PowerSet sets 0 (the fallback
    /// value), neither panics.
    #[test]
    fn power_controllers_malformed_value_are_safe() {
        // PowerAdd with garbage value -> fallback evals to 0 -> adds nothing.
        let add = ctrl(0, "PowerAdd", &[], &[(1, &["1"])], Some("0"), &[("value", "1 +")]);
        let lc_add = loaded(vec![stand_n(0, vec![add])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power = 444;
        lc_add.tick(&mut ch);
        assert_eq!(ch.power, 444, "PowerAdd garbage value adds 0");

        // PowerSet with garbage value -> fallback evals to 0 -> sets power to 0.
        let set = ctrl(0, "PowerSet", &[], &[(1, &["1"])], Some("0"), &[("value", "*/")]);
        let lc_set = loaded(vec![stand_n(0, vec![set])], tiny_air(0, &[5]));
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.power = 444;
        lc_set.tick(&mut ch2);
        assert_eq!(ch2.power, 0, "PowerSet garbage value sets the const-0 fallback");
    }

    /// AC2: the controller dispatch matches `PowerAdd`/`PowerSet` case-INsensitively
    /// (MUGEN type names are not case-sensitive). `poweradd`/`POWERSET` both fire.
    #[test]
    fn power_controllers_dispatch_case_insensitively() {
        let add = ctrl(0, "poweradd", &[], &[(1, &["1"])], Some("0"), &[("value", "40")]);
        let lc_add = loaded(vec![stand_n(0, vec![add])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power = 0;
        lc_add.tick(&mut ch);
        assert_eq!(ch.power, 40, "lowercase `poweradd` controller fires");

        let set = ctrl(0, "POWERSET", &[], &[(1, &["1"])], Some("0"), &[("value", "77")]);
        let lc_set = loaded(vec![stand_n(0, vec![set])], tiny_air(0, &[5]));
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.power = 0;
        lc_set.tick(&mut ch2);
        assert_eq!(ch2.power, 77, "uppercase `POWERSET` controller fires");
    }

    /// AC2: a `PowerAdd` whose `value` is an EXPRESSION (not a literal) is
    /// evaluated against the live character — `value = power + 100` reads the
    /// current power. Confirms the controller routes through `eval_param`.
    #[test]
    fn power_add_controller_value_is_an_expression() {
        let add = ctrl(0, "PowerAdd", &[], &[(1, &["1"])], Some("0"), &[("value", "10 * 5")]);
        let lc = loaded(vec![stand_n(0, vec![add])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.power = 100;
        lc.tick(&mut ch);
        assert_eq!(ch.power, 150, "PowerAdd `10 * 5` adds 50 to 100");
    }

    /// AC4 (reinforced): repeated entry into the KFM super-gated attack state
    /// accumulates power across entries, demonstrating the meter can climb toward
    /// the `power >= 1000` super threshold. Gated: skips when test-assets/ absent.
    #[test]
    fn real_kfm_repeated_attack_entries_climb_toward_super() {
        let def = test_asset("kfm/kfm.def");
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
        let Some(attack) = lc.states.get(&200) else {
            eprintln!("skipping: kfm.def has no [Statedef 200]");
            return;
        };
        if attack.poweradd.is_none() {
            eprintln!("skipping: [Statedef 200] carries no poweradd");
            return;
        }
        let mut ch = Character::with_constants(lc.constants);
        ch.power = 0;
        // Re-enter the attack state many times; each entry adds the authored
        // poweradd. Power must rise monotonically and never leave [0, power_max].
        let mut last = ch.power;
        for _ in 0..200 {
            ch.enter_state(&lc.states, 200, EvalEnv::self_only());
            assert!(ch.power >= last, "power never decreases across attack entries");
            assert!(
                (0..=ch.power_max).contains(&ch.power),
                "power stays within [0, power_max] (got {}, max {})",
                ch.power,
                ch.power_max
            );
            last = ch.power;
        }
        // With 200 entries of a positive poweradd, the meter must have crossed the
        // 1000 super threshold (KFM's authored poweradd=10 => 2000 before clamp),
        // proving gated supers become reachable.
        assert!(
            ch.power >= 1000,
            "repeated KFM attack entries should fill the meter past the 1000 super gate (got {})",
            ch.power
        );
    }

    /// AC4: gated real-KFM test — entering an attack state (e.g. [Statedef 200],
    /// which authors `poweradd = 10`) raises power, demonstrating the meter
    /// fills toward the 1000 super threshold. Skips when test-assets/ is absent.
    #[test]
    fn real_kfm_attack_state_fills_power() {
        let def = test_asset("kfm/kfm.def");
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
        // KFM's light-punch attack state. If the fixture lacks it (or it has no
        // poweradd), skip rather than fail — the gate is the meter mechanism,
        // and other attack states would still fill it.
        let Some(attack) = lc.states.get(&200) else {
            eprintln!("skipping: kfm.def has no [Statedef 200]");
            return;
        };
        if attack.poweradd.is_none() {
            eprintln!("skipping: [Statedef 200] carries no poweradd");
            return;
        }
        let mut ch = Character::with_constants(lc.constants);
        ch.power = 0;
        // Directly enter the attack state through the real executor path.
        ch.enter_state(&lc.states, 200, EvalEnv::self_only());
        assert!(
            ch.power > 0,
            "entering KFM attack state 200 should fill the power meter (got {})",
            ch.power
        );
        assert!(ch.power <= ch.power_max, "power stays within [0, power_max]");
    }

    // =====================================================================
    // Proctor (task 5.3): edge-case, error-path, and MUGEN-semantics coverage
    // for the per-tick executor, layered on top of Forge's tests. Each block is
    // annotated with the acceptance criterion it exercises. All synthetic except
    // the gated real-KFM tick above.
    // =====================================================================

    /// Builds an [`AirFile`] with one action that has the given per-frame
    /// durations AND an explicit `loopstart` element index, so the loop-back
    /// target can be something other than 0 (the `tiny_air` helper hardcodes 0).
    fn air_with_loopstart(action: i32, frames_ticks: &[i32], loopstart: usize) -> AirFile {
        let mut air = tiny_air(action, frames_ticks);
        if let Some(a) = air.actions.get_mut(&action) {
            a.loopstart = loopstart;
        }
        air
    }

    /// Inserts a second action into an existing AIR file (so a ChangeState target
    /// has a valid animation to advance).
    fn add_action(air: &mut AirFile, action: i32, frames_ticks: &[i32]) {
        let frames = tiny_air(action, frames_ticks)
            .actions
            .remove(&action)
            .expect("tiny_air builds the requested action")
            .frames;
        air.actions.insert(
            action,
            AnimAction {
                action_number: action,
                frames,
                loopstart: 0,
            },
        );
    }

    // ---- AC1: full special-state order (-3, -2, -1 all run before current) ----

    #[test]
    fn all_three_special_states_run_then_current() {
        // Each of -3/-2/-1 and the current state has a VelAdd; the y-accumulation
        // proves all four ran in one tick, in order. Use distinct increments so a
        // dropped state would change the total detectably.
        let s_neg3 = ctrl(-3, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "100")]);
        let s_neg2 = ctrl(-2, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "10")]);
        let s_neg1 = ctrl(-1, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "1")]);
        let s_cur = ctrl(0, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "1000")]);
        let lc = loaded(
            vec![
                stand_n(-3, vec![s_neg3]),
                stand_n(-2, vec![s_neg2]),
                stand_n(-1, vec![s_neg1]),
                stand_n(0, vec![s_cur]),
            ],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(0.0, 0.0);
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 4, "all four states fire one ctrl");
        assert!((ch.vel.x - 1111.0).abs() < 1e-6, "got {}", ch.vel.x);
    }

    #[test]
    fn special_state_change_state_redirects_current() {
        // A ChangeState fired from [Statedef -1] (the .cmd command bridge) must
        // change which numbered state is treated as "current" this tick: -1 sends
        // us from 0 to 50, and state 50's controller runs in the SAME tick.
        let cmd = ctrl(-1, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "50")]);
        let in50 = ctrl(50, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "5")]);
        // State 0's controller would add 999 if it ran — it must NOT, since -1 sent
        // us to 50 before the current-state pass.
        let in0 = ctrl(0, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "999")]);
        let lc = loaded(
            vec![
                stand_n(-1, vec![cmd]),
                state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![in0]),
                state(50, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![in50]),
            ],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::<f32>::ZERO;
        let report = lc.tick(&mut ch);
        assert_eq!(ch.state_no, 50, "[-1] ChangeState redirected the current state");
        assert!(report.transitions >= 1);
        // Only state 50's VelAdd ran on the current pass (not state 0's).
        assert!((ch.vel.x - 5.0).abs() < 1e-6, "state 0 must not run after redirect; got {}", ch.vel.x);
    }

    // ---- AC1: triggerall AND semantics (multi-condition) ----

    #[test]
    fn triggerall_all_conditions_must_be_true() {
        // Two triggerall conditions: the controller fires only when BOTH hold.
        let mk = |life_ok: &'static str| {
            ctrl(0, "VelAdd", &["ctrl", life_ok], &[(1, &["1"])], None, &[("x", "1")])
        };
        // ctrl=true and Life>50 → fires.
        let lc_pass = loaded(vec![stand_n(0, vec![mk("Life > 50")])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.ctrl = true;
        ch.life = 100;
        ch.vel = Vec2::<f32>::ZERO;
        lc_pass.tick(&mut ch);
        assert!((ch.vel.x - 1.0).abs() < 1e-6, "both triggerall true → fires");

        // Second triggerall false (Life > 5000) → does not fire despite ctrl true.
        let lc_fail = loaded(vec![stand_n(0, vec![mk("Life > 5000")])], tiny_air(0, &[5]));
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.physics = Physics::None;
        ch2.ctrl = true;
        ch2.life = 100;
        ch2.vel = Vec2::<f32>::ZERO;
        lc_fail.tick(&mut ch2);
        assert!((ch2.vel.x - 0.0).abs() < 1e-6, "one false triggerall → skipped");
    }

    // ---- AC1: within-group AND across multiple conditions ----

    #[test]
    fn group_requires_all_conditions_and() {
        // trigger1 has two AND'd conditions; the group is true only when both are.
        let c_true = ctrl(0, "VelAdd", &[], &[(1, &["Time >= 0", "StateNo = 0"])], None, &[("x", "1")]);
        let lc = loaded(vec![stand_n(0, vec![c_true])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::<f32>::ZERO;
        lc.tick(&mut ch);
        assert!((ch.vel.x - 1.0).abs() < 1e-6, "both group conds true → fires");

        // One condition false (StateNo = 7) → the whole AND-group is false.
        let c_false = ctrl(0, "VelAdd", &[], &[(1, &["Time >= 0", "StateNo = 7"])], None, &[("x", "1")]);
        let lc2 = loaded(vec![stand_n(0, vec![c_false])], tiny_air(0, &[5]));
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.physics = Physics::None;
        ch2.vel = Vec2::<f32>::ZERO;
        lc2.tick(&mut ch2);
        assert!((ch2.vel.x - 0.0).abs() < 1e-6, "one false group cond → skipped");
    }

    // ---- AC1: OR across multiple contiguous groups ----

    #[test]
    fn or_across_contiguous_groups_first_true_wins() {
        // trigger1 false, trigger2 true → fires (OR). No gap, so both are live.
        let c = ctrl(
            0,
            "ChangeState",
            &[],
            &[(1, &["0"]), (2, &["1"])],
            None,
            &[("value", "9")],
        );
        let lc = loaded(vec![stand_n(0, vec![c]), stand_n(9, vec![])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        assert_eq!(lc.tick(&mut ch).transitions, 1);
        assert_eq!(ch.state_no, 9);
    }

    #[test]
    fn trigger1_only_fires_when_true() {
        // The minimal valid controller: a single trigger1. Fires iff it is true.
        let yes = ctrl(0, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "1")]);
        let lc = loaded(vec![stand_n(0, vec![yes])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::<f32>::ZERO;
        lc.tick(&mut ch);
        assert!((ch.vel.x - 1.0).abs() < 1e-6);

        let no = ctrl(0, "VelAdd", &[], &[(1, &["0"])], None, &[("x", "1")]);
        let lc2 = loaded(vec![stand_n(0, vec![no])], tiny_air(0, &[5]));
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.physics = Physics::None;
        ch2.vel = Vec2::<f32>::ZERO;
        lc2.tick(&mut ch2);
        assert!((ch2.vel.x - 0.0).abs() < 1e-6);
    }

    #[test]
    fn empty_group_conditions_never_satisfy() {
        // A trigger1 with no conditions (`group_is_true` rejects empty groups) must
        // never fire — there is nothing to satisfy. Built directly so we can model
        // the degenerate empty-conditions case.
        let c = CompiledController {
            state_number: 0,
            label: String::new(),
            controller_type: Some("VelAdd".to_string()),
            triggerall: vec![],
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![], // empty AND-group
            }],
            persistent: None,
            ignorehitpause: None,
            params: [("x".to_string(), CompiledParam::compile("1"))]
                .into_iter()
                .collect(),
        };
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::<f32>::ZERO;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 0, "empty group cannot satisfy");
        assert!((ch.vel.x - 0.0).abs() < 1e-6);
    }

    #[test]
    fn fallback_compiled_trigger_never_fires() {
        // A trigger whose source failed to compile becomes the const-0 fallback,
        // which is always false → the controller can never fire. Use a malformed
        // expression ("1 +") that compiles to the fallback.
        let bad = CompiledExpr::compile("1 +");
        assert!(bad.is_fallback, "precondition: malformed expr is a fallback");
        let c = CompiledController {
            state_number: 0,
            label: String::new(),
            controller_type: Some("VelAdd".to_string()),
            triggerall: vec![],
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![bad],
            }],
            persistent: None,
            ignorehitpause: None,
            params: [("x".to_string(), CompiledParam::compile("1"))]
                .into_iter()
                .collect(),
        };
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::<f32>::ZERO;
        assert_eq!(lc.tick(&mut ch).controllers_fired, 0);
        assert!((ch.vel.x - 0.0).abs() < 1e-6);
    }

    // ---- AC4: ChangeState edge cases (missing value, ctrl override, unknown) ----

    #[test]
    fn change_state_without_value_is_safe_noop() {
        // ChangeState lacking a `value` param must not transition or panic.
        let c = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[]); // no value
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 0, "no value → no transition");
        assert_eq!(ch.state_no, 0);
    }

    #[test]
    fn change_state_ctrl_param_overrides_ctrl_flag() {
        // ChangeState's optional `ctrl` param sets the control flag on transition.
        let c = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "1"), ("ctrl", "1")]);
        // Destination state 1 has NO ctrl entry param, so the ChangeState ctrl wins.
        let lc = loaded(
            vec![stand_n(0, vec![c]), stand_n(1, vec![])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.ctrl = false;
        lc.tick(&mut ch);
        assert_eq!(ch.state_no, 1);
        assert!(ch.ctrl, "ChangeState ctrl=1 enabled control");
    }

    #[test]
    fn change_state_to_unknown_updates_cursor_only() {
        // Transition to a state not in the graph: the cursor moves (so triggers
        // reading StateNo see the requested number) but no entry params apply, and
        // nothing panics.
        let c = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "12345")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.prev_state_no = -1;
        ch.physics = Physics::Stand; // a recognizable pre-existing physics
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 1);
        assert_eq!(ch.state_no, 12345);
        assert_eq!(ch.prev_state_no, 0);
        // No entry params for the unknown state → physics unchanged from before.
        assert_eq!(ch.physics, Physics::Stand);
    }

    #[test]
    fn self_change_state_resets_time() {
        // A ChangeState to the CURRENT state number still counts as a re-entry:
        // state_time resets to 0 (then +1 from advance_time), prev = self.
        // Gate it so it fires only once (persistent semantics not the point here):
        // use Time = 0 so after the reset+advance it no longer qualifies.
        let c = ctrl(0, "ChangeState", &[], &[(1, &["Time = 5"])], None, &[("value", "0")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.prev_state_no = 7;
        ch.state_time = 5; // satisfies Time = 5
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 1, "self-transition still transitions");
        assert_eq!(ch.state_no, 0);
        assert_eq!(ch.prev_state_no, 0, "self-transition sets prev to self");
        assert_eq!(ch.state_time, 1, "time reset to 0 then advanced one tick");
    }

    // ---- Audit P3+P11: SelfState + VelMul dispatch arms --------------------

    #[test]
    fn self_state_changes_state_via_enter_path() {
        // `type=SelfState value=N` must change state_no to N through the normal
        // enter_state path: prev_state_no = old, state_time reset, and the
        // destination statedef's entry header (here, anim) applied.
        let c = ctrl(0, "SelfState", &[], &[(1, &["1"])], None, &[("value", "5210")]);
        // Destination state 5210 carries an entry anim so we can prove the
        // statedef header ran (mirrors a get-hit recovery state's `anim`).
        let dest = state(
            5210,
            Entry { st: Some("S"), ph: Some("N"), anim: Some("5210"), ..Entry::default() },
            vec![],
        );
        let lc = loaded(vec![stand_n(0, vec![c]), dest], tiny_air(5210, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.prev_state_no = -1;
        ch.anim = 0;
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 1, "SelfState transitions via enter_state");
        assert_eq!(ch.state_no, 5210);
        assert_eq!(ch.prev_state_no, 0, "prev set to the departed state");
        assert_eq!(ch.state_time, 1, "time reset on entry then advanced one tick");
        assert_eq!(ch.anim, 5210, "destination statedef entry anim applied");
    }

    #[test]
    fn self_state_ctrl_param_overrides_ctrl_flag() {
        // SelfState honors the optional `ctrl` override exactly like ChangeState.
        let c = ctrl(
            0,
            "SelfState",
            &[],
            &[(1, &["1"])],
            None,
            &[("value", "1"), ("ctrl", "1")],
        );
        // Destination state 1 has NO ctrl entry param, so the controller's wins.
        let lc = loaded(
            vec![stand_n(0, vec![c]), stand_n(1, vec![])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.ctrl = false;
        lc.tick(&mut ch);
        assert_eq!(ch.state_no, 1);
        assert!(ch.ctrl, "SelfState ctrl=1 enabled control");
    }

    #[test]
    fn self_state_without_value_is_safe_noop() {
        // A SelfState lacking a `value` must not transition or panic.
        let c = ctrl(0, "SelfState", &[], &[(1, &["1"])], None, &[]); // no value
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 0, "no value → no transition");
        assert_eq!(ch.state_no, 0);
    }

    #[test]
    fn vel_mul_scales_x_and_leaves_absent_axis_unchanged() {
        // VelMul x=0.5 halves vel.x; with `y` absent, vel.y is multiplied by 1.0
        // (left unchanged), matching MUGEN.
        let c = ctrl(0, "VelMul", &[], &[(1, &["1"])], None, &[("x", "0.5")]);
        // Physics::None so apply_physics does not perturb the velocity we assert.
        let st = state(
            0,
            Entry { st: Some("S"), ph: Some("N"), ..Entry::default() },
            vec![c],
        );
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(4.0, 3.0);
        lc.tick(&mut ch);
        assert!((ch.vel.x - 2.0).abs() < 1e-6, "x halved, got {}", ch.vel.x);
        assert!((ch.vel.y - 3.0).abs() < 1e-6, "absent y axis unchanged (×1.0)");
    }

    #[test]
    fn vel_mul_zero_on_both_axes_zeroes_velocity() {
        // VelMul x=0 y=0 zeroes both components.
        let c = ctrl(0, "VelMul", &[], &[(1, &["1"])], None, &[("x", "0"), ("y", "0")]);
        let st = state(
            0,
            Entry { st: Some("S"), ph: Some("N"), ..Entry::default() },
            vec![c],
        );
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(7.0, -9.0);
        lc.tick(&mut ch);
        assert!((ch.vel.x).abs() < 1e-6, "x zeroed, got {}", ch.vel.x);
        assert!((ch.vel.y).abs() < 1e-6, "y zeroed, got {}", ch.vel.y);
    }

    // ---- Proctor: extra SelfState coverage (edge + MUGEN semantics) ---------

    #[test]
    fn self_state_value_is_an_expression_not_just_a_literal() {
        // `value` is an expr like ChangeState's. KFM authors literal state
        // numbers, but the path must evaluate an expression. `value = 5200 + 10`
        // must resolve to 5210 and enter it.
        let c = ctrl(
            0,
            "SelfState",
            &[],
            &[(1, &["1"])],
            None,
            &[("value", "5200 + 10")],
        );
        let lc = loaded(
            vec![stand_n(0, vec![c]), stand_n(5210, vec![])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 1, "SelfState transitioned");
        assert_eq!(ch.state_no, 5210, "value expression 5200+10 evaluated");
    }

    #[test]
    fn self_state_ctrl_override_beats_conflicting_destination_ctrl_header() {
        // Mirrors ChangeState's ordering: enter_state applies the destination
        // statedef's `ctrl` entry header FIRST, then the controller's `ctrl`
        // param overrides it. Destination authors `ctrl = 0`, controller says
        // `ctrl = 1`; the controller must win.
        let c = ctrl(
            0,
            "SelfState",
            &[],
            &[(1, &["1"])],
            None,
            &[("value", "9"), ("ctrl", "1")],
        );
        let dest = state(
            9,
            Entry { st: Some("S"), ph: Some("N"), ctrl: Some("0"), ..Entry::default() },
            vec![],
        );
        let lc = loaded(vec![stand_n(0, vec![c]), dest], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.ctrl = false;
        lc.tick(&mut ch);
        assert_eq!(ch.state_no, 9);
        assert!(ch.ctrl, "controller ctrl=1 override beats destination ctrl=0 header");
    }

    #[test]
    fn self_state_to_unknown_state_is_safe_noncrashing_transition() {
        // SelfState to a state that does not exist in the map must not panic:
        // enter_state moves the cursor (transitions counted) but applies no entry
        // header. This mirrors ChangeState's "unknown state; cursor updated only".
        let c = ctrl(0, "SelfState", &[], &[(1, &["1"])], None, &[("value", "424242")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 7;
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 1, "transition counted even for unknown target");
        assert_eq!(ch.state_no, 424_242, "cursor moved to the requested (missing) state");
        assert_eq!(ch.prev_state_no, 0, "prev recorded");
        assert_eq!(ch.anim, 7, "no entry header applied for a missing state → anim unchanged");
    }

    #[test]
    fn self_state_dispatch_is_case_insensitive() {
        // MUGEN controller-type names are case-insensitive: `selfstate` /
        // `SELFSTATE` must reach the same arm as `SelfState`.
        for spelling in ["selfstate", "SELFSTATE", "SelfState"] {
            let c = ctrl(0, spelling, &[], &[(1, &["1"])], None, &[("value", "3")]);
            let lc = loaded(
                vec![stand_n(0, vec![c]), stand_n(3, vec![])],
                tiny_air(0, &[5]),
            );
            let mut ch = Character::new();
            ch.state_no = 0;
            let report = lc.tick(&mut ch);
            assert_eq!(report.transitions, 1, "{spelling}: transitioned");
            assert_eq!(ch.state_no, 3, "{spelling}: reached state 3");
        }
    }

    #[test]
    fn self_state_resets_state_time_on_self_transition() {
        // SelfState onto the SAME state number is a re-entry: it still resets
        // state_time (MUGEN semantics, shared with ChangeState's self-transition).
        let c = ctrl(0, "SelfState", &[], &[(1, &["1"])], None, &[("value", "0")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.state_time = 50;
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 1, "self-transition counts as a re-entry");
        assert_eq!(ch.state_no, 0);
        // enter_state set state_time = 0, then advance_time ticked it to 1.
        assert_eq!(ch.state_time, 1, "state_time reset on re-entry then advanced");
    }

    // ---- Proctor: extra VelMul coverage (edge + MUGEN semantics) -----------

    #[test]
    fn vel_mul_y_only_scales_y_and_leaves_absent_x_unchanged() {
        // Symmetric to the x-only case: `y` present, `x` absent → x × 1.0.
        let c = ctrl(0, "VelMul", &[], &[(1, &["1"])], None, &[("y", "2.0")]);
        let st = state(
            0,
            Entry { st: Some("S"), ph: Some("N"), ..Entry::default() },
            vec![c],
        );
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(5.0, -1.5);
        lc.tick(&mut ch);
        assert!((ch.vel.x - 5.0).abs() < 1e-6, "absent x axis unchanged (×1.0)");
        assert!((ch.vel.y - (-3.0)).abs() < 1e-6, "y doubled, got {}", ch.vel.y);
    }

    #[test]
    fn vel_mul_with_no_params_is_a_total_noop() {
        // VelMul with neither x nor y: both axes × 1.0, velocity untouched, the
        // controller still "fires" (it qualified). Must never panic.
        let c = ctrl(0, "VelMul", &[], &[(1, &["1"])], None, &[]);
        let st = state(
            0,
            Entry { st: Some("S"), ph: Some("N"), ..Entry::default() },
            vec![c],
        );
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(3.25, -6.5);
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1, "VelMul fired even with no params");
        assert!((ch.vel.x - 3.25).abs() < 1e-6, "x unchanged");
        assert!((ch.vel.y - (-6.5)).abs() < 1e-6, "y unchanged");
    }

    #[test]
    fn vel_mul_negative_factor_reverses_direction() {
        // A negative multiplier flips the sign and scales magnitude.
        let c = ctrl(0, "VelMul", &[], &[(1, &["1"])], None, &[("x", "-2"), ("y", "-0.5")]);
        let st = state(
            0,
            Entry { st: Some("S"), ph: Some("N"), ..Entry::default() },
            vec![c],
        );
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(3.0, -8.0);
        lc.tick(&mut ch);
        assert!((ch.vel.x - (-6.0)).abs() < 1e-6, "x = 3 × -2, got {}", ch.vel.x);
        assert!((ch.vel.y - 4.0).abs() < 1e-6, "y = -8 × -0.5, got {}", ch.vel.y);
    }

    #[test]
    fn vel_mul_evaluates_an_expression_factor() {
        // The factor is an expr, like KFM's `x = .85 * ifelse(...)`. Use a pure
        // arithmetic expr so the result is deterministic: x ×= (0.5 * 0.5) = 0.25.
        let c = ctrl(0, "VelMul", &[], &[(1, &["1"])], None, &[("x", "0.5 * 0.5")]);
        let st = state(
            0,
            Entry { st: Some("S"), ph: Some("N"), ..Entry::default() },
            vec![c],
        );
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(8.0, 1.0);
        lc.tick(&mut ch);
        assert!((ch.vel.x - 2.0).abs() < 1e-6, "x = 8 × 0.25, got {}", ch.vel.x);
        assert!((ch.vel.y - 1.0).abs() < 1e-6, "absent y unchanged");
    }

    #[test]
    fn vel_mul_garbage_factor_is_a_safe_noop_for_that_axis() {
        // A garbage expression compiles to the const-0 fallback in MUGEN's
        // "bad expression -> 0" philosophy, so VelMul multiplies that axis by 0.
        // The key contract is *no panic*; we assert the engine's defined behavior
        // (fallback factor 0 ⇒ that axis zeroed) and that the other axis is
        // untouched and intact.
        let c = ctrl(
            0,
            "VelMul",
            &[],
            &[(1, &["1"])],
            None,
            &[("x", ")(@#$ not an expr"), ("y", "3")],
        );
        let st = state(
            0,
            Entry { st: Some("S"), ph: Some("N"), ..Entry::default() },
            vec![c],
        );
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(4.0, 2.0);
        // Must not panic.
        lc.tick(&mut ch);
        // Garbage x → const-0 fallback → 4 × 0 = 0 (defined, non-panicking).
        assert!((ch.vel.x).abs() < 1e-6, "garbage x factor → 0 fallback, got {}", ch.vel.x);
        // Valid y still applied: 2 × 3 = 6.
        assert!((ch.vel.y - 6.0).abs() < 1e-6, "valid y still scaled, got {}", ch.vel.y);
    }

    #[test]
    fn vel_mul_dispatch_is_case_insensitive() {
        // `velmul` / `VELMUL` must reach the same arm as `VelMul`.
        for spelling in ["velmul", "VELMUL", "VelMul"] {
            let c = ctrl(0, spelling, &[], &[(1, &["1"])], None, &[("x", "0.5")]);
            let st = state(
                0,
                Entry { st: Some("S"), ph: Some("N"), ..Entry::default() },
                vec![c],
            );
            let lc = loaded(vec![st], tiny_air(0, &[5]));
            let mut ch = Character::new();
            ch.state_no = 0;
            ch.physics = Physics::None;
            ch.vel = Vec2::new(10.0, 0.0);
            lc.tick(&mut ch);
            assert!((ch.vel.x - 5.0).abs() < 1e-6, "{spelling}: x halved, got {}", ch.vel.x);
        }
    }

    // ---- Proctor: SelfState + VelMul via the REAL CNS parser (lowercased) ---

    #[test]
    fn self_state_and_vel_mul_dispatch_from_real_cns_text() {
        // Drive both new controllers through the actual CNS parser (which
        // lowercases keys and types), compiling via CompiledState::from_parsed.
        // This proves the dispatch matches parser output, not just hand-built
        // controllers. State 200 runs a VelMul then a SelfState to 0.
        let cns = CnsFile::from_str(
            "[Statedef 200]\ntype = S\nphysics = N\n\
             [State 200, fric]\ntype = VelMul\ntrigger1 = Time = 0\nx = .5\n\
             [State 200, back]\ntype = SelfState\ntrigger1 = Time = 0\nvalue = 0\nctrl = 1\n\
             [Statedef 0]\ntype = S\nphysics = N\n",
        )
        .unwrap();
        let st200 = CompiledState::from_parsed(&cns.statedefs[0]);
        let st0 = CompiledState::from_parsed(&cns.statedefs[1]);
        let lc = loaded(vec![st200, st0], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 200;
        ch.physics = Physics::None;
        ch.ctrl = false;
        ch.vel = Vec2::new(6.0, 2.0);
        let report = lc.tick(&mut ch);
        // VelMul x=.5 applied before the SelfState moved us out of 200.
        assert!((ch.vel.x - 3.0).abs() < 1e-6, "VelMul halved x, got {}", ch.vel.x);
        assert!((ch.vel.y - 2.0).abs() < 1e-6, "absent y unchanged");
        // SelfState carried us back to state 0 with control enabled.
        assert_eq!(ch.state_no, 0, "SelfState returned to own state 0");
        assert!(ch.ctrl, "SelfState ctrl=1 enabled control");
        assert!(report.transitions >= 1, "the SelfState transition is counted");
    }

    // ---- Proctor: gated real-KFM fixtures for SelfState + VelMul -----------

    #[test]
    fn real_kfm_authors_self_state_and_vel_mul_controllers() {
        // The faithfulness driver: KFM really does author both controllers
        // (audit P3+P11). Gated to skip when test-assets is absent.
        let def = test_asset("kfm/kfm.def");
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
        let has = |kind: &str| {
            lc.states.values().any(|s| {
                s.controllers.iter().any(|c| {
                    c.controller_type
                        .as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case(kind))
                })
            })
        };
        assert!(has("SelfState"), "KFM authors at least one SelfState (e.g. state 821)");
        assert!(has("VelMul"), "KFM authors at least one VelMul (e.g. state 1020)");
    }

    #[test]
    fn real_kfm_vel_mul_state_1020_scales_velocity() {
        // End-to-end: enter KFM's real [Statedef 1020] (which authors
        // `type=VelMul x = .85 * ifelse(...)`, y absent, physics=N), seed a known
        // velocity, tick once, and confirm x was scaled by one of the two valid
        // friction factors while the absent y axis is untouched. Gated on assets.
        let def = test_asset("kfm/kfm.def");
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
        const VM_STATE: i32 = 1020;
        let Some(state) = lc.states.get(&VM_STATE).cloned() else {
            eprintln!("skipping: KFM has no [Statedef {VM_STATE}]; asset differs");
            return;
        };
        assert!(
            state.controllers.iter().any(|c| c
                .controller_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("VelMul"))),
            "KFM [Statedef {VM_STATE}] should author a VelMul controller"
        );
        let mut states = HashMap::new();
        states.insert(VM_STATE, state);
        let air = lc.air.clone();
        let mut ch = Character::with_constants(lc.constants);
        // Enter through the proper seam (runs the velset=0,0 entry header), then
        // seed a known velocity so VelMul has something to scale.
        ch.change_state(&states, VM_STATE);
        ch.physics = Physics::None; // pin physics so apply_physics cannot perturb x
        ch.vel = Vec2::new(10.0, 4.0);
        ch.tick_with(&states, &air, None, StageView::default());
        // x = 10 × (.85 × {1 or .8}) = either 8.5 or 6.8; y absent → unchanged.
        let x = ch.vel.x;
        assert!(
            (x - 8.5).abs() < 1e-4 || (x - 6.8).abs() < 1e-4,
            "VelMul scaled x by .85×{{1,.8}}; expected 8.5 or 6.8, got {x}"
        );
        assert!((ch.vel.y - 4.0).abs() < 1e-6, "absent y axis unchanged, got {}", ch.vel.y);
    }

    // ---- AC1: persistent re-arms on state re-entry (fire_counts cleared) ----

    #[test]
    fn persistent_zero_rearms_after_leaving_and_returning() {
        // persistent=0 fires once per entry. Leave state 0 to state 1 and come
        // back; the once-per-entry controller must fire again on re-entry.
        // State 0: a persistent=0 VelAdd, plus a ChangeState->1 gated on Time=1.
        // State 1: a ChangeState->0 gated on Time=1, sending us back.
        let add = ctrl(0, "VelAdd", &[], &[(1, &["1"])], Some("0"), &[("x", "1")]);
        let go1 = ctrl(0, "ChangeState", &[], &[(1, &["Time = 1"])], None, &[("value", "1")]);
        let go0 = ctrl(1, "ChangeState", &[], &[(1, &["Time = 1"])], None, &[("value", "0")]);
        let lc = loaded(
            vec![
                state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![add, go1]),
                state(1, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![go0]),
            ],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::<f32>::ZERO;
        // Tick 1: in state 0, Time=0. add fires (x=1). go1 needs Time=1 → no.
        lc.tick(&mut ch);
        assert!((ch.vel.x - 1.0).abs() < 1e-6, "first entry: add fires once");
        assert_eq!(ch.state_no, 0);
        // Tick 2: Time=1. add already fired this entry (persistent=0) → no refire.
        //         go1 fires → enter state 1 (clears fire_counts).
        lc.tick(&mut ch);
        assert_eq!(ch.state_no, 1, "moved to state 1");
        assert!((ch.vel.x - 1.0).abs() < 1e-6, "add did not refire same entry");
        // Tick 3: in state 1, Time=1 (entered last tick, advanced). go0 fires →
        //         back to state 0 (fresh entry, fire_counts cleared again).
        lc.tick(&mut ch);
        assert_eq!(ch.state_no, 0, "back to state 0");
        // Tick 4: new entry into 0, Time=1 → add fires AGAIN (re-armed).
        lc.tick(&mut ch);
        assert!((ch.vel.x - 2.0).abs() < 1e-6, "add re-armed on re-entry, got {}", ch.vel.x);
    }

    #[test]
    fn persistent_is_per_controller_index() {
        // Two controllers in the same state with persistent=0 must each fire once
        // (independent counts keyed by index), not share one count.
        let a = ctrl(0, "VelAdd", &[], &[(1, &["1"])], Some("0"), &[("x", "1")]);
        let b = ctrl(0, "VelAdd", &[], &[(1, &["1"])], Some("0"), &[("y", "1")]);
        let lc = loaded(vec![stand_n(0, vec![a, b])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::<f32>::ZERO;
        lc.tick(&mut ch);
        // Both fired once on the first tick: x=1 and y=1.
        assert!((ch.vel.x - 1.0).abs() < 1e-6);
        assert!((ch.vel.y - 1.0).abs() < 1e-6);
        // Second tick: neither refires (each is once-per-entry, distinct index).
        lc.tick(&mut ch);
        assert!((ch.vel.x - 1.0).abs() < 1e-6);
        assert!((ch.vel.y - 1.0).abs() < 1e-6);
    }

    // ---- AC3: physics ordering (controllers first, then friction/gravity) ----

    #[test]
    fn physics_applies_after_controllers_same_tick() {
        // VelSet x=10 in a stand-physics state, then friction *0.85 applies the
        // SAME tick → final x = 10 * 0.85, proving controllers run before physics.
        let vset = ctrl(0, "VelSet", &[], &[(1, &["1"])], None, &[("x", "10")]);
        let st = state(0, Entry { st: Some("S"), ph: Some("S"), anim: Some("0"), ..Entry::default() }, vec![vset]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::Stand;
        ch.vel = Vec2::<f32>::ZERO;
        lc.tick(&mut ch);
        let f = CharacterConstants::default().movement.stand_friction;
        assert!((ch.vel.x - 10.0 * f).abs() < 1e-6, "friction applied after VelSet; got {}", ch.vel.x);
    }

    #[test]
    fn custom_constants_friction_and_gravity_are_used() {
        // The executor reads friction/gravity from the character's OWN constants,
        // not hardcoded defaults. Seed non-default values and assert they apply.
        let consts = CharacterConstants {
            movement: MovementConstants {
                yaccel: 1.5,
                stand_friction: 0.5,
                crouch_friction: 0.25,
                ..MovementConstants::default()
            },
            ..CharacterConstants::default()
        };
        // Stand friction 0.5.
        let lc = loaded(vec![stand_n(0, vec![])], tiny_air(0, &[5]));
        let mut ch = Character::with_constants(consts);
        ch.state_no = 0;
        ch.physics = Physics::Stand;
        ch.vel = Vec2::new(10.0, 0.0);
        lc.tick(&mut ch);
        assert!((ch.vel.x - 5.0).abs() < 1e-6, "custom stand friction 0.5");

        // Crouch friction 0.25.
        let mut ch2 = Character::with_constants(consts);
        ch2.state_no = 0;
        ch2.physics = Physics::Crouch;
        ch2.vel = Vec2::new(8.0, 0.0);
        lc.tick(&mut ch2);
        assert!((ch2.vel.x - 2.0).abs() < 1e-6, "custom crouch friction 0.25");

        // Air gravity 1.5.
        let mut ch3 = Character::with_constants(consts);
        ch3.state_no = 0;
        ch3.physics = Physics::Air;
        ch3.vel = Vec2::new(0.0, -3.0);
        lc.tick(&mut ch3);
        assert!((ch3.vel.y - (-1.5)).abs() < 1e-6, "custom gravity 1.5 added");
    }

    #[test]
    fn unchanged_physics_does_nothing() {
        // Physics::Unchanged leaves velocity alone (the executor's None|Unchanged
        // arm). This is the inherited-physics case.
        let lc = loaded(vec![stand_n(0, vec![])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::Unchanged;
        ch.vel = Vec2::new(3.0, 4.0);
        lc.tick(&mut ch);
        assert!((ch.vel.x - 3.0).abs() < 1e-6);
        assert!((ch.vel.y - 4.0).abs() < 1e-6);
    }

    // ---- AC1/AC4: state entry token handling (unchanged / invalid tokens) ----

    #[test]
    fn entry_unchanged_tokens_keep_prior_category() {
        // A statedef with type=U / movetype=U / physics=U must NOT clobber the
        // character's existing category on entry (MUGEN "unchanged" semantics).
        let go = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "5")]);
        let dest = state(5, Entry { st: Some("U"), mv: Some("U"), ph: Some("U"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(
            vec![state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![go]), dest],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.state_type = StateType::Crouching;
        ch.move_type = MoveType::BeingHit;
        ch.physics = Physics::Crouch;
        lc.tick(&mut ch);
        assert_eq!(ch.state_no, 5);
        // Unchanged tokens preserved the prior categories.
        assert_eq!(ch.state_type, StateType::Crouching);
        assert_eq!(ch.move_type, MoveType::BeingHit);
        assert_eq!(ch.physics, Physics::Crouch);
    }

    #[test]
    fn entry_invalid_token_keeps_prior_category() {
        // An unrecognized type token (e.g. "Z") yields None from from_token, so the
        // category is left unchanged rather than reset or panicking.
        let go = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "5")]);
        let dest = state(5, Entry { st: Some("Z"), ph: Some("?"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(
            vec![state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![go]), dest],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.state_type = StateType::Air;
        ch.physics = Physics::Air;
        lc.tick(&mut ch);
        assert_eq!(ch.state_no, 5);
        assert_eq!(ch.state_type, StateType::Air, "invalid type token left unchanged");
        assert_eq!(ch.physics, Physics::Air, "invalid physics token left unchanged");
    }

    #[test]
    fn entry_anim_resets_element_and_time() {
        // Entering a state with an `anim` header resets the element cursor and the
        // element time to 0, even if they were mid-animation before.
        let go = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "5")]);
        let dest = state(5, Entry { st: Some("S"), ph: Some("N"), anim: Some("5"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![stand_n(0, vec![go]), dest], {
            let mut air = tiny_air(0, &[5]);
            add_action(&mut air, 5, &[10, 10]);
            air
        });
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.anim_elem = 1;
        ch.anim_elem_time = 99;
        lc.tick(&mut ch);
        assert_eq!(ch.anim, 5, "anim switched on entry");
        // Reset to element 0, then advanced one tick within the new element.
        assert_eq!(ch.anim_elem, 0);
        assert_eq!(ch.anim_elem_time, 1, "elem time reset to 0 then advanced");
    }

    #[test]
    fn entry_velset_pair_and_scalar() {
        // velset with both components, and velset with a single (x-only) value.
        let go = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "5")]);
        let pair = state(5, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), velset: Some("6, -4"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![stand_n(0, vec![go]), pair], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.vel = Vec2::new(1.0, 1.0);
        lc.tick(&mut ch);
        assert!((ch.vel.x - 6.0).abs() < 1e-6);
        assert!((ch.vel.y - (-4.0)).abs() < 1e-6);

        // Scalar velset (x only) → y component becomes 0.
        let go2 = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "6")]);
        let scalar = state(6, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), velset: Some("9"), ..Entry::default() }, vec![]);
        let lc2 = loaded(vec![stand_n(0, vec![go2]), scalar], tiny_air(0, &[5]));
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.vel = Vec2::new(2.0, 2.0);
        lc2.tick(&mut ch2);
        assert!((ch2.vel.x - 9.0).abs() < 1e-6);
        assert!((ch2.vel.y - 0.0).abs() < 1e-6, "scalar velset zeroes y");
    }

    // ---- AC4: VelSet / VelAdd partial-axis semantics ----

    #[test]
    fn vel_set_missing_axis_leaves_that_axis() {
        // VelSet with only x must leave y untouched; VelSet with only y leaves x.
        let only_x = ctrl(0, "VelSet", &[], &[(1, &["1"])], None, &[("x", "5")]);
        let lc = loaded(vec![stand_n(0, vec![only_x])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(1.0, 2.0);
        lc.tick(&mut ch);
        assert!((ch.vel.x - 5.0).abs() < 1e-6, "x set");
        assert!((ch.vel.y - 2.0).abs() < 1e-6, "y left unchanged");
    }

    #[test]
    fn vel_add_accumulates_both_axes() {
        let add = ctrl(0, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "2"), ("y", "-1")]);
        let lc = loaded(vec![stand_n(0, vec![add])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(10.0, 10.0);
        lc.tick(&mut ch);
        assert!((ch.vel.x - 12.0).abs() < 1e-6);
        assert!((ch.vel.y - 9.0).abs() < 1e-6);
    }

    // ---- AC3: animation advance corner cases ----

    #[test]
    fn animation_advances_to_nonzero_loopstart() {
        // Two frames, loopstart = 1: after the last frame, loop back to element 1,
        // never returning to element 0.
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], air_with_loopstart(0, &[1, 1], 1));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.anim_elem = 0;
        ch.anim_elem_time = 0;
        lc.tick(&mut ch); // elem 0 dur 1 reached → advance to elem 1
        assert_eq!(ch.anim_elem, 1);
        lc.tick(&mut ch); // elem 1 dur 1 reached → loop back to loopstart=1
        assert_eq!(ch.anim_elem, 1, "loops to loopstart 1, not 0");
        lc.tick(&mut ch);
        assert_eq!(ch.anim_elem, 1, "stays looping at loopstart");
    }

    #[test]
    fn animation_unknown_action_is_safe_noop() {
        // The current anim id has no action in the AIR file: advancing must be a
        // no-op (cursor untouched), not a panic.
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), ..Entry::default() }, vec![]);
        // State entry sets no anim, so ch.anim stays whatever we set (777, absent).
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 777; // not in the AIR file
        ch.anim_elem = 3;
        ch.anim_elem_time = 9;
        lc.tick(&mut ch);
        assert_eq!(ch.anim_elem, 3, "unknown anim leaves element cursor");
        assert_eq!(ch.anim_elem_time, 9, "unknown anim leaves element time");
    }

    #[test]
    fn anim_time_is_negative_until_finish() {
        // For a finite 2-frame action (durations 3 and 2 → total 5), AnimTime is
        // maintained as the negative ticks-until-end. After one tick in element 0,
        // 4 ticks remain → AnimTime = -4.
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[3, 2]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.anim_elem = 0;
        ch.anim_elem_time = 0;
        lc.tick(&mut ch); // elem 0, elem_time 1 → remaining (3-1)+2 = 4
        assert_eq!(ch.anim_time, -4, "AnimTime counts down negatively; got {}", ch.anim_time);
    }

    #[test]
    fn out_of_range_anim_element_is_clamped_not_panicking() {
        // An externally-corrupted anim_elem (beyond the action length) must be
        // clamped into range by advance_animation rather than panicking.
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[2, 2]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.anim_elem = 99; // out of range for a 2-frame action
        ch.anim_elem_time = 0;
        lc.tick(&mut ch); // must not panic
        assert!(ch.anim_elem >= 0 && ch.anim_elem < 2, "clamped into range; got {}", ch.anim_elem);
    }

    // ---- AC4: TickReport counters are accurate ----

    #[test]
    fn tick_report_counts_fires_and_transitions() {
        // Two firing controllers and one transition in a single tick.
        let add = ctrl(0, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "1")]);
        let set = ctrl(0, "CtrlSet", &[], &[(1, &["1"])], None, &[("value", "1")]);
        let go = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "1")]);
        let lc = loaded(
            vec![stand_n(0, vec![add, set, go]), stand_n(1, vec![])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        // add + set + go all fired (3); one of them was a transition.
        assert_eq!(report.controllers_fired, 3);
        assert_eq!(report.transitions, 1);
        assert!(!report.transition_cap_hit);
        assert_eq!(ch.state_no, 1);
    }

    #[test]
    fn controllers_after_transition_in_same_state_are_skipped() {
        // Once a ChangeState fires, the remaining controllers of the OLD state are
        // not run this tick (MUGEN stops processing the old state's list).
        let go = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "1")]);
        let after = ctrl(0, "VelAdd", &[], &[(1, &["1"])], None, &[("x", "999")]);
        let lc = loaded(
            vec![stand_n(0, vec![go, after]), stand_n(1, vec![])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::<f32>::ZERO;
        lc.tick(&mut ch);
        assert_eq!(ch.state_no, 1);
        assert!((ch.vel.x - 0.0).abs() < 1e-6, "post-ChangeState controller in old state must not run");
    }

    // ---- AC4: no-state special slots are skipped without error ----

    #[test]
    fn missing_special_states_are_skipped() {
        // A graph with ONLY the current state (no -3/-2/-1) ticks cleanly: the
        // missing special slots are a no-op, not an error or panic.
        let lc = loaded(vec![stand_n(0, vec![])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 0);
        assert_eq!(report.transitions, 0);
        assert_eq!(ch.state_no, 0);
        assert_eq!(ch.state_time, 1);
    }

    // ---- AC4: a controller with no type line is a safe no-op ----

    #[test]
    fn controller_without_type_is_safe_noop() {
        // A controller block lacking a `type` (controller_type = None) must
        // dispatch to the no-op path, never panicking.
        let c = CompiledController {
            state_number: 0,
            label: "mystery".to_string(),
            controller_type: None,
            triggerall: vec![],
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params: HashMap::new(),
        };
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(5.0, 5.0);
        let report = lc.tick(&mut ch);
        // It qualified and "fired" (dispatch ran) but did nothing.
        assert_eq!(report.controllers_fired, 1);
        assert!((ch.vel.x - 5.0).abs() < 1e-6);
        assert!((ch.vel.y - 5.0).abs() < 1e-6);
    }

    // ---- AC4: controller type matching is case-insensitive ----

    #[test]
    fn controller_type_match_is_case_insensitive() {
        // MUGEN controller names are case-insensitive: "velset"/"VELSET" dispatch.
        let lower = ctrl(0, "velset", &[], &[(1, &["1"])], None, &[("x", "3")]);
        let upper = ctrl(0, "CHANGESTATE", &[], &[(1, &["Time = 0"])], None, &[("value", "1")]);
        let lc = loaded(
            vec![stand_n(0, vec![lower, upper]), stand_n(1, vec![])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        // velset ran (x=3) then ChangeState ran (state 1). Both case-insensitive.
        assert_eq!(ch.state_no, 1);
    }

    // ---- ignorehitpause is wired through the loader -----------------------

    #[test]
    fn ignorehitpause_is_compiled_onto_the_controller() {
        // The loader must compile and carry the flag so the executor can honor it
        // during a hit-pause freeze (task 6.5). Verify the compiled controller
        // preserves it from CNS.
        let cns = CnsFile::from_str(
            "[Statedef 0]\ntype = S\n\
             [State 0, x]\ntype = Null\ntrigger1 = 1\nignorehitpause = 1\n",
        )
        .unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        let ihp = state.controllers[0]
            .ignorehitpause
            .as_ref()
            .expect("ignorehitpause should be compiled");
        assert!(!ihp.is_fallback);
        assert_eq!(ihp.source, "1");
    }

    // ---- Task 6.5: hit-pause (impact freeze) in the executor ---------------

    /// Builds a compiled controller exactly like [`ctrl`] but with an
    /// `ignorehitpause` expression set, so the test can prove the gate runs it
    /// during a freeze.
    fn ctrl_ihp(
        state_number: i32,
        kind: &str,
        groups: &[(u32, &[&str])],
        ignorehitpause: &str,
        params: &[(&str, &str)],
    ) -> CompiledController {
        CompiledController {
            ignorehitpause: Some(CompiledExpr::compile(ignorehitpause)),
            ..ctrl(state_number, kind, &[], groups, None, params)
        }
    }

    /// AC2/AC4: a character with `hitpause = N` freezes anim, state `Time`, and
    /// position for N ticks, then resumes normal advancement on the tick the
    /// counter reaches 0. The default `hitpause = 0` path is exercised by every
    /// other test (AC3); here we set it explicitly.
    #[test]
    fn hitpause_freezes_anim_time_and_pos_for_n_ticks_then_resumes() {
        // State 0: VelSet x = 1 every tick (so a *non-frozen* tick visibly moves
        // the character via integration). Anim 0 has two 1-tick frames so a
        // running tick advances the element each tick.
        let mover = ctrl(0, "VelSet", &[], &[(1, &["1"])], None, &[("x", "1")]);
        let lc = loaded(
            vec![stand_n(0, vec![mover])],
            tiny_air(0, &[1, 1]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.facing = Facing::Right;
        ch.set_hitpause_time(2); // accessor names the same field

        // Snapshot the frozen baseline.
        let anim_elem0 = ch.anim_elem;
        let pos0 = ch.pos;
        let time0 = ch.state_time;

        // Tick 1: frozen. Nothing advances; hitpause counts 2 -> 1.
        let r1 = lc.tick(&mut ch);
        assert!(r1.hitpaused, "first paused tick is reported as hit-paused");
        assert_eq!(ch.hitpause_time(), 1, "hitpause decremented by one");
        assert_eq!(ch.anim_elem, anim_elem0, "anim frozen");
        assert_eq!(ch.state_time, time0, "state Time frozen");
        assert_eq!(ch.pos, pos0, "position frozen (no physics integration)");
        assert_eq!(ch.vel.x, 0.0, "VelSet did not fire (controller is gated off)");
        assert_eq!(r1.controllers_fired, 0, "no controllers fire while frozen");

        // Tick 2: still frozen. hitpause counts 1 -> 0.
        let r2 = lc.tick(&mut ch);
        assert!(r2.hitpaused, "second paused tick still frozen");
        assert_eq!(ch.hitpause_time(), 0, "hitpause reaches zero");
        assert_eq!(ch.anim_elem, anim_elem0, "anim still frozen");
        assert_eq!(ch.pos, pos0, "position still frozen");

        // Tick 3: hitpause is 0 -> normal advancement resumes. The mover fires,
        // physics integrates position, and the state Time advances.
        let r3 = lc.tick(&mut ch);
        assert!(!r3.hitpaused, "freeze is over; tick runs normally");
        assert_eq!(ch.vel.x, 1.0, "VelSet fired on the resumed tick");
        assert!(ch.pos.x > pos0.x, "position integrated once the freeze ended");
        assert!(ch.state_time > time0, "state Time advances after the freeze");
    }

    /// AC2/AC4: during a freeze, an `ignorehitpause`-flagged controller still
    /// fires while a normal controller in the same state does not.
    #[test]
    fn ignorehitpause_controller_runs_during_pause_normal_one_does_not() {
        // State 0 has two VarSet controllers, both unconditionally triggered:
        //  - idx 0: writes var(0) = 1, NO ignorehitpause -> skipped while frozen.
        //  - idx 1: writes var(1) = 1, ignorehitpause = 1 -> runs while frozen.
        let normal = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("v", "0"), ("value", "1")]);
        let flagged =
            ctrl_ihp(0, "VarSet", &[(1, &["1"])], "1", &[("v", "1"), ("value", "1")]);
        let lc = loaded(vec![stand_n(0, vec![normal, flagged])], tiny_air(0, &[1]));

        let mut ch = Character::new();
        ch.state_no = 0;
        ch.set_hitpause_time(1);

        let report = lc.tick(&mut ch);
        assert!(report.hitpaused, "the tick is hit-paused");
        assert_eq!(ch.vars[0], 0, "normal controller is SKIPPED during the pause");
        assert_eq!(ch.vars[1], 1, "ignorehitpause controller STILL fires during pause");
        assert_eq!(report.controllers_fired, 1, "exactly the flagged one fired");
        assert_eq!(ch.hitpause_time(), 0, "pause counted down to zero");

        // After the pause ends, the normal controller fires too.
        let report2 = lc.tick(&mut ch);
        assert!(!report2.hitpaused);
        assert_eq!(ch.vars[0], 1, "normal controller fires once the freeze ends");
    }

    /// AC2: an `ignorehitpause` whose expression evaluates to `0` is treated as
    /// absent — the controller is skipped during a freeze, like any normal one.
    #[test]
    fn ignorehitpause_evaluating_to_zero_is_skipped_during_pause() {
        let flagged_off =
            ctrl_ihp(0, "VarSet", &[(1, &["1"])], "0", &[("v", "2"), ("value", "9")]);
        let lc = loaded(vec![stand_n(0, vec![flagged_off])], tiny_air(0, &[1]));

        let mut ch = Character::new();
        ch.state_no = 0;
        ch.set_hitpause_time(1);

        let report = lc.tick(&mut ch);
        assert!(report.hitpaused);
        assert_eq!(ch.vars[2], 0, "ignorehitpause=0 controller is skipped while frozen");
        assert_eq!(report.controllers_fired, 0);
    }

    /// AC1 (executor side): the `set_hitpause_time` accessor reads/writes the same
    /// value the freeze gates on, and clamps a negative input to zero.
    #[test]
    fn hitpause_time_accessor_round_trips_and_clamps() {
        let mut ch = Character::new();
        assert_eq!(ch.hitpause_time(), 0, "default is not paused");
        ch.set_hitpause_time(5);
        assert_eq!(ch.hitpause_time(), 5);
        assert_eq!(ch.hitpause, 5, "accessor and field are the same value");
        ch.set_hitpause_time(-3);
        assert_eq!(ch.hitpause_time(), 0, "negative input clamps to zero");
    }

    // ---- Proctor (task 6.5): extra hit-pause gate edge cases ----------------

    /// AC3: a character with `hitpause == 0` (the default, and an explicit zero)
    /// takes the NORMAL path — the gate is a transparent no-op. The mover fires,
    /// physics integrates, anim and state Time advance, and the tick is not
    /// reported as hit-paused.
    #[test]
    fn zero_hitpause_is_a_transparent_no_op() {
        let mover = ctrl(0, "VelSet", &[], &[(1, &["1"])], None, &[("x", "1")]);
        let lc = loaded(vec![stand_n(0, vec![mover])], tiny_air(0, &[1, 1]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.facing = Facing::Right;
        ch.set_hitpause_time(0); // explicit zero — same as the default path

        let pos0 = ch.pos;
        let r = lc.tick(&mut ch);
        assert!(!r.hitpaused, "hitpause == 0 is never reported as a freeze");
        assert_eq!(r.controllers_fired, 1, "the normal controller fires");
        assert_eq!(ch.vel.x, 1.0, "VelSet ran on the unpaused tick");
        assert!(ch.pos.x > pos0.x, "physics integrated normally");
        assert!(ch.state_time > 0, "state Time advanced normally");
        assert_eq!(ch.hitpause_time(), 0, "still zero — nothing decremented below 0");
    }

    /// AC2: `hitpause == 1` is a single-tick freeze: one frozen tick (counter
    /// 1 -> 0), then the very next tick resumes normally. Pins the smallest
    /// non-trivial freeze and its boundary.
    #[test]
    fn hitpause_of_one_freezes_exactly_one_tick() {
        let mover = ctrl(0, "VelSet", &[], &[(1, &["1"])], None, &[("x", "1")]);
        let lc = loaded(vec![stand_n(0, vec![mover])], tiny_air(0, &[1, 1]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.facing = Facing::Right;
        ch.set_hitpause_time(1);

        let r1 = lc.tick(&mut ch);
        assert!(r1.hitpaused, "the single paused tick is frozen");
        assert_eq!(ch.hitpause_time(), 0, "one tick takes the counter to zero");
        assert_eq!(ch.vel.x, 0.0, "mover did not fire while frozen");

        let r2 = lc.tick(&mut ch);
        assert!(!r2.hitpaused, "resumes on the very next tick");
        assert_eq!(ch.vel.x, 1.0, "mover fires once the freeze ends");
    }

    /// AC2: an `ignorehitpause`-flagged controller in a SPECIAL state (`[Statedef
    /// -2]`) also runs during the freeze — the gate scans `-3,-2,-1` then the
    /// current state, not just the current state. Forge's tests only flag a
    /// controller in the current state; this covers the special-state path.
    #[test]
    fn ignorehitpause_controller_in_special_state_runs_during_pause() {
        // [Statedef -2]: writes var(3) = 1, ignorehitpause = 1 -> runs while frozen.
        let special = ctrl_ihp(-2, "VarSet", &[(1, &["1"])], "1", &[("v", "3"), ("value", "1")]);
        // Current state has a NON-flagged controller that must stay skipped.
        let normal = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("v", "4"), ("value", "1")]);
        let lc = loaded(
            vec![stand_n(-2, vec![special]), stand_n(0, vec![normal])],
            tiny_air(0, &[1]),
        );

        let mut ch = Character::new();
        ch.state_no = 0;
        ch.set_hitpause_time(1);

        let report = lc.tick(&mut ch);
        assert!(report.hitpaused);
        assert_eq!(ch.vars[3], 1, "special-state ignorehitpause controller fires");
        assert_eq!(ch.vars[4], 0, "current-state normal controller stays skipped");
        assert_eq!(report.controllers_fired, 1, "only the flagged special one fired");
    }

    /// AC2/MUGEN-semantics: a `ChangeState` carried by an `ignorehitpause`
    /// controller during a freeze updates the cursor but does NOT re-process the
    /// destination state that same frozen tick (the documented hit-pause rule:
    /// the gate scans each state once and does not follow re-entry). The mover in
    /// the destination must therefore stay inert until the freeze ends.
    #[test]
    fn changestate_during_pause_does_not_reprocess_destination() {
        // Current state 0: ignorehitpause ChangeState -> state 7.
        let jump = ctrl_ihp(0, "ChangeState", &[(1, &["1"])], "1", &[("value", "7")]);
        // Destination state 7: a mover (no ignorehitpause) that would set vel.x if
        // it were re-processed this tick — it must NOT run during the freeze.
        let dest_mover = ctrl(7, "VelSet", &[], &[(1, &["1"])], None, &[("x", "9")]);
        let lc = loaded(
            vec![stand_n(0, vec![jump]), stand_n(7, vec![dest_mover])],
            tiny_air(0, &[1]),
        );

        let mut ch = Character::new();
        ch.state_no = 0;
        ch.facing = Facing::Right;
        ch.set_hitpause_time(1);

        let report = lc.tick(&mut ch);
        assert!(report.hitpaused);
        assert_eq!(ch.state_no, 7, "the ChangeState dispatched and moved the cursor");
        assert_eq!(ch.vel.x, 0.0, "destination state was NOT re-processed this frozen tick");
        assert_eq!(ch.hitpause_time(), 0, "freeze still counted down");
    }

    /// AC2: when `shaketime` outlasts `hitpause`, the freeze ends when `hitpause`
    /// reaches 0 (normal advancement resumes) while the remaining shake ticks keep
    /// counting down on the now-normal ticks. Pins the shake counter's independent
    /// lifetime — the simplification note says the defender's shake counts down
    /// alongside the pause and continues after it.
    #[test]
    fn shaketime_outlasting_hitpause_keeps_counting_after_freeze_ends() {
        let mover = ctrl(0, "VelSet", &[], &[(1, &["1"])], None, &[("x", "1")]);
        let lc = loaded(vec![stand_n(0, vec![mover])], tiny_air(0, &[1, 1]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.facing = Facing::Right;
        ch.hitpause = 1;
        ch.shaketime = 3;

        // Tick 1: frozen (hitpause 1 -> 0, shaketime 3 -> 2). Mover gated off.
        let r1 = lc.tick(&mut ch);
        assert!(r1.hitpaused, "frozen while hitpause > 0");
        assert_eq!(ch.hitpause, 0);
        assert_eq!(ch.shaketime, 2, "shake decremented during the freeze");
        assert_eq!(ch.vel.x, 0.0, "mover gated off while frozen");

        // Tick 2: hitpause is 0 -> NOT frozen, but shaketime still counts down on
        // the normal path (3 -> 2 -> 1). The mover now fires.
        let r2 = lc.tick(&mut ch);
        assert!(!r2.hitpaused, "freeze over once hitpause hit 0");
        assert_eq!(ch.shaketime, 1, "remaining shake counts down on a normal tick");
        assert_eq!(ch.vel.x, 1.0, "mover fires once the freeze ended");

        // Tick 3: shaketime 1 -> 0.
        lc.tick(&mut ch);
        assert_eq!(ch.shaketime, 0, "shake fully counted out");
    }

    /// AC2: a frozen tick performs NO state transitions even when the current
    /// state authors a normal `ChangeState` whose triggers are satisfied — the
    /// transition is gated off with every other non-`ignorehitpause` controller.
    #[test]
    fn normal_changestate_is_suppressed_during_freeze() {
        let jump = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "42")]);
        let lc = loaded(
            vec![stand_n(0, vec![jump]), stand_n(42, vec![])],
            tiny_air(0, &[1]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.set_hitpause_time(2);

        let r = lc.tick(&mut ch);
        assert!(r.hitpaused);
        assert_eq!(ch.state_no, 0, "no transition while frozen");
        assert_eq!(r.transitions, 0, "no transitions counted during a freeze");
        assert_eq!(r.controllers_fired, 0, "the normal ChangeState did not fire");
    }

    // ---- AC5: full multi-tick walk-cycle integration through the executor ----

    #[test]
    fn integration_walk_then_idle_cycle() {
        // A small but realistic loop exercised purely through Character::tick:
        //  - State 0 (stand, ctrl): on "holdfwd" → ChangeState to 20 (walk).
        //  - State 20 (walk): VelSet x = walk speed each tick; on Time>=2 with no
        //    command → ChangeState back to 0.
        // Drives several ticks and checks the cursor + velocity evolve correctly.
        let to_walk = ctrl(0, "ChangeState", &["ctrl"], &[(1, &["command = \"holdfwd\""])], None, &[("value", "20")]);
        let walk_vel = ctrl(20, "VelSet", &[], &[(1, &["1"])], None, &[("x", "2.4")]);
        let to_stand = ctrl(20, "ChangeState", &[], &[(1, &["Time >= 2"])], None, &[("value", "0")]);
        let lc = loaded(
            vec![
                state(0, Entry { st: Some("S"), mv: Some("I"), ph: Some("S"), anim: Some("0"), ctrl: Some("1"), ..Entry::default() }, vec![to_walk]),
                state(20, Entry { st: Some("S"), ph: Some("N"), anim: Some("20"), ..Entry::default() }, vec![walk_vel, to_stand]),
            ],
            {
                let mut air = tiny_air(0, &[4]);
                add_action(&mut air, 20, &[3, 3]);
                air
            },
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.ctrl = true;

        // Tick 1: standing, no command → stays in 0.
        assert_eq!(lc.tick(&mut ch).transitions, 0);
        assert_eq!(ch.state_no, 0);

        // Tick 2: holdfwd pressed → transition to walk (state 20), anim 20.
        ch.set_command_source(Box::new(ActiveCommands::from_names(["holdfwd"])));
        assert_eq!(lc.tick(&mut ch).transitions, 1);
        assert_eq!(ch.state_no, 20);
        assert_eq!(ch.anim, 20);
        // VelSet ran on entry tick; physics is None in walk so x is preserved.
        assert!((ch.vel.x - 2.4).abs() < 1e-6);

        // Release the command. If we kept holdfwd held, the to_stand transition
        // (Time >= 2) would land in state 0, whose to_walk controller would
        // immediately re-fire in the SAME tick (command still held, ctrl just set)
        // and bounce us straight back to walk — correct MUGEN in-tick re-entry, but
        // it would mask the return-to-stand we want to observe here.
        ch.set_command_source(Box::new(NoCommands));

        // Keep ticking in walk; once the in-state Time reaches >= 2 the to_stand
        // ChangeState fires and (with no command held) the cursor settles in 0.
        // Bounded so a regression that never transitions fails instead of hanging.
        let mut returned = false;
        for _ in 0..10 {
            lc.tick(&mut ch);
            if ch.state_no == 0 {
                returned = true;
                break;
            }
        }
        assert!(returned, "walk should return to stand once Time >= 2 and command released");
    }

    // ---- AC1: persistent counts collide across special and current states ----
    //
    // This documents a real keying subtlety: fire_counts is keyed by
    // (self.state_no, idx) where self.state_no is the CURRENT numbered state even
    // while a special (-3/-2/-1) state is running. A special-state controller and
    // a current-state controller that share the same index therefore share one
    // persistent count. With persistent=1 (the default) this is harmless because
    // every qualifying tick fires regardless of count; the test pins that
    // default-persistent behavior (the common case) so a future change to the
    // keying is caught.

    #[test]
    fn default_persistent_unaffected_by_special_current_index_overlap() {
        // -2 idx0 and current-0 idx0 both default-persistent VelAdds. Both must
        // fire every tick regardless of the shared (state_no, idx) key.
        let s_neg2 = ctrl(-2, "VelAdd", &[], &[(1, &["1"])], None, &[("y", "100")]);
        let s_cur = ctrl(0, "VelAdd", &[], &[(1, &["1"])], None, &[("y", "1")]);
        let lc = loaded(
            vec![stand_n(-2, vec![s_neg2]), stand_n(0, vec![s_cur])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::<f32>::ZERO;
        lc.tick(&mut ch);
        lc.tick(&mut ch);
        // Each tick: +100 (from -2) and +1 (current) → 2 ticks = 202.
        assert!((ch.vel.y - 202.0).abs() < 1e-6, "both fire every tick; got {}", ch.vel.y);
    }

    // ---- AC1/AC4: in-tick re-entry chains follow ChangeState in the same tick --

    #[test]
    fn change_state_chain_resolves_within_one_tick() {
        // A ChangeState lands in a state whose own controller immediately fires
        // another ChangeState: MUGEN follows the chain within the same tick. Here
        // 0 → 1 → 2 all resolve in one tick, ending in state 2.
        let go1 = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "1")]);
        let go2 = ctrl(1, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "2")]);
        let lc = loaded(
            vec![
                stand_n(0, vec![go1]),
                stand_n(1, vec![go2]),
                stand_n(2, vec![]),
            ],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        let report = lc.tick(&mut ch);
        assert_eq!(ch.state_no, 2, "chain 0->1->2 resolved this tick");
        assert_eq!(report.transitions, 2);
        assert!(!report.transition_cap_hit);
        // prev_state_no reflects the LAST hop (1 -> 2).
        assert_eq!(ch.prev_state_no, 1);
    }

    // =====================================================================
    // Task 5.4: core MOVEMENT/CONTROL controllers + the remaining 5.3
    // review follow-ups (#2 prev_state_no after a -1 ChangeState, #3
    // special-vs-current persistent=0 collision keyed by ctrl.state_number).
    // =====================================================================

    // ---- 5.4 AC: ChangeAnim resets the element/time cursor ----

    #[test]
    fn change_anim_sets_anim_and_resets_cursor() {
        // ChangeAnim value=5 must switch the anim and reset elem/elem_time to the
        // start of the new action (then the per-tick advance moves elem_time to 1).
        let c = ctrl(0, "ChangeAnim", &[], &[(1, &["1"])], None, &[("value", "5")]);
        let lc = loaded(vec![stand_n(0, vec![c])], {
            let mut air = tiny_air(0, &[5]);
            add_action(&mut air, 5, &[10, 10]);
            air
        });
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.anim_elem = 3;
        ch.anim_elem_time = 42;
        lc.tick(&mut ch);
        assert_eq!(ch.anim, 5, "anim switched");
        assert_eq!(ch.anim_elem, 0, "element reset");
        assert_eq!(ch.anim_elem_time, 1, "elem time reset to 0 then advanced one tick");
    }

    #[test]
    fn change_anim_with_elem_starts_at_one_based_element() {
        // ChangeAnim with elem=2 starts at one-based element 2 → zero-based 1.
        let c = ctrl(0, "ChangeAnim", &[], &[(1, &["1"])], None, &[("value", "5"), ("elem", "2")]);
        let lc = loaded(vec![stand_n(0, vec![c])], {
            let mut air = tiny_air(0, &[5]);
            add_action(&mut air, 5, &[10, 10, 10]);
            air
        });
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        lc.tick(&mut ch);
        assert_eq!(ch.anim, 5);
        assert_eq!(ch.anim_elem, 1, "elem=2 (one-based) → zero-based 1");
    }

    #[test]
    fn change_anim2_aliases_change_anim() {
        // ChangeAnim2 behaves as ChangeAnim for a single entity.
        let c = ctrl(0, "ChangeAnim2", &[], &[(1, &["1"])], None, &[("value", "5")]);
        let lc = loaded(vec![stand_n(0, vec![c])], {
            let mut air = tiny_air(0, &[5]);
            add_action(&mut air, 5, &[10]);
            air
        });
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        lc.tick(&mut ch);
        assert_eq!(ch.anim, 5, "ChangeAnim2 switched the anim like ChangeAnim");
    }

    #[test]
    fn change_anim_without_value_is_safe_noop() {
        let c = ctrl(0, "ChangeAnim", &[], &[(1, &["1"])], None, &[]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 7;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1, "dispatch ran");
        assert_eq!(ch.anim, 7, "no value → anim unchanged");
    }

    // ---- 5.4 AC: PosSet / PosAdd move the entity ----

    #[test]
    fn pos_set_sets_components_and_missing_axis_unchanged() {
        let only_x = ctrl(0, "PosSet", &[], &[(1, &["1"])], None, &[("x", "50")]);
        let lc = loaded(vec![stand_n(0, vec![only_x])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        // Y is negative (above the floor) so the per-tick ground clamp leaves it
        // alone — this test is about PosSet's per-axis behavior, not the floor.
        ch.pos = Vec2::new(1.0, -2.0);
        lc.tick(&mut ch);
        assert!((ch.pos.x - 50.0).abs() < 1e-6, "x set");
        assert!((ch.pos.y - (-2.0)).abs() < 1e-6, "y left unchanged");

        let both = ctrl(0, "PosSet", &[], &[(1, &["1"])], None, &[("x", "3"), ("y", "-4")]);
        let lc2 = loaded(vec![stand_n(0, vec![both])], tiny_air(0, &[5]));
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.physics = Physics::None;
        ch2.pos = Vec2::new(0.0, 0.0);
        lc2.tick(&mut ch2);
        assert!((ch2.pos.x - 3.0).abs() < 1e-6);
        assert!((ch2.pos.y - (-4.0)).abs() < 1e-6);
    }

    #[test]
    fn pos_add_accumulates_both_axes() {
        // Default facing is Right (sign +1), so PosAdd x adds as written.
        let add = ctrl(0, "PosAdd", &[], &[(1, &["1"])], None, &[("x", "2"), ("y", "-1")]);
        let lc = loaded(vec![stand_n(0, vec![add])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.facing = Facing::Right;
        // Negative (airborne) Y so the per-tick ground clamp does not interfere;
        // this test exercises PosAdd accumulation on both axes.
        ch.pos = Vec2::new(10.0, -10.0);
        lc.tick(&mut ch);
        assert!((ch.pos.x - 12.0).abs() < 1e-6);
        assert!((ch.pos.y - (-11.0)).abs() < 1e-6);
    }

    // ---- 6.2c: facing-relative velocity / position integration --------------

    #[test]
    fn integration_facing_right_positive_vel_moves_plus_x() {
        // A facing-RIGHT character with vel.x = +V advances toward +x. No
        // controllers fire (empty state); Physics::None leaves velocity intact so
        // the only motion is the world-position integration `pos.x += vel.x * +1`.
        let lc = loaded(vec![stand_n(0, vec![])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.facing = Facing::Right;
        ch.pos = Vec2::<f32>::ZERO;
        ch.vel = Vec2::new(3.0, 0.0);
        lc.tick(&mut ch);
        assert!((ch.pos.x - 3.0).abs() < 1e-6, "facing right + vel.x=+3 -> +x; got {}", ch.pos.x);
        // The stored velocity is unchanged (facing-relative, not mirrored).
        assert!((ch.vel.x - 3.0).abs() < 1e-6, "stored vel.x stays facing-relative (+3)");
    }

    #[test]
    fn integration_facing_left_same_positive_vel_moves_minus_x() {
        // A facing-LEFT character with the SAME stored vel.x = +V advances toward
        // -x: the integration mirrors the X by facing (`pos.x += vel.x * -1`),
        // while the stored vel.x is left facing-relative (+V).
        let lc = loaded(vec![stand_n(0, vec![])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.facing = Facing::Left;
        ch.pos = Vec2::<f32>::ZERO;
        ch.vel = Vec2::new(3.0, 0.0);
        lc.tick(&mut ch);
        assert!((ch.pos.x - (-3.0)).abs() < 1e-6, "facing left + vel.x=+3 -> -x; got {}", ch.pos.x);
        // Stored velocity is still +3 (facing-relative), NOT mirrored to -3.
        assert!((ch.vel.x - 3.0).abs() < 1e-6, "stored vel.x stays facing-relative (+3) when facing left");
    }

    #[test]
    fn integration_y_is_never_mirrored_by_facing() {
        // The Y axis is integrated as-is regardless of facing.
        let lc = loaded(vec![stand_n(0, vec![])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.facing = Facing::Left;
        ch.pos = Vec2::<f32>::ZERO;
        ch.vel = Vec2::new(0.0, -4.0);
        lc.tick(&mut ch);
        assert!((ch.pos.y - (-4.0)).abs() < 1e-6, "y integrated unmirrored even facing left");
        assert!((ch.pos.x - 0.0).abs() < 1e-6, "no x velocity -> no x motion");
    }

    #[test]
    fn vel_x_trigger_is_facing_relative_for_both_facings() {
        // The `Vel X` trigger returns the STORED (facing-relative) velocity for
        // both facings — it is never mirrored. This is what common1.cns relies on:
        // `vel x > 0` selects the walk-forward anim regardless of facing.
        let mut right = Character::new();
        right.facing = Facing::Right;
        right.vel = Vec2::new(2.4, 0.0);
        let mut left = Character::new();
        left.facing = Facing::Left;
        left.vel = Vec2::new(2.4, 0.0);
        // X axis is encoded as 0 (see Character::axis_component).
        let vx_right = EvalContext::trigger(&right, "Vel", &[Value::Int(0)]).to_float();
        let vx_left = EvalContext::trigger(&left, "Vel", &[Value::Int(0)]).to_float();
        assert!((vx_right - 2.4).abs() < 1e-6, "facing right Vel X = +2.4");
        assert!(
            (vx_left - 2.4).abs() < 1e-6,
            "facing left Vel X stays facing-relative (+2.4), not mirrored; got {vx_left}"
        );
    }

    #[test]
    fn pos_x_trigger_is_absolute_for_both_facings() {
        // The `Pos X` trigger reports the ABSOLUTE stage position, never mirrored
        // by facing. A facing-left character at stage x = 50 reads Pos X = 50.
        let mut left = Character::new();
        left.facing = Facing::Left;
        left.pos = Vec2::new(50.0, 0.0);
        let px = EvalContext::trigger(&left, "Pos", &[Value::Int(0)]).to_float();
        assert!((px - 50.0).abs() < 1e-6, "Pos X is absolute stage position; got {px}");
    }

    #[test]
    fn pos_add_is_facing_relative_on_x() {
        // PosAdd x is mirrored by facing: facing right, x=+5 -> +5; facing left,
        // the SAME x=+5 -> -5 (forward in both cases). Physics::None + zero vel so
        // the integration adds nothing and we observe PosAdd in isolation.
        // PosAdd y is negative (upward / above the floor) so the per-tick ground
        // clamp leaves it untouched; this test is about facing-relative x.
        let add = ctrl(0, "PosAdd", &[], &[(1, &["1"])], None, &[("x", "5"), ("y", "-2")]);
        let lc = loaded(vec![stand_n(0, vec![add.clone()])], tiny_air(0, &[5]));
        let mut right = Character::new();
        right.state_no = 0;
        right.physics = Physics::None;
        right.facing = Facing::Right;
        right.pos = Vec2::<f32>::ZERO;
        lc.tick(&mut right);
        assert!((right.pos.x - 5.0).abs() < 1e-6, "facing right PosAdd x=+5 -> +5");
        assert!((right.pos.y - (-2.0)).abs() < 1e-6, "PosAdd y is never mirrored");

        let lc2 = loaded(vec![stand_n(0, vec![add])], tiny_air(0, &[5]));
        let mut left = Character::new();
        left.state_no = 0;
        left.physics = Physics::None;
        left.facing = Facing::Left;
        left.pos = Vec2::<f32>::ZERO;
        lc2.tick(&mut left);
        assert!((left.pos.x - (-5.0)).abs() < 1e-6, "facing left PosAdd x=+5 -> -5 (forward)");
        assert!((left.pos.y - (-2.0)).abs() < 1e-6, "PosAdd y unmirrored facing left");
    }

    #[test]
    fn pos_set_is_absolute_not_facing_relative() {
        // PosSet writes the absolute stage x regardless of facing.
        // Y is set above the floor (negative) so the ground clamp is a no-op here;
        // this test verifies PosSet writes the absolute x irrespective of facing.
        let set = ctrl(0, "PosSet", &[], &[(1, &["1"])], None, &[("x", "7"), ("y", "-1")]);
        let lc = loaded(vec![stand_n(0, vec![set])], tiny_air(0, &[5]));
        let mut left = Character::new();
        left.state_no = 0;
        left.physics = Physics::None;
        left.facing = Facing::Left;
        left.pos = Vec2::new(100.0, -100.0);
        lc.tick(&mut left);
        assert!((left.pos.x - 7.0).abs() < 1e-6, "PosSet x is absolute (7), not mirrored; got {}", left.pos.x);
        assert!((left.pos.y - (-1.0)).abs() < 1e-6);
    }

    // ---- 5.4 AC: VarSet / VarAdd across int/float/sys banks ----

    #[test]
    fn var_set_indexed_keys_target_correct_bank() {
        // var(1), fvar(2), sysvar(3), sysfvar(4) each set their own bank.
        let set_int = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("var(1)", "7")]);
        let set_float = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("fvar(2)", "1.5")]);
        let set_sys = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("sysvar(3)", "9")]);
        let set_sysf = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("sysfvar(4)", "2.5")]);
        let lc = loaded(
            vec![stand_n(0, vec![set_int, set_float, set_sys, set_sysf])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert_eq!(ch.vars[1], 7, "var(1) set in int bank");
        assert!((ch.fvars[2] - 1.5).abs() < 1e-6, "fvar(2) set in float bank");
        assert_eq!(ch.sysvars[3], 9, "sysvar(3) set in sys int bank");
        assert!((ch.sysfvars[4] - 2.5).abs() < 1e-6, "sysfvar(4) set in sys float bank");
    }

    #[test]
    fn var_set_v_value_form_targets_int_bank() {
        // The `v = i` + `value = expr` form sets the integer bank at index i.
        let c = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("v", "5"), ("value", "42")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert_eq!(ch.vars[5], 42);
    }

    #[test]
    fn var_set_fv_value_form_targets_float_bank() {
        let c = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("fv", "3"), ("value", "0.25")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert!((ch.fvars[3] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn var_add_accumulates_in_int_and_float_banks() {
        let add_int = ctrl(0, "VarAdd", &[], &[(1, &["1"])], None, &[("var(0)", "3")]);
        let add_float = ctrl(0, "VarAdd", &[], &[(1, &["1"])], None, &[("fvar(0)", "1.5")]);
        let lc = loaded(vec![stand_n(0, vec![add_int, add_float])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vars[0] = 10;
        ch.fvars[0] = 2.0;
        lc.tick(&mut ch);
        assert_eq!(ch.vars[0], 13);
        assert!((ch.fvars[0] - 3.5).abs() < 1e-6);
    }

    #[test]
    fn var_set_out_of_range_index_is_safe_noop() {
        // An index beyond the bank size must not panic and must change nothing.
        let c = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("var(999)", "1")]);
        let neg = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("v", "-1"), ("value", "1")]);
        let lc = loaded(vec![stand_n(0, vec![c, neg])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 2, "both dispatched without panic");
        assert!(ch.vars.iter().all(|&v| v == 0), "no slot was written");
    }

    // ---- 5.4 AC: VarRangeSet sets a contiguous range ----

    #[test]
    fn var_range_set_sets_int_range_inclusive() {
        let c = ctrl(
            0,
            "VarRangeSet",
            &[],
            &[(1, &["1"])],
            None,
            &[("value", "5"), ("first", "2"), ("last", "4")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert_eq!(ch.vars[1], 0, "below range untouched");
        assert_eq!(ch.vars[2], 5);
        assert_eq!(ch.vars[3], 5);
        assert_eq!(ch.vars[4], 5);
        assert_eq!(ch.vars[5], 0, "above range untouched");
    }

    #[test]
    fn var_range_set_float_bank_via_fvalue() {
        let c = ctrl(
            0,
            "VarRangeSet",
            &[],
            &[(1, &["1"])],
            None,
            &[("fvalue", "1.0"), ("first", "0"), ("last", "2")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert!((ch.fvars[0] - 1.0).abs() < 1e-6);
        assert!((ch.fvars[1] - 1.0).abs() < 1e-6);
        assert!((ch.fvars[2] - 1.0).abs() < 1e-6);
        assert!((ch.fvars[3] - 0.0).abs() < 1e-6, "above range untouched");
    }

    #[test]
    fn var_range_set_default_range_covers_whole_bank_without_panic() {
        // No first/last → whole int bank set; the upper bound equals the bank max
        // so the inclusive loop never indexes out of range.
        let c = ctrl(0, "VarRangeSet", &[], &[(1, &["1"])], None, &[("value", "8")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert!(ch.vars.iter().all(|&v| v == 8), "whole int bank set to 8");
    }

    // ---- 5.4 AC: StateTypeSet updates the category flags ----

    #[test]
    fn state_type_set_updates_statetype_movetype_physics() {
        let c = ctrl(
            0,
            "StateTypeSet",
            &[],
            &[(1, &["1"])],
            None,
            &[("statetype", "A"), ("movetype", "A"), ("physics", "A")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.state_type = StateType::Standing;
        ch.move_type = MoveType::Idle;
        lc.tick(&mut ch);
        assert_eq!(ch.state_type, StateType::Air);
        assert_eq!(ch.move_type, MoveType::Attack);
        assert_eq!(ch.physics, Physics::Air);
    }

    #[test]
    fn state_type_set_partial_and_unchanged_token_keep_others() {
        // Only movetype given → statetype/physics untouched. A `U` token is the
        // explicit "unchanged" no-op.
        let c = ctrl(
            0,
            "StateTypeSet",
            &[],
            &[(1, &["1"])],
            None,
            &[("movetype", "H"), ("statetype", "U")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.state_type = StateType::Crouching;
        ch.move_type = MoveType::Idle;
        lc.tick(&mut ch);
        assert_eq!(ch.move_type, MoveType::BeingHit, "movetype updated");
        assert_eq!(ch.state_type, StateType::Crouching, "U token left statetype unchanged");
    }

    // ---- 5.4 AC: Turn flips facing ----

    #[test]
    fn turn_flips_facing() {
        let c = ctrl(0, "Turn", &[], &[(1, &["1"])], Some("0"), &[]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.facing = Facing::Right;
        lc.tick(&mut ch);
        assert_eq!(ch.facing, Facing::Left, "Turn flipped right → left");
        // A second entry (persistent=0 re-arms on re-entry, but here we just call
        // the controller method semantics directly via a fresh char).
        let mut ch2 = Character::new();
        ch2.state_no = 0;
        ch2.physics = Physics::None;
        ch2.facing = Facing::Left;
        lc.tick(&mut ch2);
        assert_eq!(ch2.facing, Facing::Right, "Turn flipped left → right");
    }

    // ---- 5.4 / 8.3a AC: PlaySnd never mutates character state ----

    #[test]
    fn play_snd_does_not_mutate_character_state() {
        // PlaySnd emits a request but must not panic or mutate the character.
        let c = ctrl(0, "PlaySnd", &[], &[(1, &["1"])], None, &[("value", "1, 0")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(3.0, 4.0);
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1, "PlaySnd dispatched");
        assert_eq!(report.sound_requests.len(), 1, "one request emitted");
        assert!((ch.vel.x - 3.0).abs() < 1e-6);
        assert!((ch.vel.y - 4.0).abs() < 1e-6);
    }

    #[test]
    fn play_snd_without_value_does_not_panic() {
        let c = ctrl(0, "PlaySnd", &[], &[(1, &["1"])], None, &[]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1);
        assert!(
            report.sound_requests.is_empty(),
            "missing value → no request"
        );
    }

    // ---- 8.3a AC: PlaySnd emits SoundRequest into TickReport ----

    /// Helper: build a single-PlaySnd state, run one tick, return the report.
    fn play_snd_tick(params: &[(&str, &str)]) -> TickReport {
        let c = ctrl(0, "PlaySnd", &[], &[(1, &["1"])], None, params);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch)
    }

    #[test]
    fn play_snd_simple_value_emits_one_request_with_defaults() {
        let report = play_snd_tick(&[("value", "1, 0")]);
        assert_eq!(
            report.sound_requests,
            vec![SoundRequest {
                group: 1,
                sample: 0,
                channel: -1,
                volume_scale: 100,
                looping: false,
                common: false,
            }]
        );
    }

    #[test]
    fn play_snd_f_prefix_sets_common() {
        // `value = F0, 5` → common/fight sound file, group 0, sample 5.
        let report = play_snd_tick(&[("value", "F0, 5")]);
        let req = &report.sound_requests[0];
        assert!(req.common, "F prefix → common = true");
        assert_eq!(req.group, 0);
        assert_eq!(req.sample, 5);
        // Lowercase `f` is honored too.
        let report = play_snd_tick(&[("value", "f3, 2")]);
        assert!(report.sound_requests[0].common);
        assert_eq!(report.sound_requests[0].group, 3);
    }

    #[test]
    fn play_snd_s_prefix_is_own_snd_but_parses_digits() {
        // `S` (own .snd) → common = false, digits still parsed.
        let report = play_snd_tick(&[("value", "S7, 4")]);
        let req = &report.sound_requests[0];
        assert!(!req.common, "S prefix → common = false");
        assert_eq!(req.group, 7);
        assert_eq!(req.sample, 4);
    }

    #[test]
    fn play_snd_honors_channel_volumescale_and_loop() {
        let report = play_snd_tick(&[
            ("value", "2, 3"),
            ("channel", "5"),
            ("volumescale", "75"),
            ("loop", "1"),
        ]);
        let req = &report.sound_requests[0];
        assert_eq!(req.channel, 5);
        assert_eq!(req.volume_scale, 75);
        assert!(req.looping, "loop = 1 → looping");

        // loop = -1 is also looping; loop = 0 is not; textual `true` loops.
        assert!(play_snd_tick(&[("value", "1,0"), ("loop", "-1")]).sound_requests[0].looping);
        assert!(!play_snd_tick(&[("value", "1,0"), ("loop", "0")]).sound_requests[0].looping);
        assert!(play_snd_tick(&[("value", "1,0"), ("loop", "true")]).sound_requests[0].looping);
    }

    #[test]
    fn play_snd_garbage_value_emits_no_request() {
        // Non-numeric group, non-numeric sample, and a value missing the sample
        // each push NO request and must not panic.
        assert!(play_snd_tick(&[("value", "abc, 0")]).sound_requests.is_empty());
        assert!(play_snd_tick(&[("value", "1, xyz")]).sound_requests.is_empty());
        assert!(play_snd_tick(&[("value", "1")]).sound_requests.is_empty());
        assert!(play_snd_tick(&[("value", "")]).sound_requests.is_empty());
        // A bare `F` flag with no digits is unparseable → no request.
        assert!(play_snd_tick(&[("value", "F, 5")]).sound_requests.is_empty());
    }

    #[test]
    fn sound_requests_empty_on_tick_without_play_snd() {
        // A state whose only controller is a VelSet emits no sound requests.
        let c = ctrl(0, "VelSet", &[], &[(1, &["1"])], None, &[("x", "1")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert!(
            report.sound_requests.is_empty(),
            "no PlaySnd → empty sound_requests"
        );
    }

    #[test]
    fn parse_loop_flag_bool_ish() {
        assert!(parse_loop_flag("1"));
        assert!(parse_loop_flag("-1"));
        assert!(parse_loop_flag("true"));
        assert!(parse_loop_flag("TRUE"));
        assert!(!parse_loop_flag("0"));
        assert!(!parse_loop_flag("false"));
        assert!(!parse_loop_flag(""));
        assert!(!parse_loop_flag("2"));
    }

    // ---- P8a: Target* controllers emit deferred TargetOps ----

    /// Helper: run a single `Target*` controller for one tick with `has_target`
    /// set as requested, returning the report's `target_ops`.
    fn target_tick(kind: &str, params: &[(&str, &str)], has_target: bool) -> Vec<TargetOp> {
        let c = ctrl(0, kind, &[], &[(1, &["1"])], None, params);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.has_target = has_target;
        lc.tick(&mut ch).target_ops
    }

    #[test]
    fn target_state_emits_state_op() {
        let ops = target_tick("TargetState", &[("value", "820")], true);
        assert_eq!(ops, vec![TargetOp::State(820)]);
    }

    #[test]
    fn target_bind_emits_bind_op_with_time_and_pos() {
        let ops = target_tick("TargetBind", &[("time", "3"), ("pos", "5, -2")], true);
        assert_eq!(
            ops,
            vec![TargetOp::Bind {
                time: 3,
                pos: (5.0, -2.0)
            }]
        );
    }

    #[test]
    fn target_bind_defaults_time_one_and_pos_zero() {
        // Absent time defaults to MUGEN's 1; absent pos axes default to 0.0.
        let ops = target_tick("TargetBind", &[], true);
        assert_eq!(
            ops,
            vec![TargetOp::Bind {
                time: 1,
                pos: (0.0, 0.0)
            }]
        );
    }

    #[test]
    fn target_life_add_emits_value_and_kill() {
        let ops = target_tick(
            "TargetLifeAdd",
            &[("value", "-40"), ("kill", "1")],
            true,
        );
        assert_eq!(
            ops,
            vec![TargetOp::LifeAdd {
                value: -40,
                kill: true
            }]
        );
    }

    #[test]
    fn target_life_add_kill_defaults_true_and_honors_zero() {
        // Absent kill defaults to MUGEN's true (lethal allowed).
        let dflt = target_tick("TargetLifeAdd", &[("value", "-10")], true);
        assert_eq!(
            dflt,
            vec![TargetOp::LifeAdd {
                value: -10,
                kill: true
            }]
        );
        // Explicit kill = 0 → not lethal.
        let no_kill = target_tick("TargetLifeAdd", &[("value", "-10"), ("kill", "0")], true);
        assert_eq!(
            no_kill,
            vec![TargetOp::LifeAdd {
                value: -10,
                kill: false
            }]
        );
    }

    #[test]
    fn target_facing_emits_facing_op() {
        assert_eq!(
            target_tick("TargetFacing", &[("value", "-1")], true),
            vec![TargetOp::Facing(-1)]
        );
        assert_eq!(
            target_tick("TargetFacing", &[("value", "1")], true),
            vec![TargetOp::Facing(1)]
        );
    }

    #[test]
    fn target_vel_set_and_add_emit_pairs() {
        assert_eq!(
            target_tick("TargetVelSet", &[("x", "4"), ("y", "-6")], true),
            vec![TargetOp::VelSet((4.0, -6.0))]
        );
        assert_eq!(
            target_tick("TargetVelAdd", &[("x", "1.5")], true),
            vec![TargetOp::VelAdd((1.5, 0.0))],
            "absent y defaults to 0.0"
        );
    }

    #[test]
    fn target_power_add_emits_value() {
        assert_eq!(
            target_tick("TargetPowerAdd", &[("value", "500")], true),
            vec![TargetOp::PowerAdd(500)]
        );
    }

    #[test]
    fn target_controllers_are_noops_without_target() {
        // With has_target = false every Target* controller pushes nothing.
        assert!(target_tick("TargetState", &[("value", "820")], false).is_empty());
        assert!(target_tick("TargetBind", &[("time", "3"), ("pos", "5, -2")], false).is_empty());
        assert!(target_tick("TargetLifeAdd", &[("value", "-40")], false).is_empty());
        assert!(target_tick("TargetFacing", &[("value", "-1")], false).is_empty());
        assert!(target_tick("TargetVelSet", &[("x", "4"), ("y", "-6")], false).is_empty());
        assert!(target_tick("TargetVelAdd", &[("x", "1.5")], false).is_empty());
        assert!(target_tick("TargetPowerAdd", &[("value", "500")], false).is_empty());
    }

    #[test]
    fn target_controllers_with_missing_required_value_push_nothing() {
        // value-less State/LifeAdd/Facing/PowerAdd are safe no-ops even WITH a target.
        assert!(target_tick("TargetState", &[], true).is_empty());
        assert!(target_tick("TargetLifeAdd", &[], true).is_empty());
        assert!(target_tick("TargetFacing", &[], true).is_empty());
        assert!(target_tick("TargetPowerAdd", &[], true).is_empty());
    }

    #[test]
    fn target_ops_empty_on_tick_without_target_controller() {
        // A fresh TickReport carries no target_ops when no Target* fired.
        let c = ctrl(0, "Null", &[], &[(1, &["1"])], None, &[]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.has_target = true;
        assert!(lc.tick(&mut ch).target_ops.is_empty());
    }

    // ---- P8a (Proctor): additional edge / MUGEN-semantics coverage ----------

    /// A fresh `Character` has no target: the default is `false`, so every
    /// `Target*` controller is a no-op until a hit establishes a target.
    #[test]
    fn fresh_character_has_no_target_by_default() {
        let ch = Character::new();
        assert!(!ch.has_target, "has_target defaults to false");
        assert!(!Character::default().has_target);
    }

    /// AC2/AC3: the dispatch matches the controller type **case-insensitively**
    /// (MUGEN controller names are not case-sensitive), so lowercase / uppercase
    /// spellings still emit the right `TargetOp`.
    #[test]
    fn target_dispatch_is_case_insensitive() {
        assert_eq!(
            target_tick("targetstate", &[("value", "820")], true),
            vec![TargetOp::State(820)]
        );
        assert_eq!(
            target_tick("TARGETPOWERADD", &[("value", "300")], true),
            vec![TargetOp::PowerAdd(300)]
        );
        assert_eq!(
            target_tick("TargetVelAdd", &[("x", "2"), ("y", "3")], true),
            vec![TargetOp::VelAdd((2.0, 3.0))]
        );
    }

    /// AC2: params are evaluated through the expression VM, not parsed as raw
    /// literals. A `TargetState` whose `value` is an arithmetic expression
    /// resolves to the computed state number.
    #[test]
    fn target_state_value_is_an_evaluated_expression() {
        let ops = target_tick("TargetState", &[("value", "800 + 20")], true);
        assert_eq!(ops, vec![TargetOp::State(820)]);
    }

    /// AC2: param expressions read live character state. With `var(0) = 815` the
    /// `TargetState` value expression `var(0)` resolves to that state number,
    /// proving the existing `eval_param` helper is actually wired in.
    #[test]
    fn target_state_value_reads_character_var() {
        let c = ctrl(0, "TargetState", &[], &[(1, &["1"])], None, &[("value", "var(0)")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.has_target = true;
        ch.vars[0] = 815;
        assert_eq!(lc.tick(&mut ch).target_ops, vec![TargetOp::State(815)]);
    }

    /// AC2: `TargetLifeAdd`'s `value` is evaluated, and a fractional float result
    /// truncates toward zero (MUGEN life deltas are integers).
    #[test]
    fn target_life_add_value_truncates_float_expression() {
        let ops = target_tick("TargetLifeAdd", &[("value", "-12.9")], true);
        assert_eq!(
            ops,
            vec![TargetOp::LifeAdd {
                value: -12,
                kill: true
            }]
        );
    }

    /// MUGEN treats any non-zero `kill` as true, not only literal `1`. A
    /// `kill = 2` expression is truthy → lethal allowed.
    #[test]
    fn target_life_add_kill_is_truthy_not_literal_one() {
        let ops = target_tick("TargetLifeAdd", &[("value", "-10"), ("kill", "2")], true);
        assert_eq!(
            ops,
            vec![TargetOp::LifeAdd {
                value: -10,
                kill: true
            }]
        );
    }

    /// `TargetVelSet`/`TargetVelAdd` with NEITHER axis given default both x and y
    /// to `0.0` (rather than skipping the op): a vel controller with no params is
    /// a real, zeroed emission.
    #[test]
    fn target_vel_set_defaults_both_axes_to_zero() {
        assert_eq!(
            target_tick("TargetVelSet", &[], true),
            vec![TargetOp::VelSet((0.0, 0.0))]
        );
        assert_eq!(
            target_tick("TargetVelAdd", &[], true),
            vec![TargetOp::VelAdd((0.0, 0.0))]
        );
    }

    /// `TargetBind` with only an `x` in `pos` leaves `y` at the `0.0` default;
    /// `time` still defaults to MUGEN's `1`.
    #[test]
    fn target_bind_single_pos_component_defaults_y() {
        let ops = target_tick("TargetBind", &[("pos", "7")], true);
        assert_eq!(
            ops,
            vec![TargetOp::Bind {
                time: 1, // absent `time` → MUGEN default
                pos: (7.0, 0.0)
            }]
        );
    }

    /// AC2/AC3: multiple `Target*` controllers in one state emit their ops in
    /// **fire order** onto the single per-tick `target_ops` vec — the exact KFM
    /// throw (state 810) shape: Bind, then State, LifeAdd, Facing each tick.
    #[test]
    fn multiple_target_controllers_emit_in_fire_order() {
        let bind = ctrl(0, "TargetBind", &[], &[(1, &["1"])], None, &[("time", "1"), ("pos", "10, 0")]);
        let state_c = ctrl(0, "TargetState", &[], &[(1, &["1"])], None, &[("value", "820")]);
        let life = ctrl(0, "TargetLifeAdd", &[], &[(1, &["1"])], None, &[("value", "-13"), ("kill", "0")]);
        let face = ctrl(0, "TargetFacing", &[], &[(1, &["1"])], None, &[("value", "-1")]);
        let lc = loaded(
            vec![stand_n(0, vec![bind, state_c, life, face])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.has_target = true;
        let ops = lc.tick(&mut ch).target_ops;
        assert_eq!(
            ops,
            vec![
                TargetOp::Bind { time: 1, pos: (10.0, 0.0) },
                TargetOp::State(820),
                TargetOp::LifeAdd { value: -13, kill: false },
                TargetOp::Facing(-1),
            ],
            "ops preserve controller fire order"
        );
    }

    /// AC1: `target_ops` is rebuilt empty each tick — emissions never carry
    /// across ticks. A persistent `TargetState` pushes exactly one op on tick 1
    /// AND exactly one (not two) on tick 2.
    #[test]
    fn target_ops_do_not_accumulate_across_ticks() {
        let c = ctrl(0, "TargetState", &[], &[(1, &["1"])], None, &[("value", "820")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.has_target = true;
        let t1 = lc.tick(&mut ch).target_ops;
        assert_eq!(t1, vec![TargetOp::State(820)], "tick 1 emits one op");
        let t2 = lc.tick(&mut ch).target_ops;
        assert_eq!(t2, vec![TargetOp::State(820)], "tick 2 emits one op, not two");
    }

    /// A `Target*` controller gated off by its trigger never runs, so emits
    /// nothing even with a target — confirming the op only fires when the
    /// controller actually qualifies.
    #[test]
    fn gated_off_target_controller_emits_nothing() {
        let c = ctrl(0, "TargetState", &[], &[(1, &["0"])], None, &[("value", "820")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.has_target = true;
        assert!(lc.tick(&mut ch).target_ops.is_empty());
    }

    /// `TickReport::default()` (and a no-controller tick) leaves `target_ops`
    /// empty — the field starts clear, like `sound_requests`.
    #[test]
    fn tick_report_default_target_ops_is_empty() {
        assert!(TickReport::default().target_ops.is_empty());
    }

    // ---- 5.4 helper: parse_var_bank_key unit coverage ----

    #[test]
    fn parse_var_bank_key_recognizes_all_banks() {
        assert_eq!(parse_var_bank_key("var(0)"), Some((VarBank::Int, 0)));
        assert_eq!(parse_var_bank_key("fvar(12)"), Some((VarBank::Float, 12)));
        assert_eq!(parse_var_bank_key("sysvar(3)"), Some((VarBank::SysInt, 3)));
        assert_eq!(parse_var_bank_key("sysfvar(4)"), Some((VarBank::SysFloat, 4)));
        // Whitespace inside the parens is tolerated.
        assert_eq!(parse_var_bank_key("var( 7 )"), Some((VarBank::Int, 7)));
        // sysvar must not be mis-parsed as the `var` bank.
        assert_ne!(parse_var_bank_key("sysvar(1)").map(|(b, _)| b), Some(VarBank::Int));
        // Non-var keys and malformed forms → None.
        assert_eq!(parse_var_bank_key("value"), None);
        assert_eq!(parse_var_bank_key("var"), None);
        assert_eq!(parse_var_bank_key("var()"), None);
        assert_eq!(parse_var_bank_key("var(x)"), None);
    }

    // ---- 5.3 review fix (2): prev_state_no correct after a -1 ChangeState ----

    #[test]
    fn prev_state_no_correct_after_special_state_change_state() {
        // A ChangeState fired from [Statedef -1] (the command bridge) sends us from
        // state 7 to state 50. prev_state_no must record 7 (the state we left),
        // not -1 (the special state that issued the ChangeState).
        let cmd = ctrl(-1, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "50")]);
        let lc = loaded(
            vec![
                stand_n(-1, vec![cmd]),
                stand_n(7, vec![]),
                stand_n(50, vec![]),
            ],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 7;
        ch.prev_state_no = -999;
        let report = lc.tick(&mut ch);
        assert!(report.transitions >= 1);
        assert_eq!(ch.state_no, 50, "-1 ChangeState redirected the current state");
        assert_eq!(ch.prev_state_no, 7, "prev_state_no is the state we left, not -1");
    }

    // ---- 5.3 review fix (3): fire_counts keyed by ctrl.state_number ----

    #[test]
    fn persistent_zero_collision_resolved_across_special_and_current() {
        // A persistent=0 controller at index 0 in special state -2 AND a
        // persistent=0 controller at index 0 in the current state 0. Keying
        // fire_counts by ctrl.state_number (not self.state_no) keeps their
        // once-per-entry counts independent, so BOTH fire on the first tick.
        // (If they shared a key, the second to qualify would see count==2 and be
        // suppressed by persistent=0.)
        let in_neg2 = ctrl(-2, "VarAdd", &[], &[(1, &["1"])], Some("0"), &[("var(0)", "10")]);
        let in_cur = ctrl(0, "VarAdd", &[], &[(1, &["1"])], Some("0"), &[("var(1)", "1")]);
        let lc = loaded(
            vec![stand_n(-2, vec![in_neg2]), stand_n(0, vec![in_cur])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 2, "both once-per-entry controllers fired");
        assert_eq!(ch.vars[0], 10, "special -2 idx0 fired");
        assert_eq!(ch.vars[1], 1, "current 0 idx0 fired despite same index");
        // A second tick: each is once-per-entry, neither refires.
        let report2 = lc.tick(&mut ch);
        assert_eq!(report2.controllers_fired, 0, "both already fired this entry");
        assert_eq!(ch.vars[0], 10);
        assert_eq!(ch.vars[1], 1);
    }

    // =====================================================================
    // Proctor (task 5.4): edge-case, error-path, and MUGEN-semantics coverage
    // for the new controllers + the 5.3 review follow-ups, layered on top of
    // Forge's tests. Each block names the acceptance criterion it exercises.
    // All synthetic; the gated real-KFM tick lives above.
    // =====================================================================

    // ---- AC2 (5.3 fix #1): the collapsed exit clause + invariant debug_assert -

    #[test]
    fn no_fire_pass_exits_without_tripping_invariant() {
        // A current state whose only controller never fires (trigger false) takes
        // the `self.state_no == current` exit path with zero transitions. The
        // collapsed clause + debug_assert must NOT trip (no counted transition is
        // required because state_no never moved). In a debug build the assert is
        // live, so this directly exercises the invariant on the no-transition path.
        let c = ctrl(0, "VelAdd", &[], &[(1, &["0"])], None, &[("x", "1")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::<f32>::ZERO;
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 0);
        assert_eq!(report.controllers_fired, 0);
        assert!(!report.transition_cap_hit);
        assert_eq!(ch.state_no, 0);
    }

    #[test]
    fn self_transition_exits_via_collapsed_clause_no_assert_trip() {
        // A ChangeState into the CURRENT state number counts a transition but leaves
        // state_no == current, so the loop exits via `if self.state_no == current`
        // BEFORE the debug_assert (which only guards the "moved to a different
        // state" fall-through). This pins that a self-transition does not loop and
        // does not trip the invariant in a debug build.
        let c = ctrl(0, "ChangeState", &[], &[(1, &["1"])], None, &[("value", "0")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        let report = lc.tick(&mut ch);
        // Exactly one self-transition; the cap is never hit (no looping).
        assert_eq!(report.transitions, 1);
        assert!(!report.transition_cap_hit);
        assert_eq!(ch.state_no, 0);
    }

    // ---- AC1/AC3: VarSet/VarAdd cross-type coercion into the target bank ----

    #[test]
    fn var_set_indexed_key_coerces_value_to_bank_type() {
        // Setting a FLOAT bank via an int-looking expression stores it as f32, and
        // setting an INT bank via a float-looking expression truncates to i32
        // (Value::to_int / to_float coercion at the bank boundary).
        let to_float = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("fvar(0)", "3")]);
        let to_int = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("var(0)", "1.9")]);
        let lc = loaded(vec![stand_n(0, vec![to_float, to_int])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert!((ch.fvars[0] - 3.0).abs() < 1e-6, "int expr widened into float bank");
        assert_eq!(ch.vars[0], 1, "float expr truncated into int bank");
    }

    #[test]
    fn var_set_first_indexed_key_wins_when_multiple_present() {
        // A VarSet sets exactly one variable. When several indexed keys are present
        // (malformed authoring), the implementation returns after the first match.
        // HashMap iteration order is unspecified, so assert the INVARIANT that holds
        // regardless of which key was chosen: exactly one of the two targets is set
        // (to its own value) and the other is untouched — never both, never a panic.
        let c = ctrl(
            0,
            "VarSet",
            &[],
            &[(1, &["1"])],
            None,
            &[("var(0)", "11"), ("var(1)", "22")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        let set0 = ch.vars[0] == 11 && ch.vars[1] == 0;
        let set1 = ch.vars[1] == 22 && ch.vars[0] == 0;
        assert!(set0 ^ set1, "exactly one indexed key wins; got vars={:?}", &ch.vars[0..2]);
    }

    #[test]
    fn var_add_v_value_form_targets_int_bank() {
        // VarAdd via the `v = i` + `value = expr` form accumulates in the int bank.
        let c = ctrl(0, "VarAdd", &[], &[(1, &["1"])], None, &[("v", "2"), ("value", "5")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vars[2] = 10;
        lc.tick(&mut ch);
        assert_eq!(ch.vars[2], 15, "v/value VarAdd accumulates in int bank");
    }

    #[test]
    fn var_add_fv_value_form_targets_float_bank() {
        let c = ctrl(0, "VarAdd", &[], &[(1, &["1"])], None, &[("fv", "1"), ("value", "0.5")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.fvars[1] = 2.0;
        lc.tick(&mut ch);
        assert!((ch.fvars[1] - 2.5).abs() < 1e-6, "fv/value VarAdd accumulates in float bank");
    }

    #[test]
    fn var_set_value_without_index_is_safe_noop() {
        // `value` present but neither an indexed key nor `v`/`fv`: nothing to target
        // → safe no-op (debug-logged), no panic, no slot written.
        let c = ctrl(0, "VarSet", &[], &[(1, &["1"])], None, &[("value", "99")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1, "dispatched");
        assert!(ch.vars.iter().all(|&v| v == 0), "no int slot written");
        assert!(ch.fvars.iter().all(|&v| v == 0.0), "no float slot written");
    }

    #[test]
    fn var_add_wraps_on_overflow_without_panic() {
        // VarAdd uses wrapping_add on the int bank, so adding past i32::MAX wraps
        // rather than panicking (the engine must never crash on adversarial state).
        let c = ctrl(0, "VarAdd", &[], &[(1, &["1"])], None, &[("var(0)", "1")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vars[0] = i32::MAX;
        lc.tick(&mut ch); // must not panic
        assert_eq!(ch.vars[0], i32::MIN, "i32::MAX + 1 wraps to i32::MIN");
    }

    // ---- AC1/AC3: VarRangeSet boundary and combined-bank semantics ----

    #[test]
    fn var_range_set_first_greater_than_last_writes_nothing() {
        // An inverted range (first > last) yields an empty inclusive loop: no slots
        // are written and nothing panics.
        let c = ctrl(
            0,
            "VarRangeSet",
            &[],
            &[(1, &["1"])],
            None,
            &[("value", "5"), ("first", "4"), ("last", "2")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert!(ch.vars.iter().all(|&v| v == 0), "inverted range writes nothing");
    }

    #[test]
    fn var_range_set_last_beyond_bank_is_clamped_safely() {
        // A `last` past the bank maximum must not panic: out-of-range indices are
        // skipped by assign_var, in-range ones are set.
        let c = ctrl(
            0,
            "VarRangeSet",
            &[],
            &[(1, &["1"])],
            None,
            &[("value", "3"), ("first", "58"), ("last", "100")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch); // must not panic despite last=100 > NUM_VARS-1
        assert_eq!(ch.vars[58], 3);
        assert_eq!(ch.vars[NUM_VARS - 1], 3, "top valid index set");
    }

    #[test]
    fn var_range_set_both_value_and_fvalue_set_both_banks() {
        // A single VarRangeSet carrying BOTH `value` and `fvalue` sets the int AND
        // float banks over the shared first/last range.
        let c = ctrl(
            0,
            "VarRangeSet",
            &[],
            &[(1, &["1"])],
            None,
            &[("value", "7"), ("fvalue", "1.5"), ("first", "0"), ("last", "1")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert_eq!(ch.vars[0], 7);
        assert_eq!(ch.vars[1], 7);
        assert!((ch.fvars[0] - 1.5).abs() < 1e-6);
        assert!((ch.fvars[1] - 1.5).abs() < 1e-6);
        assert_eq!(ch.vars[2], 0, "above range untouched (int)");
        assert!((ch.fvars[2] - 0.0).abs() < 1e-6, "above range untouched (float)");
    }

    // ---- AC1: StateTypeSet ignores a fully invalid token ----

    #[test]
    fn state_type_set_invalid_token_leaves_category_unchanged() {
        // An unrecognized statetype token (e.g. "Z") yields None from from_token, so
        // the category is left unchanged rather than reset or panicking.
        let c = ctrl(
            0,
            "StateTypeSet",
            &[],
            &[(1, &["1"])],
            None,
            &[("statetype", "Z"), ("physics", "?")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.state_type = StateType::Air;
        ch.physics = Physics::Air;
        lc.tick(&mut ch);
        assert_eq!(ch.state_type, StateType::Air, "invalid statetype token left unchanged");
        assert_eq!(ch.physics, Physics::Air, "invalid physics token left unchanged");
    }

    #[test]
    fn state_type_set_lowercase_token_is_accepted() {
        // Letter tokens are matched case-insensitively (from_token trims + ignores
        // case): a lowercase `c` sets crouching.
        let c = ctrl(0, "StateTypeSet", &[], &[(1, &["1"])], None, &[("statetype", "c")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.state_type = StateType::Standing;
        lc.tick(&mut ch);
        assert_eq!(ch.state_type, StateType::Crouching, "lowercase token accepted");
    }

    // ---- AC1: ChangeAnim elem param edge cases (zero / negative clamp) ----

    #[test]
    fn change_anim_elem_zero_and_negative_clamp_to_first_element() {
        // elem is one-based; saturating_sub(1).max(0) clamps `0` and negatives to
        // the first element (zero-based 0) rather than producing a negative index.
        for elem_src in ["0", "-5"] {
            let c = ctrl(0, "ChangeAnim", &[], &[(1, &["1"])], None, &[("value", "5"), ("elem", elem_src)]);
            let lc = loaded(vec![stand_n(0, vec![c])], {
                let mut air = tiny_air(0, &[5]);
                add_action(&mut air, 5, &[10, 10]);
                air
            });
            let mut ch = Character::new();
            ch.state_no = 0;
            ch.anim = 0;
            lc.tick(&mut ch);
            assert_eq!(ch.anim, 5);
            assert_eq!(ch.anim_elem, 0, "elem={elem_src} clamped to first element");
        }
    }

    // ---- AC1: Turn with default persistent flips every tick within one entry ---

    #[test]
    fn turn_default_persistent_flips_every_tick() {
        // With no persistent param (default 1), Turn flips facing on EVERY tick of
        // the same state entry: right -> left -> right over two ticks.
        let c = ctrl(0, "Turn", &[], &[(1, &["1"])], None, &[]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.facing = Facing::Right;
        lc.tick(&mut ch);
        assert_eq!(ch.facing, Facing::Left, "tick 1 flips right -> left");
        lc.tick(&mut ch);
        assert_eq!(ch.facing, Facing::Right, "tick 2 flips left -> right");
    }

    // ---- AC3: PosSet/PosAdd are independent of per-tick physics ----

    #[test]
    fn pos_controllers_are_not_disturbed_by_physics() {
        // The `physics` (friction) step acts on VELOCITY only — it never touches
        // position directly. `PosSet` writes the ABSOLUTE stage position. The
        // per-tick world integration then advances position by the
        // (post-friction, facing-relative) velocity. Facing right, the integrated
        // x delta is the friction-scaled velocity (no mirror), so:
        //   pos.x = 100 (PosSet) + 10 * stand_friction * (+1)
        let pset = ctrl(0, "PosSet", &[], &[(1, &["1"])], None, &[("x", "100"), ("y", "-20")]);
        let st = state(0, Entry { st: Some("S"), ph: Some("S"), anim: Some("0"), ..Entry::default() }, vec![pset]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::Stand;
        ch.facing = Facing::Right;
        ch.pos = Vec2::new(1.0, 1.0);
        ch.vel = Vec2::new(10.0, 0.0);
        lc.tick(&mut ch);
        let f = CharacterConstants::default().movement.stand_friction;
        // PosSet wrote the absolute x, then integration added the friction-scaled
        // velocity (facing right => no mirror). y has zero velocity, so PosSet's
        // -20 is intact.
        assert!(
            (ch.pos.x - (100.0 + 10.0 * f)).abs() < 1e-6,
            "PosSet (absolute) + facing-relative integration of friction-scaled vel; got {}",
            ch.pos.x
        );
        assert!((ch.pos.y - (-20.0)).abs() < 1e-6);
        // Velocity, by contrast, was scaled by stand friction this tick.
        assert!((ch.vel.x - 10.0 * f).abs() < 1e-6);
    }

    // ---- AC1: new controllers are also reachable from a special state ----

    #[test]
    fn pos_add_fires_from_special_state_minus2() {
        // The new controllers honor the special-state pass too: a PosAdd in [-2]
        // moves the entity before the current state's controllers run.
        let s_neg2 = ctrl(-2, "PosAdd", &[], &[(1, &["1"])], None, &[("x", "5")]);
        let lc = loaded(
            vec![stand_n(-2, vec![s_neg2]), stand_n(0, vec![])],
            tiny_air(0, &[5]),
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.pos = Vec2::<f32>::ZERO;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1, "special-state PosAdd fired");
        assert!((ch.pos.x - 5.0).abs() < 1e-6);
    }

    // ---- AC1: dispatch through the real CNS parser (lowercased keys/types) ----

    #[test]
    fn new_controllers_dispatch_from_real_cns_text() {
        // Parse a statedef whose controllers are the 5.4 set through the real CNS
        // parser (which lowercases keys), compile, and verify each applies. This
        // proves the dispatch works against parser output, not just hand-built
        // controllers with already-lowercased keys.
        let cns = CnsFile::from_str(
            "[Statedef 0]\ntype = S\nphysics = N\nanim = 0\n\
             [State 0, anim]\ntype = ChangeAnim\ntrigger1 = Time = 0\nvalue = 5\n\
             [State 0, pos]\ntype = PosAdd\ntrigger1 = Time = 0\nx = 3\ny = -2\n\
             [State 0, var]\ntype = VarSet\ntrigger1 = Time = 0\nvar(4) = 9\n\
             [State 0, turn]\ntype = Turn\ntrigger1 = Time = 0\npersistent = 0\n\
             [State 0, stype]\ntype = StateTypeSet\ntrigger1 = Time = 0\nmovetype = A\n",
        )
        .unwrap();
        let st = CompiledState::from_parsed(&cns.statedefs[0]);
        let lc = loaded(vec![st], {
            let mut air = tiny_air(0, &[5]);
            add_action(&mut air, 5, &[10, 10]);
            air
        });
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.facing = Facing::Right;
        ch.pos = Vec2::<f32>::ZERO;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 5, "all five 5.4 controllers fired");
        assert_eq!(ch.anim, 5, "ChangeAnim");
        assert!((ch.pos.x - 3.0).abs() < 1e-6, "PosAdd x");
        assert!((ch.pos.y - (-2.0)).abs() < 1e-6, "PosAdd y");
        assert_eq!(ch.vars[4], 9, "VarSet var(4)");
        assert_eq!(ch.facing, Facing::Left, "Turn");
        assert_eq!(ch.move_type, MoveType::Attack, "StateTypeSet movetype");
    }

    // ---- AC1: PlaySnd via real CNS text (the `value = g, i` pair form) ----

    #[test]
    fn play_snd_pair_value_from_cns_emits_request_without_mutating_state() {
        // The canonical PlaySnd form `value = group, index` parses through the CNS
        // parser; it must dispatch, emit one request, and leave all state untouched.
        let cns = CnsFile::from_str(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, snd]\ntype = PlaySnd\ntrigger1 = 1\nvalue = S1, 0\n",
        )
        .unwrap();
        let st = CompiledState::from_parsed(&cns.statedefs[0]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.life = 1000;
        let before_vars = ch.vars;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1, "PlaySnd dispatched");
        // `S1` → own .snd (common = false), group 1, sample 0, MUGEN defaults.
        assert_eq!(
            report.sound_requests,
            vec![SoundRequest {
                group: 1,
                sample: 0,
                channel: -1,
                volume_scale: 100,
                looping: false,
                common: false,
            }]
        );
        assert_eq!(ch.life, 1000, "PlaySnd mutates no character state");
        assert_eq!(ch.vars, before_vars);
    }

    // ---- Task 6.2: HitDef controller ---------------------------------------

    /// A synthetic `HitDef` controller builds the expected `active_hitdef`: a
    /// **string** param (`attr`) is parsed from the raw source, and a **numeric**
    /// param (`damage`) is evaluated against the attacker.
    #[test]
    fn hit_def_builds_active_hitdef_string_and_numeric() {
        let hitdef = ctrl(
            200,
            "HitDef",
            &[],
            &[(1, &["1"])],
            None,
            &[
                ("attr", "S, NA"),
                ("damage", "20, 5"),
                ("hitflag", "MAF"),
                ("guardflag", "MA"),
                ("ground.type", "Low"),
                ("ground.velocity", "-4, 0"),
                ("air.velocity", "-3, -6"),
                ("pausetime", "12, 12"),
                ("p2stateno", "5050"),
                ("fall", "1"),
                ("priority", "5, Miss"),
                ("sparkno", "S2"),
                ("hitsound", "5, 0"),
                ("guardsound", "S6, 1"),
            ],
        );
        let st = stand_n(200, vec![hitdef]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 200;
        ch.physics = Physics::None;

        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1, "HitDef dispatched");

        let hd = ch.active_hitdef.expect("HitDef must populate active_hitdef");
        // String param (attr) parsed from the raw source.
        assert_eq!(hd.attr, fp_combat::AttackAttr::parse("S, NA"));
        assert_eq!(hd.attr.class, fp_combat::StateClass::Standing);
        // Numeric param (damage) evaluated: hit=20, guard=5.
        assert_eq!(hd.damage.hit, 20);
        assert_eq!(hd.damage.guard, 5);
        // Other string/enum params.
        assert_eq!(hd.hitflag, fp_combat::HitFlags::parse("MAF"));
        assert_eq!(hd.guardflag, fp_combat::HitFlags::parse("MA"));
        assert_eq!(hd.ground_type, fp_combat::HitType::Low);
        // Other numeric params.
        assert!((hd.ground_velocity.x - (-4.0)).abs() < 1e-4);
        assert!((hd.air_velocity.y - (-6.0)).abs() < 1e-4);
        assert_eq!(hd.pausetime.p1, 12);
        assert_eq!(hd.pausetime.p2, 12);
        assert_eq!(hd.p2stateno, Some(5050));
        assert!(hd.fall);
        assert_eq!(hd.priority.value, 5);
        assert_eq!(hd.priority.kind, fp_combat::PriorityType::Miss);
        // `S`-prefixed sparkno: prefix stripped, numeric id kept.
        assert_eq!(hd.resources.sparkno, 2);
        // `hitsound = 5, 0` (no prefix) → the common/fight sound file, group 5.
        assert_eq!(
            hd.resources.hitsound,
            Some(fp_combat::SoundId { group: 5, sample: 0, common: true })
        );
        // `guardsound = S6, 1` → `S` prefix selects the character's own .snd.
        assert_eq!(
            hd.resources.guardsound,
            Some(fp_combat::SoundId { group: 6, sample: 1, common: false })
        );
    }

    /// Unspecified params fall back to `HitDef::default()`'s MUGEN sentinels.
    #[test]
    fn hit_def_unspecified_params_use_defaults() {
        let hitdef = ctrl(
            0,
            "HitDef",
            &[],
            &[(1, &["1"])],
            None,
            &[("attr", "C, HP")],
        );
        let st = stand_n(0, vec![hitdef]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;

        let _ = lc.tick(&mut ch);
        let hd = ch.active_hitdef.expect("active_hitdef populated");
        let def = fp_combat::HitDef::default();
        // Only attr was set; everything else equals the default.
        assert_eq!(hd.attr, fp_combat::AttackAttr::parse("C, HP"));
        assert_eq!(hd.damage, def.damage);
        assert_eq!(hd.hitflag, def.hitflag); // MAF sentinel
        assert_eq!(hd.hittimes, def.hittimes); // ground=0, air=20, guard=0
        assert_eq!(hd.priority, def.priority); // value 4, Hit
        assert_eq!(hd.chainid, def.chainid); // -1 sentinel
        assert_eq!(hd.p2stateno, None);
    }

    /// Numeric params are *evaluated*, not read literally: an expression that
    /// references the attacker's state (`var(1)`) resolves against `self`.
    #[test]
    fn hit_def_numeric_params_are_evaluated_against_self() {
        let hitdef = ctrl(
            0,
            "HitDef",
            &[],
            &[(1, &["1"])],
            None,
            // damage = var(1) * 2, var(1); ground.hittime = var(1) + 5
            &[
                ("attr", "S, NA"),
                ("damage", "var(1) * 2, var(1)"),
                ("ground.hittime", "var(1) + 5"),
            ],
        );
        let st = stand_n(0, vec![hitdef]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vars[1] = 30; // attacker state read by the expressions

        let _ = lc.tick(&mut ch);
        let hd = ch.active_hitdef.expect("active_hitdef populated");
        assert_eq!(hd.damage.hit, 60, "var(1)*2 evaluated against attacker");
        assert_eq!(hd.damage.guard, 30, "var(1) evaluated against attacker");
        assert_eq!(hd.hittimes.ground, 35, "var(1)+5 evaluated against attacker");
    }

    /// The full CNS authoring path: a `HitDef` block parsed by the real CNS
    /// parser then compiled and dispatched produces the expected active_hitdef.
    #[test]
    fn hit_def_from_real_cns_text() {
        let cns = CnsFile::from_str(
            "[Statedef 200]\ntype = S\nphysics = N\n\
             [State 200, hit]\ntype = HitDef\ntrigger1 = 1\n\
             attr = S, NA\ndamage = 23, 5\nground.type = Low\n\
             animtype = Light\nguardflag = MA\nhitflag = MAF\n\
             pausetime = 12, 12\nsparkno = 0\np2stateno = 5001\n",
        )
        .unwrap();
        let st = CompiledState::from_parsed(&cns.statedefs[0]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 200;
        ch.physics = Physics::None;

        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1);
        let hd = ch.active_hitdef.expect("active_hitdef from CNS HitDef");
        assert_eq!(hd.attr, fp_combat::AttackAttr::parse("S, NA"));
        assert_eq!(hd.damage.hit, 23);
        assert_eq!(hd.damage.guard, 5);
        assert_eq!(hd.ground_type, fp_combat::HitType::Low);
        assert_eq!(hd.p2stateno, Some(5001));
    }

    /// The HitDef controller never panics on malformed params: a bad attr falls
    /// back to the default, a non-numeric damage evaluates to 0, and the
    /// controller still populates `active_hitdef`.
    #[test]
    fn hit_def_malformed_params_never_panic() {
        let hitdef = ctrl(
            0,
            "HitDef",
            &[],
            &[(1, &["1"])],
            None,
            &[
                ("attr", "totally bogus"),
                ("damage", ","), // empty components -> 0, 0
                ("priority", "not a number, Frobnicate"),
            ],
        );
        let st = stand_n(0, vec![hitdef]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;

        let _ = lc.tick(&mut ch);
        let hd = ch.active_hitdef.expect("active_hitdef populated even on bad input");
        assert_eq!(hd.attr, fp_combat::AttackAttr::default(), "bad attr -> default");
        assert_eq!(hd.damage.hit, 0, "empty damage component -> 0");
        // Unrecognized priority type keeps the default kind.
        assert_eq!(hd.priority.kind, fp_combat::PriorityType::Hit);
    }

    // ---- AC4: gated real-KFM HitDef test (skips when test-assets absent) ----

    /// Ticks real KFM into a state that contains a `HitDef` controller and
    /// asserts `active_hitdef` becomes `Some` with a parsed `attr`. KFM's
    /// standing light punch is state 200, whose first controller is a HitDef.
    /// Skips cleanly when test-assets/ is absent.
    #[test]
    fn real_kfm_hit_def_populates_active_hitdef() {
        let def = test_asset("kfm/kfm.def");
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
        // Find a state that actually contains a HitDef controller (KFM's
        // attack states 200/210/... do). Skip gracefully if none is found.
        let Some((&state_no, _)) = lc.states.iter().find(|(_, s)| {
            s.controllers.iter().any(|c| {
                c.controller_type
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("HitDef"))
            })
        }) else {
            eprintln!("skipping: no HitDef-bearing state found in KFM");
            return;
        };

        let mut ch = Character::with_constants(lc.constants);
        ch.state_no = state_no;
        ch.anim = state_no;
        // Tick until the HitDef fires (its triggers may gate on AnimElem); cap
        // the number of ticks so a non-firing trigger can't hang the test.
        let mut fired = false;
        for _ in 0..120 {
            let _ = ch.tick(&lc, None, StageView::default());
            if ch.active_hitdef.is_some() {
                fired = true;
                break;
            }
        }
        if !fired {
            eprintln!(
                "skipping assertion: HitDef in state {state_no} did not fire within 120 ticks"
            );
            return;
        }
        let hd = ch
            .active_hitdef
            .expect("active_hitdef is Some after HitDef fired");
        // A parsed attr is present (KFM attacks are standing/crouch/air normals).
        assert!(matches!(
            hd.attr.class,
            fp_combat::StateClass::Standing
                | fp_combat::StateClass::Crouching
                | fp_combat::StateClass::Air
        ));
    }

    // =====================================================================
    // Proctor (task 6.2): additional HitDef-controller, GetHitVar, and
    // get-hit-state-readiness coverage layered on top of Forge's tests.
    // Each block is annotated with the acceptance criterion it exercises.
    // All synthetic except the gated real-KFM tests above.
    // =====================================================================

    /// Convenience: builds a `HitDef` controller (trigger1 = 1, no triggerall,
    /// default persistent) carrying the given params, dispatches it in state 0,
    /// and returns the resulting `active_hitdef` (panics in test only if the
    /// controller failed to populate it).
    fn build_hitdef(params: &[(&str, &str)]) -> fp_combat::HitDef {
        let hitdef = ctrl(0, "HitDef", &[], &[(1, &["1"])], None, params);
        let lc = loaded(vec![stand_n(0, vec![hitdef])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1, "HitDef must dispatch");
        ch.active_hitdef.expect("HitDef must populate active_hitdef")
    }

    // ---- AC1: every numeric param is evaluated and mapped --------------------

    #[test]
    fn hit_def_all_numeric_params_mapped() {
        // Cover the numeric params not exercised by Forge's happy-path test:
        // p1stateno, guard.velocity, guard.hittime, air.hittime, fall.yvelocity,
        // id, chainid, and the priority value-only form.
        let hd = build_hitdef(&[
            ("attr", "S, NA"),
            ("p1stateno", "1100"),
            ("p2stateno", "5000"),
            ("guard.velocity", "-6"),
            ("ground.hittime", "11"),
            ("air.hittime", "22"),
            ("guard.hittime", "9"),
            ("fall.yvelocity", "-4.5"),
            ("id", "7"),
            ("chainid", "3"),
            ("priority", "6"), // value only, no type token
        ]);
        assert_eq!(hd.p1stateno, Some(1100));
        assert_eq!(hd.p2stateno, Some(5000));
        assert!((hd.guard_velocity - (-6.0)).abs() < 1e-4);
        assert_eq!(hd.hittimes.ground, 11);
        assert_eq!(hd.hittimes.air, 22);
        assert_eq!(hd.hittimes.guard, 9);
        assert!((hd.fall_yvelocity - (-4.5)).abs() < 1e-4);
        assert_eq!(hd.id, 7);
        assert_eq!(hd.chainid, 3);
        assert_eq!(hd.priority.value, 6);
        // No type token after the value → the default kind (Hit) is preserved.
        assert_eq!(hd.priority.kind, fp_combat::PriorityType::Hit);
    }

    // ---- P7: animtype / air.animtype parsing --------------------------------

    #[test]
    fn hit_def_parses_animtype() {
        // Each authored spelling maps to the right AnimType, including BOTH `Med`
        // and `Medium`.
        let hard = build_hitdef(&[("attr", "S, NA"), ("animtype", "Hard")]);
        assert_eq!(hard.animtype, fp_combat::AnimType::Hard);
        let med = build_hitdef(&[("attr", "S, NA"), ("animtype", "Med")]);
        assert_eq!(med.animtype, fp_combat::AnimType::Medium);
        let medium = build_hitdef(&[("attr", "S, NA"), ("animtype", "Medium")]);
        assert_eq!(medium.animtype, fp_combat::AnimType::Medium);
        let up = build_hitdef(&[("attr", "S, NA"), ("animtype", "Up")]);
        assert_eq!(up.animtype, fp_combat::AnimType::Up);
        // Unknown -> Light (the default).
        let bad = build_hitdef(&[("attr", "S, NA"), ("animtype", "wat")]);
        assert_eq!(bad.animtype, fp_combat::AnimType::Light);
    }

    #[test]
    fn hit_def_air_animtype_defaults_to_ground_animtype_when_absent() {
        // No `air.animtype` key: MUGEN defaults it to the parsed `animtype`.
        let hd = build_hitdef(&[("attr", "S, NA"), ("animtype", "Hard")]);
        assert_eq!(hd.animtype, fp_combat::AnimType::Hard);
        assert_eq!(
            hd.air_animtype,
            fp_combat::AnimType::Hard,
            "absent air.animtype inherits the ground animtype"
        );
    }

    #[test]
    fn hit_def_explicit_air_animtype_overrides_ground() {
        // An explicit `air.animtype` overrides the inherited ground value, while
        // the ground `animtype` is untouched.
        let hd = build_hitdef(&[
            ("attr", "S, NA"),
            ("animtype", "Light"),
            ("air.animtype", "Up"),
        ]);
        assert_eq!(hd.animtype, fp_combat::AnimType::Light, "ground stays Light");
        assert_eq!(hd.air_animtype, fp_combat::AnimType::Up, "air overridden to Up");
    }

    #[test]
    fn hit_def_no_animtype_keys_leave_both_light() {
        // Neither key present: both default to Light (the HitDef::default value).
        let hd = build_hitdef(&[("attr", "S, NA")]);
        assert_eq!(hd.animtype, fp_combat::AnimType::Light);
        assert_eq!(hd.air_animtype, fp_combat::AnimType::Light);
    }

    // ---- AC1: velocity single-component fallback keeps the default axis -------

    #[test]
    fn hit_def_velocity_single_component_keeps_default_y() {
        // `ground.velocity = -4` (x only) must leave y at the default's y (0.0)
        // via pair_to_vec2's per-axis fallback, not zero it spuriously or panic.
        let hd = build_hitdef(&[("attr", "S, NA"), ("ground.velocity", "-4")]);
        assert!((hd.ground_velocity.x - (-4.0)).abs() < 1e-4);
        assert!(
            (hd.ground_velocity.y - fp_combat::HitDef::default().ground_velocity.y).abs() < 1e-4,
            "missing y component falls back to the default y"
        );
    }

    // ---- AC1: guardflag empty = unblockable ----------------------------------

    #[test]
    fn hit_def_empty_guardflag_is_unblockable() {
        // An explicitly-empty guardflag must parse to the empty (unblockable) set,
        // overriding HitDef::default()'s (also-empty) guardflag — and crucially it
        // must NOT inherit the hitflag's MAF default.
        let hd = build_hitdef(&[("attr", "S, NA"), ("guardflag", "")]);
        assert!(hd.guardflag.is_empty(), "empty guardflag = unblockable");
    }

    // ---- AC1: fall = 0 yields false ------------------------------------------

    #[test]
    fn hit_def_fall_zero_is_false() {
        let hd = build_hitdef(&[("attr", "S, NA"), ("fall", "0")]);
        assert!(!hd.fall, "fall = 0 must be false");
        // And an expression that evaluates to nonzero is true.
        let hd2 = build_hitdef(&[("attr", "S, NA"), ("fall", "2 - 1")]);
        assert!(hd2.fall, "fall = (2-1) evaluates truthy");
    }

    // ---- AC1: MUGEN single-active-HitDef — a later HitDef overwrites ----------

    #[test]
    fn hit_def_later_controller_overwrites_earlier() {
        // Two HitDef controllers fire in one tick; MUGEN keeps a single active
        // HitDef, so the SECOND one must win (overwrite the first).
        let first = ctrl(0, "HitDef", &[], &[(1, &["1"])], None, &[("attr", "S, NA"), ("damage", "10, 0")]);
        let second = ctrl(0, "HitDef", &[], &[(1, &["1"])], None, &[("attr", "C, HP"), ("damage", "99, 1")]);
        let lc = loaded(vec![stand_n(0, vec![first, second])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 2, "both HitDefs dispatch");
        let hd = ch.active_hitdef.expect("active_hitdef populated");
        assert_eq!(hd.attr, fp_combat::AttackAttr::parse("C, HP"), "second HitDef wins");
        assert_eq!(hd.damage.hit, 99, "second HitDef's damage wins");
    }

    // ---- AC1: a gated HitDef that does not qualify leaves active_hitdef None --

    #[test]
    fn hit_def_not_firing_leaves_active_hitdef_none() {
        // The HitDef's only trigger group is false → it never dispatches, so
        // active_hitdef stays at its initial None (no spurious population).
        let hitdef = ctrl(0, "HitDef", &[], &[(1, &["0"])], None, &[("attr", "S, NA")]);
        let lc = loaded(vec![stand_n(0, vec![hitdef])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 0, "gated-off HitDef does not fire");
        assert!(ch.active_hitdef.is_none(), "no fire → active_hitdef stays None");
    }

    // ---- AC1: a HitDef with NO params still builds a default-valued HitDef ----

    #[test]
    fn hit_def_no_params_is_full_default() {
        // A bare `type = HitDef` (no params at all) must still populate
        // active_hitdef with exactly HitDef::default() — the MUGEN sentinels.
        let hd = build_hitdef(&[]);
        assert_eq!(hd, fp_combat::HitDef::default());
        // Spot-check the two non-zero sentinels survive.
        assert_eq!(hd.hitflag, fp_combat::HitFlags::parse("MAF"));
        assert_eq!(hd.chainid, -1);
        assert_eq!(hd.hittimes.air, 20);
    }

    // ---- AC1: raw_param tolerates a mixed-case key (case-insensitive lookup) --

    #[test]
    fn raw_param_is_case_insensitive_fallback() {
        // The loader lowercases keys, but raw_param's scan fallback must still
        // find a stray mixed-case key without panicking. Build the controller's
        // params map directly with a non-lowercased key.
        let c = CompiledController {
            state_number: 0,
            label: String::new(),
            controller_type: Some("HitDef".to_string()),
            triggerall: vec![],
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params: [
                ("AtTr".to_string(), CompiledParam::compile("C, HP")),
                ("Ground.Type".to_string(), CompiledParam::compile("Low")),
            ]
            .into_iter()
            .collect(),
        };
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let _ = lc.tick(&mut ch);
        let hd = ch.active_hitdef.expect("active_hitdef populated");
        assert_eq!(hd.attr, fp_combat::AttackAttr::parse("C, HP"), "mixed-case attr key found");
        assert_eq!(hd.ground_type, fp_combat::HitType::Low, "mixed-case ground.type key found");
    }

    // ---- helper-fn unit coverage: parse_resource_id --------------------------

    #[test]
    fn parse_resource_id_handles_prefix_and_garbage() {
        // Plain numeric id.
        assert_eq!(parse_resource_id("3", -1), 3);
        // Upper- and lower-case `S` prefix stripped.
        assert_eq!(parse_resource_id("S2", -1), 2);
        assert_eq!(parse_resource_id("s7", -1), 7);
        // Only the first comma-separated component is read.
        assert_eq!(parse_resource_id("S5, 0", -1), 5);
        // Non-numeric → fallback preserved (the field's current default).
        assert_eq!(parse_resource_id("nope", -1), -1);
        assert_eq!(parse_resource_id("", 42), 42);
        // A bare `S` with no digits → fallback.
        assert_eq!(parse_resource_id("S", -1), -1);
    }

    // ---- helper-fn unit coverage: parse_hit_type -----------------------------

    #[test]
    fn parse_hit_type_all_tokens_and_default() {
        assert_eq!(parse_hit_type("High"), fp_combat::HitType::High);
        assert_eq!(parse_hit_type("low"), fp_combat::HitType::Low);
        assert_eq!(parse_hit_type("  Trip "), fp_combat::HitType::Trip);
        assert_eq!(parse_hit_type("None"), fp_combat::HitType::None);
        // Unrecognized → MUGEN's High default.
        assert_eq!(parse_hit_type("sideways"), fp_combat::HitType::High);
    }

    // ---- helper-fn unit coverage: parse_priority_type ------------------------

    #[test]
    fn parse_priority_type_reads_second_token() {
        // The type is the SECOND comma-separated token of the priority value.
        assert_eq!(parse_priority_type("5, Hit"), Some(fp_combat::PriorityType::Hit));
        assert_eq!(parse_priority_type("5, Miss"), Some(fp_combat::PriorityType::Miss));
        assert_eq!(parse_priority_type("5, dodge"), Some(fp_combat::PriorityType::Dodge));
        // No second token → None (keep the default kind).
        assert_eq!(parse_priority_type("5"), None);
        // Empty second token → None.
        assert_eq!(parse_priority_type("5, "), None);
        // Unrecognized second token → None.
        assert_eq!(parse_priority_type("5, Frobnicate"), None);
    }

    // ---- helper-fn unit coverage: pair_to_vec2 -------------------------------

    #[test]
    fn pair_to_vec2_uses_default_per_missing_axis() {
        let dflt = Vec2::new(1.0, 2.0);
        // Both present → both used.
        assert_eq!(pair_to_vec2(&[Value::Float(3.0), Value::Float(4.0)], dflt), Vec2::new(3.0, 4.0));
        // Only x present → y falls back to default.y.
        assert_eq!(pair_to_vec2(&[Value::Float(3.0)], dflt), Vec2::new(3.0, 2.0));
        // Empty → both default.
        assert_eq!(pair_to_vec2(&[], dflt), dflt);
    }

    // ======================================================================
    // Proctor (6.2b): scalar 5.4/6.2 controllers read component 0 via the
    // accessor and ignore any stray extra components; multi-component
    // controllers read the right index. Each builds the param through the real
    // CnsFile parser so the loader's top-level-comma split is exercised.
    // ======================================================================

    /// Builds a synthetic graph from a single CNS source so the loader's
    /// param-splitting path (not the test `ctrl` helper) is what produces the
    /// CompiledParam component lists. Returns the Synth + the entry state number.
    fn synth_from_cns(src: &str) -> Synth {
        let cns = CnsFile::from_str(src).expect("cns source parses");
        let states: Vec<CompiledState> = cns
            .statedefs
            .iter()
            .map(CompiledState::from_parsed)
            .collect();
        loaded(states, tiny_air(0, &[5]))
    }

    #[test]
    fn changestate_value_reads_component_zero_through_loader_split() {
        // AC3: ChangeState's `value` is scalar — read via component 0. Even if an
        // author appended a stray second value, only component 0 is consumed.
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, go]\ntype = ChangeState\ntrigger1 = 1\nvalue = 42, 99\n\
             [Statedef 42]\ntype = S\nphysics = N\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.transitions, 1);
        assert_eq!(ch.state_no, 42, "ChangeState read component 0 (42), not 99");
    }

    #[test]
    fn velset_x_y_are_independent_scalar_params_each_component_zero() {
        // AC3/AC4: VelSet uses two SEPARATE scalar params `x` and `y`, each read
        // via component 0. A comma INSIDE one of them must not bleed across axes.
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, v]\ntype = VelSet\ntrigger1 = 1\nx = -4\ny = 0\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(9.0, 9.0);
        lc.tick(&mut ch);
        assert!((ch.vel.x - (-4.0)).abs() < 1e-6, "x ← component 0 of `x`");
        assert!((ch.vel.y - 0.0).abs() < 1e-6, "y ← component 0 of `y`");
    }

    #[test]
    fn varset_indexed_key_reads_component_zero_only() {
        // AC3: VarSet `var(2) = expr` is scalar. If an author writes a stray
        // second value, only component 0 assigns; the bank gets exactly one value.
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, set]\ntype = VarSet\ntrigger1 = 1\nvar(2) = 7, 123\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert_eq!(ch.vars[2], 7, "VarSet assigned component 0 (7), not 123");
    }

    #[test]
    fn hitdef_ground_velocity_reads_x_then_y_components() {
        // AC4: a multi-component param read by index. `ground.velocity = -4, -3`
        // sets x from component 0 and y from component 1 (distinct values prove
        // the index, not a single shared component).
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, h]\ntype = HitDef\ntrigger1 = 1\n\
             attr = S, NA\nground.velocity = -4, -3\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        let hd = ch.active_hitdef.expect("active_hitdef");
        assert!((hd.ground_velocity.x - (-4.0)).abs() < 1e-6, "x ← component 0");
        assert!((hd.ground_velocity.y - (-3.0)).abs() < 1e-6, "y ← component 1");
    }

    #[test]
    fn hitdef_pausetime_p1_p2_are_distinct_components() {
        // AC4: pausetime p1 (component 0) and p2 (component 1) are read
        // independently — distinct values guard against reading the same index.
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, h]\ntype = HitDef\ntrigger1 = 1\n\
             attr = S, NA\npausetime = 12, 8\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        let hd = ch.active_hitdef.expect("active_hitdef");
        assert_eq!(hd.pausetime.p1, 12, "p1 ← component 0");
        assert_eq!(hd.pausetime.p2, 8, "p2 ← component 1");
    }

    #[test]
    fn hitdef_damage_components_are_per_component_expressions_vs_self() {
        // AC4 + MUGEN-semantics: each component is its OWN compiled expression,
        // evaluated against the attacker. `damage = var(1)*2, var(1)+1` with
        // var(1)=10 → hit=20, guard=11 (component 1 is NOT a copy of component 0).
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, h]\ntype = HitDef\ntrigger1 = 1\n\
             attr = S, NA\ndamage = var(1) * 2, var(1) + 1\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vars[1] = 10;
        lc.tick(&mut ch);
        let hd = ch.active_hitdef.expect("active_hitdef");
        assert_eq!(hd.damage.hit, 20, "component 0 = var(1)*2");
        assert_eq!(hd.damage.guard, 11, "component 1 = var(1)+1");
    }

    #[test]
    fn hitdef_priority_value_is_expr_component_zero_type_from_raw() {
        // AC4: `priority = value [, type]` — component 0 is the numeric value
        // (evaluated), while the type token is parsed from the RAW source (the
        // second component is an identifier, not arithmetic).
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, h]\ntype = HitDef\ntrigger1 = 1\n\
             attr = S, NA\npriority = 5, Miss\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        let hd = ch.active_hitdef.expect("active_hitdef");
        assert_eq!(hd.priority.value, 5, "priority value ← component 0");
        // `Miss` is NOT the default (`Hit`), so this proves the raw-token read.
        assert_eq!(hd.priority.kind, fp_combat::PriorityType::Miss, "type ← raw token");
    }

    // ---- Audit P9: NotHitBy / HitBy controller dispatch -------------------

    #[test]
    fn nothitby_controller_sets_slot1_attrs_mode_and_time() {
        // `[State] type=NotHitBy / value=SCA / time=5` arms slot 1 as an exclude
        // window covering all classes for 5 ticks.
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, nhb]\ntype = NotHitBy\ntrigger1 = 1\nvalue = SCA\ntime = 5\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        // Slot 1 active for `time` ticks (the top-of-tick decrement was a no-op
        // from 0, then the controller set it to 5).
        assert_eq!(ch.invuln.slot1.mode, crate::invuln::InvulnMode::NotHitBy);
        assert_eq!(ch.invuln.slot1.time_remaining, 5, "time -> slot1 remaining");
        assert!(ch.invuln.slot1.is_active());
        // Slot 2 was untouched (no `value2`).
        assert!(!ch.invuln.slot2.is_active(), "value2 absent -> slot2 inactive");
        // The SCA set covers a standing normal attack.
        let attr = fp_combat::AttackAttr::parse("S, NA");
        assert!(ch.invuln.slot1.blocks(&attr), "SCA NotHitBy blocks S,NA");
    }

    #[test]
    fn nothitby_value2_arms_slot2_and_value_arms_slot1() {
        // KFM's get-up shape: slot1 from `value`, slot2 from `value2`.
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, a]\ntype = NotHitBy\ntrigger1 = 1\nvalue = , NT,ST,HT\ntime = 12\n\
             [State 0, b]\ntype = NotHitBy\ntrigger1 = 1\nvalue2 = SCA\ntime = 3\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 12, "value -> slot1");
        assert_eq!(ch.invuln.slot2.time_remaining, 3, "value2 -> slot2");
        // Slot1 = throws only; slot2 = all.
        let throw = fp_combat::AttackAttr::parse("S, NT");
        let punch = fp_combat::AttackAttr::parse("S, NA");
        assert!(ch.invuln.slot1.blocks(&throw), "slot1 blocks throws");
        assert!(!ch.invuln.slot1.blocks(&punch), "slot1 allows punches");
        assert!(ch.invuln.slot2.blocks(&punch), "slot2 (SCA) blocks punches");
    }

    #[test]
    fn nothitby_time_defaults_to_one_when_absent() {
        // MUGEN default `time = 1` — the common per-frame `value = SCA` form.
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, nhb]\ntype = NotHitBy\ntrigger1 = 1\nvalue = SCA\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 1, "absent time defaults to 1");
    }

    #[test]
    fn hitby_controller_sets_include_mode() {
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, hby]\ntype = HitBy\ntrigger1 = 1\nvalue = , NT,ST,HT\ntime = 8\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.mode, crate::invuln::InvulnMode::HitBy);
        // HitBy admitting only throws blocks a normal attack.
        let punch = fp_combat::AttackAttr::parse("S, NA");
        let throw = fp_combat::AttackAttr::parse("S, NT");
        assert!(ch.invuln.slot1.blocks(&punch), "HitBy(throws) blocks a punch");
        assert!(!ch.invuln.slot1.blocks(&throw), "HitBy(throws) admits a throw");
    }

    #[test]
    fn nothitby_window_decrements_each_tick_and_expires() {
        // A NotHitBy fired once (persistent gating off after the first qualifying
        // tick via a one-shot trigger) so the slot is NOT re-armed each tick and
        // we can watch it count down. We fire it on tick 0 only (`time = 0`).
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, nhb]\ntype = NotHitBy\ntrigger1 = time = 0\nvalue = SCA\ntime = 3\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;

        // Tick 0: controller fires (Time = 0), slot set to 3.
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 3, "armed on tick 0");
        // Tick 1: Time != 0, controller does not fire; slot decrements to 2.
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 2);
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 1);
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 0, "expired");
        assert!(!ch.invuln.slot1.is_active());
        // Further ticks keep it at 0 (saturating).
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 0);
    }

    #[test]
    fn nothitby_ignorehitpause_counts_during_pause_others_freeze() {
        // A NotHitBy with `ignorehitpause = 1` keeps counting during a hit-pause
        // freeze; one without it is frozen. Arm both on tick 0, then enter a
        // hit-pause and tick.
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, frozen]\ntype = NotHitBy\ntrigger1 = time = 0\nvalue = SCA\ntime = 9\n\
             [State 0, live]\ntype = NotHitBy\ntrigger1 = time = 0\nvalue2 = SCA\ntime = 9\n\
             ignorehitpause = 1\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;

        // Tick 0: both slots armed to 9.
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 9);
        assert_eq!(ch.invuln.slot2.time_remaining, 9);
        assert!(!ch.invuln.slot1.ignore_hitpause, "slot1 has no ignorehitpause");
        assert!(ch.invuln.slot2.ignore_hitpause, "slot2 carries ignorehitpause");

        // Enter a hit-pause and tick: slot1 frozen, slot2 (ignorehitpause) counts.
        ch.hitpause = 4;
        let report = lc.tick(&mut ch);
        assert!(report.hitpaused, "this tick was a hit-pause freeze");
        assert_eq!(ch.invuln.slot1.time_remaining, 9, "frozen slot held during pause");
        assert_eq!(ch.invuln.slot2.time_remaining, 8, "ignorehitpause slot counted");
    }

    // ---- Proctor (Audit P9): controller-dispatch edge cases ---------------
    // Forge's tests above cover the happy path (value/value2 -> slot1/slot2,
    // default time, HitBy mode, decrement/expiry, ignorehitpause). These pin the
    // ctrl_invuln edges the doc-comment promises but that no test exercised:
    // time<=0 -> inactive slot, re-arm of a live slot, an absent slot left
    // untouched, and a `*` wildcard armed through the real loader split.

    /// P9 (Proctor): `time = 0` arms an immediately-INACTIVE slot (blocks
    /// nothing), per the doc-comment "a `time` of `0` or less sets an
    /// immediately-inactive slot". The mask must not accidentally grant invuln.
    #[test]
    fn nothitby_time_zero_arms_inactive_slot() {
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, nhb]\ntype = NotHitBy\ntrigger1 = 1\nvalue = SCA\ntime = 0\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        // The slot was written (mode/attrs set) but is inactive at time 0.
        assert_eq!(ch.invuln.slot1.mode, crate::invuln::InvulnMode::NotHitBy);
        assert_eq!(ch.invuln.slot1.time_remaining, 0);
        assert!(!ch.invuln.slot1.is_active(), "time=0 slot is inactive");
        let attr = fp_combat::AttackAttr::parse("S, NA");
        assert!(!ch.invuln.slot1.blocks(&attr), "an inactive slot blocks nothing");
    }

    /// P9 (Proctor): a NEGATIVE `time` also yields an inactive slot (the
    /// `time_remaining > 0` activeness rule, never a panic on the negative).
    #[test]
    fn nothitby_negative_time_arms_inactive_slot() {
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, nhb]\ntype = NotHitBy\ntrigger1 = 1\nvalue = SCA\ntime = -5\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, -5);
        assert!(!ch.invuln.slot1.is_active(), "negative time is inactive");
    }

    /// P9 (Proctor): re-arming a STILL-ACTIVE slot each time the controller fires
    /// resets its time (MUGEN re-arms on every fire). A persistent (default)
    /// NotHitBy refreshes the window so it never counts down toward expiry while
    /// the controller keeps firing.
    #[test]
    fn nothitby_rearms_a_live_slot_each_tick() {
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, nhb]\ntype = NotHitBy\ntrigger1 = 1\nvalue = SCA\ntime = 3\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;

        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 3, "armed to 3 on the first fire");
        // Next tick: top-of-tick decrement to 2, then the controller re-fires and
        // resets it back to 3 — so a continuously-firing NotHitBy stays at `time`.
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 3, "re-armed back to 3, not decayed");
        lc.tick(&mut ch);
        assert_eq!(ch.invuln.slot1.time_remaining, 3, "still re-armed while firing");
        assert!(ch.invuln.slot1.is_active());
    }

    /// P9 (Proctor): a controller that sets ONLY `value2` leaves slot 1 fully
    /// untouched — it does not clear or overwrite an independently-armed slot 1.
    /// (The doc: "an absent `value` simply leaves slot 1 untouched".) Here slot 1
    /// is pre-armed by hand and a `value2`-only controller fires; slot 1 survives
    /// unchanged including its mode.
    #[test]
    fn value2_only_controller_preserves_slot1() {
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, b]\ntype = NotHitBy\ntrigger1 = 1\nvalue2 = SCA\ntime = 4\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        // Pre-arm slot 1 by hand to a DISTINCT mode/attr/time so we can prove it
        // is preserved. (time 7 so the top-of-tick decrement to 6 still leaves it
        // active and clearly not overwritten by the controller's time=4.)
        ch.invuln.slot1 = crate::invuln::InvulnSlot {
            attrs: crate::invuln::AttackAttrSet::parse(", NT,ST,HT"),
            mode: crate::invuln::InvulnMode::HitBy,
            time_remaining: 7,
            ignore_hitpause: false,
        };

        lc.tick(&mut ch);
        // Slot 1 untouched by the value2-only controller (only the per-tick
        // decrement applied: 7 -> 6); mode/attrs intact.
        assert_eq!(ch.invuln.slot1.mode, crate::invuln::InvulnMode::HitBy, "slot1 mode preserved");
        assert_eq!(ch.invuln.slot1.time_remaining, 6, "slot1 only decremented, not re-armed");
        // Slot 2 was the one the controller armed.
        assert_eq!(ch.invuln.slot2.mode, crate::invuln::InvulnMode::NotHitBy);
        assert_eq!(ch.invuln.slot2.time_remaining, 4, "value2 armed slot2");
    }

    /// P9 (Proctor): a `value = *` wildcard armed through the real loader split
    /// blocks EVERY attacker attr while active (NotHitBy `*` = blanket invuln).
    #[test]
    fn nothitby_wildcard_value_blocks_every_attr() {
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, nhb]\ntype = NotHitBy\ntrigger1 = 1\nvalue = *\ntime = 5\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert!(ch.invuln.slot1.attrs.any, "`*` parsed as the wildcard set");
        for s in ["S, NA", "C, HP", "A, ST", "S, SP"] {
            let attr = fp_combat::AttackAttr::parse(s);
            assert!(ch.invuln.slot1.blocks(&attr), "wildcard NotHitBy blocks {s:?}");
        }
    }

    /// P9 (Proctor): firing NotHitBy then HitBy in the same tick leaves slot 1 in
    /// the LAST controller's mode (later controllers overwrite the slot). The two
    /// dispatch arms share `ctrl_invuln`; this proves the mode argument is wired
    /// per-arm and the second write wins.
    #[test]
    fn later_controller_overwrites_slot_mode() {
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, a]\ntype = NotHitBy\ntrigger1 = 1\nvalue = SCA\ntime = 5\n\
             [State 0, b]\ntype = HitBy\ntrigger1 = 1\nvalue = SCA\ntime = 5\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        lc.tick(&mut ch);
        assert_eq!(
            ch.invuln.slot1.mode,
            crate::invuln::InvulnMode::HitBy,
            "the later HitBy controller overwrote slot1's mode"
        );
    }

    #[test]
    fn statetypeset_token_read_from_raw_not_compiled_component() {
        // AC3: StateTypeSet reads bare letter tokens from raw(), not via the
        // compiled component (a bare `C` parses as an Ident, but the controller
        // intentionally uses raw()). Confirm the override applies.
        let lc = synth_from_cns(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, t]\ntype = StateTypeSet\ntrigger1 = 1\nstatetype = C\nphysics = C\n",
        );
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.state_type = StateType::Standing;
        lc.tick(&mut ch);
        assert_eq!(ch.state_type, StateType::Crouching, "statetype overridden to C");
        assert_eq!(ch.physics, Physics::Crouch, "physics overridden to C");
    }

    // ---- 6.2b: component accessor reads the loader-split components ------------

    #[test]
    fn eval_param_components_evaluates_each_loader_split_component() {
        // The loader splits a param on top-level commas into a component list;
        // `eval_param_components` evaluates each pre-compiled component against
        // self (no re-splitting). The old raw-source re-split is gone.
        let mut ch = Character::new();
        ch.vars[2] = 8;
        // `var(2) * 2, var(2), ` → [16, 8, 0] (trailing empty component → 0).
        let comps = ch.eval_param_components(
            &CompiledParam::compile("var(2) * 2, var(2), "),
            EvalEnv::self_only(),
        );
        assert_eq!(comps.len(), 3);
        assert_eq!(comps[0].to_int(), 16);
        assert_eq!(comps[1].to_int(), 8);
        assert_eq!(comps[2].to_int(), 0, "empty trailing component → 0");
        // A single component yields a one-element vec.
        let one = ch.eval_param_components(&CompiledParam::compile("42"), EvalEnv::self_only());
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].to_int(), 42);
    }

    #[test]
    fn eval_param_component_reads_index_with_none_when_absent() {
        // The scalar/component accessor: index 0 is the scalar value; a missing
        // component returns None so callers can substitute their own default.
        let ch = Character::new();
        let p = CompiledParam::compile("-4, 0");
        let env = EvalEnv::self_only();
        assert_eq!(ch.eval_param_component(&p, 0, env).map(|v| v.to_int()), Some(-4));
        assert_eq!(ch.eval_param_component(&p, 1, env).map(|v| v.to_int()), Some(0));
        assert!(ch.eval_param_component(&p, 2, env).is_none(), "no third component");
        // eval_param is shorthand for component 0.
        assert_eq!(ch.eval_param(&p, env).map(|v| v.to_int()), Some(-4));
    }

    // ---- AC3: get-hit-state readiness — a synthetic 5000-range state runs -----

    #[test]
    fn get_hit_state_reads_gethitvar_and_dispatches() {
        // Part C readiness: a get-hit state (5000-range) that gates a ChangeState
        // on a GetHitVar member must (a) resolve the GetHitVar read against the
        // character's get_hit_vars, and (b) dispatch the ChangeState — proving the
        // common get-hit states are runnable through the executor today.
        //
        // State 5000: ChangeState to 5001 when GetHitVar(fall) != 0.
        let go = ctrl(
            5000,
            "ChangeState",
            &[],
            &[(1, &["GetHitVar(fall) != 0"])],
            None,
            &[("value", "5001")],
        );
        let lc = loaded(
            vec![stand_n(5000, vec![go]), stand_n(5001, vec![])],
            tiny_air(0, &[5]),
        );

        // With a default get_hit_vars (fall = 0), the trigger is false → no move.
        let mut idle = Character::new();
        idle.state_no = 5000;
        idle.physics = Physics::None;
        assert_eq!(lc.tick(&mut idle).transitions, 0, "fall=0 → stays put");
        assert_eq!(idle.state_no, 5000);

        // Populate get_hit_vars as hit resolution (task 6.3) eventually will; the
        // get-hit state now reads it and transitions.
        let mut hit = Character::new();
        hit.state_no = 5000;
        hit.physics = Physics::None;
        hit.get_hit_vars = crate::GetHitVars { fall: 1, ..crate::GetHitVars::default() };
        assert_eq!(lc.tick(&mut hit).transitions, 1, "fall=1 → get-hit state advances");
        assert_eq!(hit.state_no, 5001);
    }

    #[test]
    fn get_hit_state_velset_from_gethitvar_velocity() {
        // A get-hit state commonly applies the imparted knockback via
        // `VelSet x = GetHitVar(xvel)`. Confirm the executor evaluates the
        // GetHitVar redirection inside a controller parameter expression.
        let vset = ctrl(
            5000,
            "VelSet",
            &[],
            &[(1, &["1"])],
            None,
            &[("x", "GetHitVar(xvel)"), ("y", "GetHitVar(yvel)")],
        );
        let lc = loaded(vec![stand_n(5000, vec![vset])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 5000;
        ch.physics = Physics::None;
        ch.get_hit_vars = crate::GetHitVars {
            xvel: -5.5,
            yvel: -3.0,
            ..crate::GetHitVars::default()
        };
        lc.tick(&mut ch);
        assert!((ch.vel.x - (-5.5)).abs() < 1e-4, "VelSet x from GetHitVar(xvel)");
        assert!((ch.vel.y - (-3.0)).abs() < 1e-4, "VelSet y from GetHitVar(yvel)");
    }

    // ---- AC1: HitDef does NOT require ctrl / works in any move type ----------

    #[test]
    fn hit_def_fires_regardless_of_move_type() {
        // A HitDef is an offensive controller; it must build active_hitdef even if
        // the attacker is mid-attack (move_type Attack) — gating is purely by the
        // trigger, not by move_type. (Smoke test that nothing in dispatch gates on
        // move_type.)
        let hitdef = ctrl(0, "HitDef", &[], &[(1, &["1"])], None, &[("attr", "A, SP")]);
        let lc = loaded(vec![stand_n(0, vec![hitdef])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.move_type = MoveType::Attack;
        let _ = lc.tick(&mut ch);
        let hd = ch.active_hitdef.expect("active_hitdef populated mid-attack");
        assert_eq!(hd.attr.class, fp_combat::StateClass::Air);
        assert_eq!(hd.attr.power, fp_combat::AttackPower::Special);
        assert_eq!(hd.attr.kind, fp_combat::AttackKind::Projectile);
    }

    // =====================================================================
    // Proctor (task 8.3a): additional edge-case, error-path, and
    // MUGEN-semantics coverage for the `PlaySnd` -> `SoundRequest` emitter,
    // layered on top of Forge's tests. Every test is annotated with the
    // acceptance criterion (AC1..AC5) it exercises. All synthetic except the
    // gated real-KFM fixture test at the end.
    // =====================================================================

    // ---- AC1: SoundRequest struct shape, derives, and field semantics ------

    /// AC1: `SoundRequest` derives `Debug`, `Clone`, and `PartialEq`, and the
    /// fields round-trip through a clone. Pins the public contract so a later
    /// refactor that drops a derive (which downstream `fp-audio` relies on) is
    /// caught here.
    #[test]
    fn sound_request_is_debug_clone_partial_eq() {
        let req = SoundRequest {
            group: 5,
            sample: 2,
            channel: 3,
            volume_scale: 80,
            looping: true,
            common: true,
        };
        // Clone + PartialEq.
        let copy = req.clone();
        assert_eq!(req, copy);
        // Debug renders every field (used in tracing / test failure messages).
        let dbg = format!("{req:?}");
        for needle in [
            "group", "sample", "channel", "volume_scale", "looping", "common",
        ] {
            assert!(dbg.contains(needle), "Debug output missing field {needle:?}");
        }
        // Distinct field values compare unequal (PartialEq is structural).
        let other = SoundRequest {
            group: 6,
            ..req
        };
        assert_ne!(req, other);
    }

    // ---- AC1/AC2: TickReport.sound_requests is fresh per tick --------------

    /// AC1: `TickReport` is built fresh each tick, so `sound_requests` never
    /// carries a request from a previous tick into a later one. A PlaySnd that
    /// fires on tick 1 (Time = 0) but not tick 2 must leave tick 2's report
    /// empty.
    #[test]
    fn sound_requests_do_not_leak_across_ticks() {
        // Fire PlaySnd only on the entry tick (Time = 0), persistent default.
        let c = ctrl(
            0,
            "PlaySnd",
            &[],
            &[(1, &["Time = 0"])],
            None,
            &[("value", "1, 0")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.state_time = 0;

        // Tick 1: Time == 0 → one request.
        let r1 = lc.tick(&mut ch);
        assert_eq!(r1.sound_requests.len(), 1, "tick 1 emits one request");

        // Tick 2: Time is now 1 → trigger false → fresh empty report.
        let r2 = lc.tick(&mut ch);
        assert!(
            r2.sound_requests.is_empty(),
            "tick 2 report must be fresh/empty, not carry tick 1's request"
        );
    }

    // ---- AC2: multiple PlaySnd in one tick preserve fire order -------------

    /// AC2: two PlaySnd controllers in the same state both fire on one tick and
    /// push their requests onto `sound_requests` **in controller (fire) order**,
    /// as the doc comment on the field promises.
    #[test]
    fn multiple_play_snd_emit_in_fire_order() {
        let first = ctrl(0, "PlaySnd", &[], &[(1, &["1"])], None, &[("value", "1, 0")]);
        let second = ctrl(0, "PlaySnd", &[], &[(1, &["1"])], None, &[("value", "F2, 3")]);
        let lc = loaded(vec![stand_n(0, vec![first, second])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.sound_requests.len(), 2, "both PlaySnd fired");
        // Order matches controller order: own-snd group 1 first, common group 2.
        assert_eq!(report.sound_requests[0].group, 1);
        assert!(!report.sound_requests[0].common);
        assert_eq!(report.sound_requests[1].group, 2);
        assert!(report.sound_requests[1].common);
    }

    /// AC2/AC3: a PlaySnd whose trigger is false does NOT fire and emits no
    /// request — gating must precede emission.
    #[test]
    fn play_snd_not_fired_when_trigger_false_emits_no_request() {
        let c = ctrl(0, "PlaySnd", &[], &[(1, &["0"])], None, &[("value", "1, 0")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 0, "gating failed → not dispatched");
        assert!(report.sound_requests.is_empty(), "no fire → no request");
    }

    // ---- AC2: MUGEN-semantic value parsing edge cases ----------------------

    /// AC2: negative group and sample numbers are valid MUGEN sound ids and must
    /// parse through unchanged (the `value` is read from raw source, so the `-`
    /// is preserved).
    #[test]
    fn play_snd_negative_group_and_sample_parse() {
        let report = play_snd_tick(&[("value", "-1, -2")]);
        let req = &report.sound_requests[0];
        assert_eq!(req.group, -1);
        assert_eq!(req.sample, -2);
        assert!(!req.common);
    }

    /// AC2: surrounding/interior whitespace and tab padding in the `value`
    /// tokens is trimmed before parsing (MUGEN ignores it).
    #[test]
    fn play_snd_value_tolerates_whitespace_padding() {
        let report = play_snd_tick(&[("value", "  7 ,   8 ")]);
        let req = &report.sound_requests[0];
        assert_eq!(req.group, 7);
        assert_eq!(req.sample, 8);
    }

    /// AC2: an `F` prefix followed by whitespace then digits (`F 5`) still sets
    /// `common` and parses the group — the flag char is stripped and the
    /// remainder is trimmed before the integer parse.
    #[test]
    fn play_snd_f_prefix_with_inner_space_still_common() {
        let report = play_snd_tick(&[("value", "F 5, 1")]);
        let req = &report.sound_requests[0];
        assert!(req.common, "F<space>5 → common");
        assert_eq!(req.group, 5);
        assert_eq!(req.sample, 1);
    }

    /// AC2: an unknown leading letter other than F/S (e.g. `X`) is treated as a
    /// non-common flag (own .snd) but its trailing digits are still parsed,
    /// matching the documented "S or other unknown leading letter" rule.
    #[test]
    fn play_snd_unknown_letter_prefix_is_own_snd_parses_digits() {
        let report = play_snd_tick(&[("value", "X9, 1")]);
        let req = &report.sound_requests[0];
        assert!(!req.common, "non-F letter → own .snd (common = false)");
        assert_eq!(req.group, 9);
        assert_eq!(req.sample, 1);
    }

    // ---- AC2: param defaults when individually present/absent --------------

    /// AC2: when only `channel` is given, `volume_scale` still defaults to 100
    /// and `looping` to false; conversely when only `volumescale` is given,
    /// `channel` still defaults to -1. Confirms each optional param defaults
    /// independently.
    #[test]
    fn play_snd_optional_params_default_independently() {
        let only_channel = play_snd_tick(&[("value", "1, 0"), ("channel", "4")]);
        let r = &only_channel.sound_requests[0];
        assert_eq!(r.channel, 4);
        assert_eq!(r.volume_scale, 100, "volumescale defaults when absent");
        assert!(!r.looping, "loop defaults to false when absent");

        let only_vol = play_snd_tick(&[("value", "1, 0"), ("volumescale", "50")]);
        let r = &only_vol.sound_requests[0];
        assert_eq!(r.channel, -1, "channel defaults to -1 when absent");
        assert_eq!(r.volume_scale, 50);
    }

    /// AC2: `channel = 0` is honored as an explicit value (the reserved voice
    /// channel), distinct from the absent default of -1. Guards against an
    /// implementation that confuses "channel 0" with "no channel".
    #[test]
    fn play_snd_explicit_channel_zero_is_honored() {
        let report = play_snd_tick(&[("value", "1, 0"), ("channel", "0")]);
        assert_eq!(
            report.sound_requests[0].channel, 0,
            "explicit channel 0 must not collapse to the -1 default"
        );
    }

    /// AC2: optional numeric params are *expressions* evaluated against the
    /// character, not literals. A `volumescale = 50 + 25` and a
    /// `channel = var(3)` resolve against `self`. Confirms the emitter uses the
    /// VM (`eval_param`) for these params, matching authored MUGEN content.
    #[test]
    fn play_snd_numeric_params_are_evaluated_expressions() {
        let c = ctrl(
            0,
            "PlaySnd",
            &[],
            &[(1, &["1"])],
            None,
            &[("value", "1, 0"), ("channel", "var(3)"), ("volumescale", "50 + 25")],
        );
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vars[3] = 6;
        let report = lc.tick(&mut ch);
        let req = &report.sound_requests[0];
        assert_eq!(req.channel, 6, "channel resolved from var(3)");
        assert_eq!(req.volume_scale, 75, "volumescale resolved from 50 + 25");
    }

    /// AC2 (MUGEN distinction): the MUGEN `volume`/`volumescale` distinction —
    /// the emitter reads `volumescale`, NOT `volume`. A controller carrying only
    /// `volume` (as real KFM does, e.g. `volume = -40`) leaves `volume_scale` at
    /// its 100 default. This documents the known gap: `volume` is not yet mapped.
    #[test]
    fn play_snd_volume_param_is_not_volumescale() {
        let report = play_snd_tick(&[("value", "1, 0"), ("volume", "-40")]);
        assert_eq!(
            report.sound_requests[0].volume_scale, 100,
            "`volume` (additive dB) is not `volumescale`; volume_scale stays at default"
        );
    }

    // ---- AC3: robustness — bad content never panics, emits nothing ---------

    /// AC3: a wide battery of malformed `value` strings each push NO request and
    /// never panic. Extends Forge's garbage test with whitespace-only, lone
    /// comma, float-looking, and trailing-junk forms.
    #[test]
    fn play_snd_more_garbage_values_emit_no_request() {
        for bad in [
            "   ",      // whitespace only
            ",",        // lone comma, both tokens empty
            ", 5",      // empty group
            "5, ",      // empty sample
            "1.5, 0",   // float group (parse::<i32> fails)
            "1, 2.5",   // float sample
            "1 2, 0",   // space-separated junk in group
            "0x1, 0",   // hex literal (not a plain i32)
            "S, 0",     // S prefix with no digits
            "FF, 0",    // F prefix then a non-digit letter
        ] {
            let report = play_snd_tick(&[("value", bad)]);
            assert!(
                report.sound_requests.is_empty(),
                "garbage value {bad:?} must emit no request"
            );
        }
    }

    /// AC3: a malformed optional param (`channel`/`volumescale` that evaluate via
    /// the const-0 fallback, `loop` garbage) never prevents the request when the
    /// `value` is well-formed; the bad params fall back to safe defaults and the
    /// request is still emitted. Never panics.
    #[test]
    fn play_snd_garbage_optional_params_fall_back_and_still_emit() {
        // `channel`/`volumescale` are compiled expressions; a non-arithmetic
        // token compiles to the const-0 fallback (group = 0). `loop` garbage is
        // not bool-ish → false.
        let report = play_snd_tick(&[
            ("value", "1, 0"),
            ("channel", "@@@"),
            ("volumescale", "$$$"),
            ("loop", "maybe"),
        ]);
        assert_eq!(report.sound_requests.len(), 1, "request still emitted");
        let req = &report.sound_requests[0];
        assert_eq!(req.group, 1);
        // Fallback expressions evaluate to 0.
        assert_eq!(req.channel, 0, "garbage channel expr → const-0 fallback");
        assert_eq!(req.volume_scale, 0, "garbage volumescale expr → const-0 fallback");
        assert!(!req.looping, "non-bool-ish loop token → false");
    }

    // ---- AC4: empty sound_requests on a no-controller / empty state --------

    /// AC4: a state with no controllers at all produces an empty
    /// `sound_requests` (complements Forge's "VelSet-only" empty test).
    #[test]
    fn sound_requests_empty_on_state_with_no_controllers() {
        let lc = loaded(vec![stand_n(0, vec![])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        assert!(lc.tick(&mut ch).sound_requests.is_empty());
    }

    // ---- AC2: loop flag bool-ish corner values via the full pipeline -------

    /// AC2: the `loop` flag corner values exercised end-to-end through the
    /// emitter (not just the `parse_loop_flag` unit): `+1`/`-1`/`true`/`TRUE`
    /// loop; `0`/`2`/`false`/empty do not.
    #[test]
    fn play_snd_loop_flag_corner_values_end_to_end() {
        for (tok, expect) in [
            ("1", true),
            ("-1", true),
            ("true", true),
            ("TRUE", true),
            ("0", false),
            ("2", false),
            ("false", false),
            ("", false),
        ] {
            let report = play_snd_tick(&[("value", "1, 0"), ("loop", tok)]);
            assert_eq!(
                report.sound_requests[0].looping, expect,
                "loop = {tok:?} should be looping = {expect}"
            );
        }
    }

    // ---- AC2: F-prefix common flag survives the CNS pipeline (real form) ---

    /// AC2: the `value = F5, 2` form — exactly as authored in real KFM
    /// (`kfm.cns` state 200) — parses through the CNS parser and the compiled
    /// controller, emitting a common-file request with the F flag stripped. This
    /// is the synthetic-CNS counterpart to the gated fixture test below.
    #[test]
    fn play_snd_f_prefix_via_cns_text_sets_common() {
        let cns = CnsFile::from_str(
            "[Statedef 0]\ntype = S\nphysics = N\n\
             [State 0, snd]\ntype = PlaySnd\ntrigger1 = 1\nvalue = F5, 2\nvolume = -40\n",
        )
        .expect("valid synthetic CNS");
        let st = CompiledState::from_parsed(&cns.statedefs[0]);
        let lc = loaded(vec![st], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1, "PlaySnd dispatched");
        assert_eq!(
            report.sound_requests,
            vec![SoundRequest {
                group: 5,
                sample: 2,
                channel: -1,
                volume_scale: 100, // `volume` is not `volumescale`; stays default
                looping: false,
                common: true, // F prefix → common/fight sound file
            }]
        );
    }

    // ---- AC5: gated real-KFM fixture — PlaySnd emits from authored content --

    /// AC5: load the real KFM character and run a real parsed/compiled state
    /// that authors a `PlaySnd` firing on its **entry tick** (`Time = 0`), so the
    /// authored `value = group, sample` flows through the whole
    /// parse -> compile -> emit pipeline and yields a `SoundRequest`.
    ///
    /// KFM's `[Statedef 1300]` (the stand reversal/counter) authors
    /// `[State 1300, 1] type = PlaySnd / trigger1 = Time = 0 / value = 0, 1`,
    /// and its only `ChangeState` is gated on `AnimTime = 0` (the end of the
    /// move), so the PlaySnd fires on tick 1 before anything transitions away.
    /// The compiled state is run directly through [`Character::tick_with`] over a
    /// minimal map holding only that state, bypassing KFM's `[Statedef -1]`
    /// command bridge — which would otherwise `ChangeState` an idle,
    /// control-less character back to stand every tick. This still exercises the
    /// real authored controller and the real compiled `value` param. Skips
    /// cleanly when `test-assets/` is absent.
    ///
    /// A `Time = 0` PlaySnd is deliberately chosen over the stand-punch
    /// (`Statedef 200`, PlaySnd at `Time = 1`): the executor advances
    /// `anim_time` only at the *end* of a tick, so on a state's first tick an
    /// `AnimTime = 0`-gated `ChangeState` fires spuriously and pre-empts a
    /// later-than-entry PlaySnd. Firing at `Time = 0` avoids that unrelated
    /// quirk and keeps this test focused on the 8.3a emit path.
    #[test]
    fn real_kfm_play_snd_emits_sound_request() {
        let def = test_asset("kfm/kfm.def");
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

        // KFM's stand reversal state authors a PlaySnd at Time = 0.
        const SND_STATE: i32 = 1300;
        let Some(state) = lc.states.get(&SND_STATE).cloned() else {
            eprintln!("skipping: KFM has no [Statedef {SND_STATE}]; asset differs");
            return;
        };
        // Sanity: the authored state really does carry a PlaySnd controller.
        assert!(
            state.controllers.iter().any(|c| c
                .controller_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("PlaySnd"))),
            "KFM [Statedef {SND_STATE}] should author a PlaySnd controller"
        );

        // Minimal state map: just the real compiled state. Drive it directly so
        // the special states do not bounce us out before the PlaySnd fires.
        let mut states = HashMap::new();
        states.insert(SND_STATE, state);
        let air = lc.air.clone();

        let mut ch = Character::with_constants(lc.constants);
        // Enter through the proper seam so entry params initialize the cursor.
        ch.change_state(&states, SND_STATE);

        // The PlaySnd fires on the entry tick (Time = 0).
        let report = ch.tick_with(&states, &air, None, StageView::default());

        // KFM authors `value = 0, 1` (own .snd) here: one request, group 0,
        // sample 1, common = false, with the emitter's MUGEN defaults for the
        // params it does not author.
        assert!(
            report
                .sound_requests
                .iter()
                .any(|r| r.group == 0 && r.sample == 1 && !r.common),
            "expected the authored `value = 0, 1` own-snd request from real KFM \
             [Statedef {SND_STATE}]; got {:?}",
            report.sound_requests
        );
        // Defaults (channel -1, volume_scale 100, not looping) hold for the
        // authored request since KFM 1300 specifies none of those params.
        let req = report
            .sound_requests
            .iter()
            .find(|r| r.group == 0 && r.sample == 1)
            .expect("authored request present (asserted above)");
        assert_eq!(req.channel, -1, "unspecified channel → MUGEN default -1");
        assert_eq!(req.volume_scale, 100, "unspecified volumescale → 100");
        assert!(!req.looping, "unspecified loop → false");
    }

    // =====================================================================
    // Task A.P6: AnimElemTime(n) per-element timing table. The executor builds
    // a cumulative start-offset table in advance_animation so AnimElemTime(n)
    // resolves for EVERY element of the current action (past = positive,
    // current = anim_elem_time, future = negative), reflects the current loop
    // iteration, and is safe for out-of-range n. All synthetic except the gated
    // real-KFM test at the end.
    // =====================================================================

    /// Convenience: read `AnimElemTime(n)` (one-based) through the trigger seam,
    /// exactly as a compiled CNS expression would.
    fn anim_elem_time(ch: &Character, n: i32) -> i32 {
        ch.trigger("AnimElemTime", &[fp_vm::Value::Int(n)]).to_int()
    }

    /// AC1/AC3: a synthetic three-element action (ticks `[3, 5, 2]`) ticked
    /// forward; `AnimElemTime(1/2/3)` is positive-and-growing for the current /
    /// past elements, equals `anim_elem_time` for the current element, and is
    /// negative for not-yet-reached future elements.
    #[test]
    fn anim_elem_time_resolves_for_all_elements() {
        // Element start offsets for ticks [3,5,2]: [0, 3, 8]; total = 10.
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[3, 5, 2]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.anim_elem = 0;
        ch.anim_elem_time = 0;

        // --- During element 0 (ticks 1..3) ---
        lc.tick(&mut ch); // elem 0, time 1
        assert_eq!(ch.anim_elem, 0);
        assert_eq!(ch.anim_elem_time, 1);
        assert_eq!(anim_elem_time(&ch, 1), 1, "current element == anim_elem_time");
        assert!(anim_elem_time(&ch, 2) < 0, "element 2 not yet reached");
        assert!(anim_elem_time(&ch, 3) < 0, "element 3 not yet reached");

        lc.tick(&mut ch); // elem 0, time 2
        assert_eq!(anim_elem_time(&ch, 1), 2, "current element grows with time");

        // --- Cross into element 1 (tick 3 ends element 0 at dur 3) ---
        lc.tick(&mut ch); // elem 1, time 0
        assert_eq!(ch.anim_elem, 1);
        assert_eq!(ch.anim_elem_time, 0);
        // Element 1 just began: time-since == anim_elem_time == 0.
        assert_eq!(anim_elem_time(&ch, 2), 0, "current element == anim_elem_time");
        // Element 1 starts at offset 3; elapsed is 3 → time-since-element-1 = 3.
        assert_eq!(anim_elem_time(&ch, 1), 3, "past element positive & growing");
        assert!(anim_elem_time(&ch, 3) < 0, "element 3 still not reached");

        lc.tick(&mut ch); // elem 1, time 1
        assert_eq!(anim_elem_time(&ch, 2), 1, "current element grows");
        assert_eq!(anim_elem_time(&ch, 1), 4, "past element keeps growing");
        assert!(anim_elem_time(&ch, 3) < 0);

        // Drive into element 2 (element 1 has dur 5: ends after 5 ticks in it).
        for _ in 0..4 {
            lc.tick(&mut ch);
        }
        // After 5 ticks in elem 1 we land on elem 2, time 0.
        assert_eq!(ch.anim_elem, 2);
        assert_eq!(ch.anim_elem_time, 0);
        assert_eq!(anim_elem_time(&ch, 3), 0, "current element == anim_elem_time");
        // Element 2 starts at offset 8; element 1 at 3, element 0 at 0.
        assert_eq!(anim_elem_time(&ch, 1), 8);
        assert_eq!(anim_elem_time(&ch, 2), 5);
    }

    /// AC2/AC3: a looping action reflects the CURRENT loop iteration — after the
    /// action wraps back to element 0, AnimElemTime restarts from that iteration
    /// (it is not cumulative across loops).
    #[test]
    fn anim_elem_time_reflects_current_loop_iteration() {
        // Two elements, ticks [2, 2], loopstart 0. Total iteration length 4.
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[2, 2]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;

        // Play a full iteration (4 ticks) so the cursor wraps back to element 0.
        for _ in 0..4 {
            lc.tick(&mut ch);
        }
        assert_eq!(ch.anim_elem, 0, "wrapped back to loopstart element 0");
        assert_eq!(ch.anim_elem_time, 0);
        // Fresh iteration: element 0 just (re)started, element 1 not yet reached.
        assert_eq!(anim_elem_time(&ch, 1), 0, "loop iteration restarts the clock");
        assert!(
            anim_elem_time(&ch, 2) < 0,
            "element 2 negative again in the new loop iteration"
        );

        // One more tick keeps us in the current iteration's element 0.
        lc.tick(&mut ch);
        assert_eq!(anim_elem_time(&ch, 1), 1);
        assert!(anim_elem_time(&ch, 2) < 0);
    }

    /// AC3: a non-zero loopstart action wraps to its loopstart and AnimElemTime
    /// is measured from that element's offset in the new iteration.
    #[test]
    fn anim_elem_time_with_nonzero_loopstart() {
        // Three elements, ticks [1, 1, 1], loopstart 1: after the last element
        // the cursor returns to element 1 (offsets [0, 1, 2]).
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], air_with_loopstart(0, &[1, 1, 1], 1));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;

        // 3 ticks plays elements 0,1,2; the 3rd tick ends element 2 → wrap to 1.
        for _ in 0..3 {
            lc.tick(&mut ch);
        }
        assert_eq!(ch.anim_elem, 1, "wrapped to loopstart 1");
        assert_eq!(ch.anim_elem_time, 0);
        // In this iteration element 1 just began (offset 1). Element 0 (offset 0)
        // reads as the current iteration's element-1 elapsed minus its offset.
        assert_eq!(anim_elem_time(&ch, 2), 0, "current element == anim_elem_time");
        assert_eq!(anim_elem_time(&ch, 1), 1, "offset gap to element 1 start");
        assert!(anim_elem_time(&ch, 3) < 0, "element 3 not yet reached again");
    }

    /// AC2: out-of-range `n` (n < 1 and n > num_elements) is clamped to a valid
    /// element and never panics; the result is a finite, sane time.
    #[test]
    fn anim_elem_time_out_of_range_is_safe() {
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[3, 5, 2]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        lc.tick(&mut ch); // elem 0, time 1

        // n = 0 clamps to element 1; n = 99 clamps to the last element (3).
        let clamped_low = anim_elem_time(&ch, 0);
        let clamped_high = anim_elem_time(&ch, 99);
        assert_eq!(clamped_low, anim_elem_time(&ch, 1), "n<1 clamps to element 1");
        assert_eq!(clamped_high, anim_elem_time(&ch, 3), "n>len clamps to last element");
        // Strongly negative n must also be safe (no overflow, no panic).
        let _ = anim_elem_time(&ch, i32::MIN);
        let _ = anim_elem_time(&ch, i32::MAX);
    }

    /// AC1/AC3 (regression): for the current element, AnimElemTime equals the
    /// legacy `anim_elem_time` scalar at every tick, with and without a built
    /// offset table.
    #[test]
    fn anim_elem_time_current_matches_legacy_scalar() {
        // With NO table yet (fields set directly, never ticked) the legacy
        // fallback path must still answer current==anim_elem_time, future<0.
        let mut ch = Character::new();
        ch.anim = 0;
        ch.anim_elem = 1; // one-based element 2 is current
        ch.anim_elem_time = 7;
        assert!(ch.anim_elem_start_offsets.is_empty(), "no table built yet");
        assert_eq!(anim_elem_time(&ch, 2), 7, "legacy: current == anim_elem_time");
        assert!(anim_elem_time(&ch, 5) < 0, "legacy: future element negative");

        // With a table built by ticking, the current element still matches.
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[4, 4, 4]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        for _ in 0..10 {
            lc.tick(&mut ch);
            let current_one_based = ch.anim_elem + 1;
            assert_eq!(
                anim_elem_time(&ch, current_one_based),
                ch.anim_elem_time,
                "current element AnimElemTime must equal the legacy scalar"
            );
        }
    }

    /// AC1: the offset table is rebuilt when the action number changes (a
    /// ChangeAnim to a different-length action repopulates the offsets), so
    /// AnimElemTime reads the new action's geometry.
    #[test]
    fn offset_table_rebuilds_on_action_change() {
        let mut air = tiny_air(0, &[2, 2]);
        add_action(&mut air, 1, &[5, 1, 1]); // different action, different offsets
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], air);
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        lc.tick(&mut ch);
        assert_eq!(ch.anim_elem_start_offsets, vec![0, 2], "action 0 offsets");
        assert_eq!(ch.anim_table_action, Some(0));

        // Switch to action 1; the table must rebuild on the next advance.
        ch.anim = 1;
        ch.anim_elem = 0;
        ch.anim_elem_time = 0;
        lc.tick(&mut ch);
        assert_eq!(ch.anim_elem_start_offsets, vec![0, 5, 6], "action 1 offsets");
        assert_eq!(ch.anim_table_action, Some(1));
    }

    /// AC2 (gated, skips if test-assets absent): drive a real KFM action and
    /// confirm two distinct elements report distinct, sane AnimElemTime values.
    #[test]
    fn real_kfm_anim_elem_time_two_elements_distinct() {
        let def = test_asset("kfm/kfm.def");
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
        let mut ch = Character::with_constants(lc.constants);
        ch.state_no = 0;
        ch.anim = 0;
        ch.ctrl = true;

        // Tick until KFM's idle/stand action has advanced past its first element
        // so element 1 is in the past and the current element is later. Stop once
        // we are on at least the second element with a multi-element table.
        let mut advanced = false;
        for _ in 0..120 {
            let _ = ch.tick(&lc, None, StageView::default());
            if ch.anim_elem >= 1 && ch.anim_elem_start_offsets.len() >= 2 {
                advanced = true;
                break;
            }
        }
        if !advanced {
            eprintln!("skipping: KFM anim 0 did not advance past element 0 in 120 ticks");
            return;
        }

        let cur = ch.anim_elem + 1; // one-based current element
        let past = cur - 1; // a strictly-earlier element
        let t_cur = anim_elem_time(&ch, cur);
        let t_past = anim_elem_time(&ch, past);
        // Current element equals the legacy scalar; past element is strictly
        // larger (it began earlier in this iteration) — distinct, sane times.
        assert_eq!(t_cur, ch.anim_elem_time, "current == legacy scalar");
        assert!(
            t_past > t_cur,
            "earlier element ({past}) must report a larger time-since than the \
             current element ({cur}): {t_past} vs {t_cur}"
        );
        assert!(t_past >= 0 && t_cur >= 0, "reached elements are non-negative");
    }

    // =====================================================================
    // Task A.P6 — Proctor supplementary coverage. These exercise gaps the
    // implementation-author tests did not cover: hold-forever (`ticks <= 0`)
    // elements, stale-table cleanup when the action becomes unknown / empty,
    // the offset/elapsed invariant across a multi-element advance, the bare
    // AnimElem / AnimElemNo / AnimTime triggers as a co-regression, and a
    // single-element action. All synthetic, never panics.
    // =====================================================================

    /// AC1/AC2: a hold-forever element (`ticks <= 0`, MUGEN's `-1`) contributes
    /// `0` to later start offsets, the cursor parks on it forever, and
    /// `AnimElemTime` stays sane: the current (hold-forever) element grows with
    /// time and equals the legacy scalar; an element BEFORE it stays positive.
    #[test]
    fn anim_elem_time_hold_forever_element_is_safe() {
        // ticks [2, -1, 4]: offsets [0, 2, 2] (the -1 contributes 0 onward).
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[2, -1, 4]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;

        // 2 ticks finish element 0 and land us on the hold-forever element 1.
        lc.tick(&mut ch);
        lc.tick(&mut ch);
        assert_eq!(ch.anim_elem, 1, "parked on the hold-forever element");
        assert_eq!(ch.anim_elem_start_offsets, vec![0, 2, 2], "hold-forever → 0 onward");
        assert_eq!(ch.anim_elem_time, 0);
        // Current element (2, one-based) just began.
        assert_eq!(anim_elem_time(&ch, 2), 0, "current == anim_elem_time");
        // Element 1 (offset 0) began 2 ticks ago.
        assert_eq!(anim_elem_time(&ch, 1), 2);
        // Element 3 shares element 2's offset (2) → reads as already reached (0),
        // which is fine: the cursor never gets there, so it is never queried in
        // anger. The key contract is no panic and a finite value.
        let _ = anim_elem_time(&ch, 3);

        // Many more ticks: the cursor must NOT advance off the hold-forever frame
        // and the current-element time must keep growing in lockstep with the
        // legacy scalar.
        for expected in 1..=20 {
            lc.tick(&mut ch);
            assert_eq!(ch.anim_elem, 1, "still parked on hold-forever element");
            assert_eq!(ch.anim_elem_time, expected);
            assert_eq!(
                anim_elem_time(&ch, 2),
                ch.anim_elem_time,
                "hold-forever current element tracks the legacy scalar"
            );
            assert_eq!(anim_elem_time(&ch, 1), 2 + expected, "earlier element grows too");
        }
    }

    /// AC1 (invariant): at an arbitrary mid-action moment the public
    /// `AnimElemTime(n)` for a PAST element equals the hand-computed
    /// `offset[cur] + anim_elem_time - offset[n-1]`. Guards against an off-by-one
    /// in the elapsed/offset arithmetic across a multi-element advance.
    #[test]
    fn anim_elem_time_matches_offset_elapsed_invariant() {
        // ticks [3, 5, 2, 4]: offsets [0, 3, 8, 10].
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[3, 5, 2, 4]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;

        // Tick to land mid-action on element 2 (one-based 3): 3 + 5 = 8 ticks to
        // reach it, +1 tick into it.
        for _ in 0..9 {
            lc.tick(&mut ch);
        }
        assert_eq!(ch.anim_elem, 2, "mid-action, on element index 2");
        assert_eq!(ch.anim_elem_time, 1);

        let offsets = ch.anim_elem_start_offsets.clone();
        assert_eq!(offsets, vec![0, 3, 8, 10]);
        let cur = ch.anim_elem as usize;
        let elapsed = offsets[cur] + ch.anim_elem_time; // 8 + 1 = 9
        assert_eq!(elapsed, 9);
        // Verify the trigger output for EVERY element against the closed form.
        for one_based in 1..=4i32 {
            let expected = elapsed - offsets[(one_based - 1) as usize];
            assert_eq!(
                anim_elem_time(&ch, one_based),
                expected,
                "AnimElemTime({one_based}) must equal elapsed - offset[n-1]"
            );
        }
        // Spot-check signs: elements 1,2,3 reached (>=0), element 4 future (<0).
        assert!(anim_elem_time(&ch, 1) > 0);
        assert!(anim_elem_time(&ch, 2) > 0);
        assert_eq!(anim_elem_time(&ch, 3), ch.anim_elem_time, "current == scalar");
        assert!(anim_elem_time(&ch, 4) < 0, "last element not yet reached");
    }

    /// AC2: switching to an UNKNOWN animation drops the stale offset table so
    /// `AnimElemTime` reverts to the legacy single-element fallback (rather than
    /// indexing the previous action's geometry). Never panics.
    #[test]
    fn unknown_action_clears_stale_offset_table() {
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[3, 5, 2]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        lc.tick(&mut ch);
        assert_eq!(ch.anim_elem_start_offsets, vec![0, 3, 8], "table built for action 0");

        // Point anim at an action the AIR does not define.
        ch.anim = 9999;
        ch.anim_elem = 0;
        ch.anim_elem_time = 4;
        lc.tick(&mut ch);
        assert!(
            ch.anim_elem_start_offsets.is_empty(),
            "stale table dropped for unknown action"
        );
        assert_eq!(ch.anim_table_action, Some(9999));
        // Legacy fallback: current element (1) == scalar, others negative.
        assert_eq!(anim_elem_time(&ch, 1), ch.anim_elem_time, "fallback current == scalar");
        assert!(anim_elem_time(&ch, 2) < 0, "fallback future element negative");
    }

    /// AC2: an action with zero frames also yields the empty-table legacy
    /// fallback and never panics.
    #[test]
    fn empty_frame_action_uses_legacy_fallback() {
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        // tiny_air with an empty slice → action 0 exists but has no frames.
        let lc = loaded(vec![st], tiny_air(0, &[]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        ch.anim_elem = 0;
        ch.anim_elem_time = 2;
        lc.tick(&mut ch);
        assert!(ch.anim_elem_start_offsets.is_empty(), "no frames → empty table");
        assert_eq!(anim_elem_time(&ch, 1), ch.anim_elem_time, "current via legacy fallback");
        assert!(anim_elem_time(&ch, 2) < 0);
        // Strong out-of-range n on the legacy path must also be safe.
        let _ = anim_elem_time(&ch, i32::MIN);
        let _ = anim_elem_time(&ch, i32::MAX);
        let _ = anim_elem_time(&ch, 0);
    }

    /// AC1: a single-element action — `AnimElemTime(1)` tracks the scalar and any
    /// other (clamped) n collapses to element 1; never negative-by-accident.
    #[test]
    fn anim_elem_time_single_element_action() {
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        // Single hold-forever element so the cursor parks and time accrues.
        let lc = loaded(vec![st], tiny_air(0, &[-1]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        for expected in 1..=5 {
            lc.tick(&mut ch);
            assert_eq!(ch.anim_elem, 0, "single element stays current");
            assert_eq!(anim_elem_time(&ch, 1), expected, "element 1 == scalar");
            // Out-of-range high clamps to element 1 (the only element).
            assert_eq!(anim_elem_time(&ch, 7), anim_elem_time(&ch, 1));
            // Out-of-range low (and i32::MIN) clamp to element 1 too — never panic.
            assert_eq!(anim_elem_time(&ch, 0), anim_elem_time(&ch, 1));
            let _ = anim_elem_time(&ch, i32::MIN);
        }
    }

    /// AC3 (co-regression): the per-element AnimElemTime work leaves the bare
    /// `AnimElem`, `AnimElemNo`, and `AnimTime` triggers intact and consistent
    /// with the cursor at every tick of a multi-element action.
    #[test]
    fn bare_anim_triggers_unchanged_alongside_elem_time() {
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], tiny_air(0, &[3, 5, 2]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;
        for _ in 0..12 {
            lc.tick(&mut ch);
            let elem_no = ch.trigger("AnimElemNo", &[]).to_int();
            let elem = ch.trigger("AnimElem", &[]).to_int();
            let anim_time = ch.trigger("AnimTime", &[]).to_int();
            // AnimElem and AnimElemNo are both the one-based current element.
            assert_eq!(elem, ch.anim_elem + 1, "AnimElem one-based current");
            assert_eq!(elem_no, ch.anim_elem + 1, "AnimElemNo one-based current");
            // AnimTime mirrors the executor's anim_time field unchanged.
            assert_eq!(anim_time, ch.anim_time, "AnimTime mirrors anim_time field");
            // And AnimElemTime(current) still agrees with the scalar.
            assert_eq!(anim_elem_time(&ch, elem_no), ch.anim_elem_time);
        }
    }

    /// AC2: a looping action queried for an element BEYOND the loopstart element
    /// reports negative again on each fresh iteration (the "reached" guard
    /// re-arms every loop), proving the time is per-iteration, not cumulative.
    #[test]
    fn looping_future_element_re_arms_each_iteration() {
        // ticks [2, 2, 2], loopstart 0: offsets [0, 2, 4], iteration length 6.
        let st = state(0, Entry { st: Some("S"), ph: Some("N"), anim: Some("0"), ..Entry::default() }, vec![]);
        let lc = loaded(vec![st], air_with_loopstart(0, &[2, 2, 2], 0));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.anim = 0;

        // Iteration 1: drive to element 2, where element 3 is still future (<0).
        for _ in 0..5 {
            lc.tick(&mut ch);
        }
        assert_eq!(ch.anim_elem, 2);
        assert!(anim_elem_time(&ch, 3) >= 0, "element 3 reached in iter 1");

        // Complete the iteration → wrap to element 0 (one more tick ends elem 2).
        lc.tick(&mut ch);
        assert_eq!(ch.anim_elem, 0, "wrapped to loopstart");
        assert_eq!(ch.anim_elem_time, 0);
        // Fresh iteration: elements 2 AND 3 are future again (negative).
        assert!(anim_elem_time(&ch, 2) < 0, "element 2 future again after wrap");
        assert!(anim_elem_time(&ch, 3) < 0, "element 3 future again after wrap");
        // Element 1 (current, offset 0) just restarted.
        assert_eq!(anim_elem_time(&ch, 1), 0, "current element restarted at 0");
    }
}
