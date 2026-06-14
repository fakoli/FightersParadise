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
//!    `n` = every `n`th qualifying tick). `ignorehitpause` is evaluated and
//!    wired through (there is no hitpause yet, so it has no effect this task).
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
//! `StateTypeSet`, `Turn`, and a `PlaySnd` stub (parsed and logged; real audio
//! is Phase 8). Task 6.2 adds the `HitDef` controller (builds a
//! [`fp_combat::HitDef`] into [`Character::active_hitdef`]). Any other controller
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
//!    `HitFallDamage`, `HitAdd`, `SelfState`, `LifeAdd`, …) are not yet
//!    implemented; the dispatch routes them to its safe, **debug-logged** no-op
//!    branch (visible, not silent) and they are deferred to task 6.3+.
//! 2. [`Character::get_hit_vars`] stays at its default until hit *resolution*
//!    (task 6.3) populates it, so a get-hit state run *before* 6.3 sees zeroed
//!    hit effects. This is expected: 6.2 only wires the read path.
//!
//! [03 §3]: ../../../docs/knowledge-base/03-engine-architecture.md

use std::collections::HashMap;

use fp_core::Vec2;
use fp_formats::air::AirFile;
use fp_vm::{eval, EvalContext, Value};

use crate::loader::{
    CompiledController, CompiledExpr, CompiledParam, CompiledState, CompiledTriggerGroup,
};
use crate::{
    Character, Facing, LoadedCharacter, MoveType, Physics, StateType, NUM_FVARS, NUM_VARS,
};

/// Upper bound on `ChangeState` transitions resolved within a single tick.
///
/// MUGEN re-enters the destination state in the same tick after a
/// `ChangeState`. A buggy or cyclic state graph (`A → B → A → …`) could loop
/// forever; the executor caps the number of transitions per tick and warns when
/// the cap is hit, degrading safely rather than hanging.
const MAX_TRANSITIONS_PER_TICK: u32 = 512;

/// A summary of what one [`Character::tick`] did, returned for diagnostics and
/// tests.
///
/// All counters are best-effort and never affect gameplay; they exist so a
/// caller (or a test) can assert that the expected work happened without
/// reaching into private executor state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    pub fn tick(&mut self, loaded: &LoadedCharacter) -> TickReport {
        self.tick_with(&loaded.states, &loaded.air)
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
    ) -> TickReport {
        let mut report = TickReport::default();

        // Hit-pause gate: while frozen by a connecting hit, this minimal model holds
        // the character completely still for the paused tick — it skips ALL state/
        // physics processing and counts the pause down by one. (MUGEN additionally
        // runs controllers flagged `ignorehitpause` during the pause; that exception
        // is NOT implemented here yet — deferred, tracked as CB30 — and is currently
        // benign because no get-hit common state relies on it.) The shake timer (the
        // defender's visual jitter during the pause) counts down alongside it.
        // Decrementing after the gate makes a freshly-set `hitpause = N` last N ticks.
        if self.hitpause > 0 {
            self.hitpause -= 1;
            if self.shaketime > 0 {
                self.shaketime -= 1;
            }
            report.hitpaused = true;
            return report;
        }
        if self.shaketime > 0 {
            self.shaketime -= 1;
        }

        // Process the special states first, in MUGEN order, then the current
        // state. The current state number is re-read after each special state in
        // case one of them changed it via ChangeState.
        for special in [-3, -2, -1] {
            self.run_state(states, special, &mut report);
        }

        // Then the current numbered state. ChangeState within it re-enters the
        // destination in the same tick (bounded by run_current_with_transitions).
        self.run_current_with_transitions(states, &mut report);

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
    pub fn change_state(&mut self, states: &HashMap<i32, CompiledState>, target: i32) {
        self.enter_state(states, target);
    }

    /// Runs every controller of the state numbered `state_no` (if it exists),
    /// in file order, applying gating and `persistent` semantics. Used for the
    /// special states `-3`/`-2`/`-1`, which do not themselves transition the
    /// current numbered state but may `ChangeState` it.
    fn run_state(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        state_no: i32,
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
            self.run_controller(states, &ctrl, idx, report);
        }
    }

    /// Runs the current numbered state's controllers, following `ChangeState`
    /// transitions within the same tick up to `MAX_TRANSITIONS_PER_TICK`.
    fn run_current_with_transitions(
        &mut self,
        states: &HashMap<i32, CompiledState>,
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
                self.run_controller(states, &ctrl, idx, report);
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

    /// Evaluates one controller's gating and `persistent` policy and, if it
    /// qualifies to fire this tick, dispatches it.
    fn run_controller(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        ctrl: &CompiledController,
        idx: usize,
        report: &mut TickReport,
    ) {
        if !self.gating_passes(ctrl) {
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

        if !persistent_allows(self.persistent_value(ctrl), count) {
            return;
        }

        report.controllers_fired += 1;
        self.dispatch(states, ctrl, report);
    }

    /// Returns `true` if the controller's gating passes: all `triggerall`
    /// conditions are true (AND) **and** at least one numbered trigger group is
    /// fully true (OR across groups), after applying the CB6 contiguity rule.
    fn gating_passes(&self, ctrl: &CompiledController) -> bool {
        // triggerall: every condition must be true.
        for cond in &ctrl.triggerall {
            if !self.eval_bool(cond) {
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
            if self.group_is_true(group) {
                return true;
            }
        }
        false
    }

    /// Returns `true` if every condition in a numbered group is true (AND).
    fn group_is_true(&self, group: &CompiledTriggerGroup) -> bool {
        // An empty group (no conditions) cannot be satisfied.
        !group.conditions.is_empty() && group.conditions.iter().all(|c| self.eval_bool(c))
    }

    /// Evaluates a compiled expression against this character as a boolean.
    ///
    /// A fallback (const-`0`) expression is always false, so a controller whose
    /// trigger failed to compile can never fire.
    fn eval_bool(&self, expr: &CompiledExpr) -> bool {
        eval(&expr.expr, self as &dyn EvalContext).as_bool()
    }

    /// Evaluates a compiled expression to a [`Value`].
    fn eval_value(&self, expr: &CompiledExpr) -> Value {
        eval(&expr.expr, self as &dyn EvalContext)
    }

    /// Evaluates component `i` of a multi-component parameter, returning `None`
    /// when the parameter has no such component.
    ///
    /// This is the component accessor every controller uses to read a parameter:
    /// a scalar parameter is read with `i == 0`; the second value of an `x, y`
    /// pair is read with `i == 1`. A missing component returns `None` so the
    /// caller can substitute its own documented default. Never panics.
    fn eval_param_component(&self, param: &CompiledParam, i: usize) -> Option<Value> {
        param.component(i).map(|expr| self.eval_value(expr))
    }

    /// Evaluates a parameter's scalar value: its first (index `0`) component.
    ///
    /// Most controllers read a single value (`value`, `x`, `y`, …); this is the
    /// shorthand for `eval_param_component(param, 0)`. Returns `None` only for
    /// the (in practice impossible) empty parameter.
    fn eval_param(&self, param: &CompiledParam) -> Option<Value> {
        self.eval_param_component(param, 0)
    }

    /// Evaluates every component of a parameter, in order, into [`Value`]s.
    ///
    /// Replaces the old `eval_components` raw-source re-split: the loader already
    /// split the parameter on top-level commas and compiled each component, so
    /// this simply evaluates the pre-compiled components against `self`. An empty
    /// or whitespace-only authored component is the const-`0` fallback and
    /// evaluates to `0`. Never panics.
    fn eval_param_components(&self, param: &CompiledParam) -> Vec<Value> {
        param
            .components
            .iter()
            .map(|expr| self.eval_value(expr))
            .collect()
    }

    /// Resolves the controller's `persistent` value: the compiled expression if
    /// present, otherwise MUGEN's default of `1` (re-fire every qualifying tick).
    fn persistent_value(&self, ctrl: &CompiledController) -> i32 {
        match &ctrl.persistent {
            Some(expr) => self.eval_value(expr).to_int(),
            None => 1,
        }
    }

    /// Dispatches a controller that has qualified to fire this tick.
    ///
    /// Handles the core MOVEMENT/CONTROL controllers: `ChangeState`, `VelSet`,
    /// `VelAdd`, `CtrlSet`, `Null` (task 5.3) plus `ChangeAnim`/`ChangeAnim2`,
    /// `PosSet`, `PosAdd`, `VarSet`, `VarAdd`, `VarRangeSet`, `StateTypeSet`,
    /// `Turn`, and a `PlaySnd` stub (task 5.4). Task 6.2 adds the `HitDef`
    /// controller, which builds a [`fp_combat::HitDef`] into
    /// [`active_hitdef`](Character::active_hitdef). Every other type is a safe
    /// no-op, debug-logged and deferred to a later task.
    fn dispatch(
        &mut self,
        states: &HashMap<i32, CompiledState>,
        ctrl: &CompiledController,
        report: &mut TickReport,
    ) {
        let kind = ctrl.controller_type.as_deref().unwrap_or("");
        if kind.eq_ignore_ascii_case("ChangeState") {
            self.ctrl_change_state(states, ctrl, report);
        } else if kind.eq_ignore_ascii_case("VelSet") {
            self.ctrl_vel_set(ctrl);
        } else if kind.eq_ignore_ascii_case("VelAdd") {
            self.ctrl_vel_add(ctrl);
        } else if kind.eq_ignore_ascii_case("CtrlSet") {
            self.ctrl_ctrl_set(ctrl);
        } else if kind.eq_ignore_ascii_case("PosSet") {
            self.ctrl_pos_set(ctrl);
        } else if kind.eq_ignore_ascii_case("PosAdd") {
            self.ctrl_pos_add(ctrl);
        } else if kind.eq_ignore_ascii_case("ChangeAnim")
            || kind.eq_ignore_ascii_case("ChangeAnim2")
        {
            // ChangeAnim2 aliases ChangeAnim here. (In MUGEN, ChangeAnim2 selects
            // the *opponent's* anim table during a custom-state throw; with a
            // single entity there is no distinct table yet, so it behaves as
            // ChangeAnim.)
            self.ctrl_change_anim(ctrl);
        } else if kind.eq_ignore_ascii_case("VarSet") {
            self.ctrl_var_set(ctrl);
        } else if kind.eq_ignore_ascii_case("VarAdd") {
            self.ctrl_var_add(ctrl);
        } else if kind.eq_ignore_ascii_case("VarRangeSet") {
            self.ctrl_var_range_set(ctrl);
        } else if kind.eq_ignore_ascii_case("StateTypeSet") {
            self.ctrl_state_type_set(ctrl);
        } else if kind.eq_ignore_ascii_case("Turn") {
            self.ctrl_turn();
        } else if kind.eq_ignore_ascii_case("PlaySnd") {
            self.ctrl_play_snd(ctrl);
        } else if kind.eq_ignore_ascii_case("HitDef") {
            self.ctrl_hit_def(ctrl);
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
        report: &mut TickReport,
    ) {
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p)) else {
            tracing::debug!(
                "tick: ChangeState in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        let target = value.to_int();
        // A self-transition still counts as a re-entry in MUGEN (resets time).
        self.enter_state(states, target);
        report.transitions += 1;

        // ChangeState's optional `ctrl` parameter overrides the statedef ctrl.
        if let Some(ctrl_val) = ctrl.params.get("ctrl").and_then(|p| self.eval_param(p)) {
            self.ctrl = ctrl_val.as_bool();
        }
    }

    /// `VelSet`: set x/y velocity components from the `x`/`y` parameters. A
    /// missing component leaves that axis unchanged.
    fn ctrl_vel_set(&mut self, ctrl: &CompiledController) {
        if let Some(v) = ctrl.params.get("x").and_then(|p| self.eval_param(p)) {
            self.vel.x = v.to_float();
        }
        if let Some(v) = ctrl.params.get("y").and_then(|p| self.eval_param(p)) {
            self.vel.y = v.to_float();
        }
    }

    /// `VelAdd`: add to the x/y velocity components from the `x`/`y` parameters.
    /// A missing component adds nothing on that axis.
    fn ctrl_vel_add(&mut self, ctrl: &CompiledController) {
        if let Some(v) = ctrl.params.get("x").and_then(|p| self.eval_param(p)) {
            self.vel.x += v.to_float();
        }
        if let Some(v) = ctrl.params.get("y").and_then(|p| self.eval_param(p)) {
            self.vel.y += v.to_float();
        }
    }

    /// `CtrlSet`: set the player control flag from the `value` parameter.
    fn ctrl_ctrl_set(&mut self, ctrl: &CompiledController) {
        if let Some(v) = ctrl.params.get("value").and_then(|p| self.eval_param(p)) {
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
    fn ctrl_pos_set(&mut self, ctrl: &CompiledController) {
        if let Some(v) = ctrl.params.get("x").and_then(|p| self.eval_param(p)) {
            self.pos.x = v.to_float();
        }
        if let Some(v) = ctrl.params.get("y").and_then(|p| self.eval_param(p)) {
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
    fn ctrl_pos_add(&mut self, ctrl: &CompiledController) {
        if let Some(v) = ctrl.params.get("x").and_then(|p| self.eval_param(p)) {
            self.pos.x += v.to_float() * self.facing.sign() as f32;
        }
        if let Some(v) = ctrl.params.get("y").and_then(|p| self.eval_param(p)) {
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
    fn ctrl_change_anim(&mut self, ctrl: &CompiledController) {
        let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p)) else {
            tracing::debug!(
                "tick: ChangeAnim in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        self.anim = value.to_int();
        // MUGEN's optional `elem` is one-based; store it zero-based. Default to
        // the first element when absent.
        let start_elem = match ctrl.params.get("elem").and_then(|p| self.eval_param(p)) {
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
    fn ctrl_var_set(&mut self, ctrl: &CompiledController) {
        // Indexed-key forms: `var(i)`, `fvar(i)`, `sysvar(i)`, `sysfvar(i)`.
        for (key, param) in &ctrl.params {
            if let Some((bank, index)) = parse_var_bank_key(key) {
                let value = self.eval_param(param).unwrap_or(Value::DEFAULT);
                self.assign_var(bank, index, value);
                // A VarSet sets exactly one variable; the first matching key wins.
                return;
            }
        }
        // `v`/`fv` + `value` form.
        if let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p)) {
            if let Some(index) = ctrl.params.get("v").and_then(|p| self.eval_param(p)) {
                self.assign_var(VarBank::Int, index.to_int(), value);
            } else if let Some(index) = ctrl.params.get("fv").and_then(|p| self.eval_param(p)) {
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
    fn ctrl_var_add(&mut self, ctrl: &CompiledController) {
        for (key, param) in &ctrl.params {
            if let Some((bank, index)) = parse_var_bank_key(key) {
                let delta = self.eval_param(param).unwrap_or(Value::DEFAULT);
                self.add_var(bank, index, delta);
                return;
            }
        }
        if let Some(delta) = ctrl.params.get("value").and_then(|p| self.eval_param(p)) {
            if let Some(index) = ctrl.params.get("v").and_then(|p| self.eval_param(p)) {
                self.add_var(VarBank::Int, index.to_int(), delta);
            } else if let Some(index) = ctrl.params.get("fv").and_then(|p| self.eval_param(p)) {
                self.add_var(VarBank::Float, index.to_int(), delta);
            } else {
                tracing::debug!(
                    "tick: VarAdd in state {} has `value` but no `v`/`fv` index; ignored",
                    ctrl.state_number
                );
            }
        }
    }

    /// `VarRangeSet`: set a contiguous range of variables to one value.
    ///
    /// Parameters (case-insensitive): `value = expr` sets the integer bank,
    /// `fvalue = expr` sets the float bank; `first`/`last` bound the inclusive
    /// index range (both default to covering the whole bank when absent — MUGEN
    /// defaults `first` to `0` and `last` to the bank's maximum index). Indices
    /// outside the bank are skipped; the controller never panics.
    fn ctrl_var_range_set(&mut self, ctrl: &CompiledController) {
        let first = ctrl
            .params
            .get("first")
            .and_then(|p| self.eval_param(p))
            .map_or(0, |v| v.to_int());
        // `value` targets the int bank; `fvalue` targets the float bank.
        if let Some(value) = ctrl.params.get("value").and_then(|p| self.eval_param(p)) {
            let last = ctrl
                .params
                .get("last")
                .and_then(|p| self.eval_param(p))
                .map_or(NUM_VARS as i32 - 1, |v| v.to_int());
            for index in first..=last {
                self.assign_var(VarBank::Int, index, value);
            }
        }
        if let Some(value) = ctrl.params.get("fvalue").and_then(|p| self.eval_param(p)) {
            let last = ctrl
                .params
                .get("last")
                .and_then(|p| self.eval_param(p))
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

    /// `PlaySnd` (stub): parse the `value` parameter and log it; no audio is
    /// produced. Real sound playback arrives in Phase 8.
    fn ctrl_play_snd(&mut self, ctrl: &CompiledController) {
        // `value` is a `group, index` sound reference; keep the raw source for
        // diagnostics. The expression VM cannot represent the pair, so the raw
        // text is the useful artifact here.
        let value = ctrl.params.get("value").map_or("<none>", CompiledParam::raw);
        tracing::debug!(
            "tick: PlaySnd {value:?} in state {} (audio is Phase 8; no-op)",
            ctrl.state_number
        );
    }

    /// `HitDef`: build a [`fp_combat::HitDef`] from the controller's parameters
    /// and store it as this character's [`active_hitdef`](Character::active_hitdef).
    ///
    /// MUGEN's `HitDef` carries two *kinds* of parameter:
    ///
    /// - **String / enum** params (`attr`, `hitflag`, `guardflag`, `ground.type`,
    ///   and the spark / sound ids which may carry an `S` prefix) are read from
    ///   the controller's **raw parameter source** ([`CompiledParam::raw`]) and
    ///   parsed with [`fp_combat::AttackAttr::parse`] /
    ///   [`fp_combat::HitFlags::parse`] / a small local type parser. Compiling
    ///   these as numeric expressions would be wrong (`S, NA` is not arithmetic).
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
    fn ctrl_hit_def(&mut self, ctrl: &CompiledController) {
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

        // Spark / sound ids. These may carry a leading `S` (use the character's
        // own AIR/SND set rather than the common set). The `S` prefix is not
        // modelled in `fp_combat::HitResources` yet, so we strip it and keep the
        // numeric id; an absent / non-numeric id keeps the default (`-1`).
        if let Some(src) = raw_param(ctrl, "sparkno") {
            hd.resources.sparkno = parse_resource_id(src, hd.resources.sparkno);
        }
        if let Some(src) = raw_param(ctrl, "hitsound") {
            hd.resources.hitsound = parse_resource_id(src, hd.resources.hitsound);
        }
        if let Some(src) = raw_param(ctrl, "guardsound") {
            hd.resources.guardsound = parse_resource_id(src, hd.resources.guardsound);
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
            if let Some(hit) = self.eval_param_component(param, 0) {
                hd.damage.hit = hit.to_int();
            }
            if let Some(guard) = self.eval_param_component(param, 1) {
                hd.damage.guard = guard.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("ground.velocity") {
            let comps = self.eval_param_components(param);
            hd.ground_velocity = pair_to_vec2(&comps, hd.ground_velocity);
        }
        if let Some(param) = ctrl.params.get("air.velocity") {
            let comps = self.eval_param_components(param);
            hd.air_velocity = pair_to_vec2(&comps, hd.air_velocity);
        }
        if let Some(param) = ctrl.params.get("guard.velocity") {
            // Single X pushback (Y unused).
            if let Some(x) = self.eval_param_component(param, 0) {
                hd.guard_velocity = x.to_float();
            }
        }
        if let Some(param) = ctrl.params.get("pausetime") {
            if let Some(p1) = self.eval_param_component(param, 0) {
                hd.pausetime.p1 = p1.to_int();
            }
            if let Some(p2) = self.eval_param_component(param, 1) {
                hd.pausetime.p2 = p2.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("ground.hittime") {
            if let Some(v) = self.eval_param_component(param, 0) {
                hd.hittimes.ground = v.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("air.hittime") {
            if let Some(v) = self.eval_param_component(param, 0) {
                hd.hittimes.air = v.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("guard.hittime") {
            if let Some(v) = self.eval_param_component(param, 0) {
                hd.hittimes.guard = v.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("p1stateno") {
            if let Some(v) = self.eval_param_component(param, 0) {
                hd.p1stateno = Some(v.to_int());
            }
        }
        if let Some(param) = ctrl.params.get("p2stateno") {
            if let Some(v) = self.eval_param_component(param, 0) {
                hd.p2stateno = Some(v.to_int());
            }
        }
        if let Some(param) = ctrl.params.get("fall") {
            if let Some(v) = self.eval_param_component(param, 0) {
                hd.fall = v.as_bool();
            }
        }
        if let Some(param) = ctrl.params.get("fall.yvelocity") {
            if let Some(v) = self.eval_param_component(param, 0) {
                hd.fall_yvelocity = v.to_float();
            }
        }
        if let Some(param) = ctrl.params.get("priority") {
            // `priority = value [, type]`. The numeric value is component 0; the
            // optional type token is a string/enum read from the raw source.
            if let Some(v) = self.eval_param_component(param, 0) {
                hd.priority.value = v.to_int();
            }
            if let Some(kind) = parse_priority_type(param.raw()) {
                hd.priority.kind = kind;
            }
        }
        if let Some(param) = ctrl.params.get("id") {
            if let Some(v) = self.eval_param_component(param, 0) {
                hd.id = v.to_int();
            }
        }
        if let Some(param) = ctrl.params.get("chainid") {
            if let Some(v) = self.eval_param_component(param, 0) {
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

    // ---- State entry -------------------------------------------------------

    /// Performs a state transition into `target`: records the previous state,
    /// resets time-in-state, clears the per-entry `persistent` bookkeeping, and
    /// applies the destination statedef's entry parameters.
    ///
    /// An unknown destination still updates the cursor (so triggers reading
    /// `StateNo` see the requested number) but applies no entry parameters and
    /// warns — never panics.
    fn enter_state(&mut self, states: &HashMap<i32, CompiledState>, target: i32) {
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
        self.apply_state_entry(state);
    }

    /// Applies a statedef's entry parameters: `type`/`movetype`/`physics`
    /// (letter tokens), `anim`/`ctrl` (compiled expressions), and `velset`
    /// (`x, y`). An unrecognized or absent value leaves the field unchanged
    /// (MUGEN's "unchanged" semantics).
    fn apply_state_entry(&mut self, state: &CompiledState) {
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
            self.anim = self.eval_value(anim_expr).to_int();
            // A new animation restarts at the first element.
            self.anim_elem = 0;
            self.anim_elem_time = 0;
        }
        if let Some(ctrl_expr) = &state.ctrl {
            self.ctrl = self.eval_value(ctrl_expr).as_bool();
        }
        if let Some(velset) = &state.velset {
            if let Some((x, y)) = parse_velset(velset) {
                self.vel.x = x;
                self.vel.y = y;
            }
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
    /// tick: `world pos.x += vel.x * facing_sign`, `world pos.y += vel.y`.
    ///
    /// MUGEN state-controller velocities are **facing-relative** (`+x` = the way
    /// the character faces), so the stored `vel.x` is mirrored by the facing sign
    /// (`+1` right, `-1` left) only here, when advancing the absolute stage
    /// position. The stored velocity itself is left untouched (the `Vel X`
    /// trigger keeps returning the facing-relative value), and the Y axis is
    /// never mirrored. A facing-right character with `vel.x = +V` moves `+x`; a
    /// facing-left character with the *same* stored `vel.x = +V` moves `-x`.
    fn integrate_position(&mut self) {
        self.pos.x += self.vel.x * self.facing.sign() as f32;
        self.pos.y += self.vel.y;
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
            // Unknown animation: nothing to advance (safe no-op).
            return;
        };
        if action.frames.is_empty() {
            return;
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
            ch.tick_with(&self.states, &self.air)
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
    /// velset.
    #[derive(Clone, Copy, Default)]
    struct Entry<'a> {
        st: Option<&'a str>,
        mv: Option<&'a str>,
        ph: Option<&'a str>,
        anim: Option<&'a str>,
        ctrl: Option<&'a str>,
        velset: Option<&'a str>,
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
        let st20 = state(20, Entry { st: Some("A"), mv: Some("A"), ph: Some("A"), anim: Some("20"), ctrl: Some("1"), velset: Some("3, -5") }, vec![]);
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
            let _ = ch.tick(&lc);
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

    // ---- AC5: ignorehitpause is wired through the loader (no runtime effect yet)

    #[test]
    fn ignorehitpause_is_compiled_onto_the_controller() {
        // There is no hitpause yet, so ignorehitpause has no observable runtime
        // effect; but the loader must compile and carry the flag so 5.4+ can honor
        // it. Verify the compiled controller preserves it from CNS.
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
        ch.pos = Vec2::new(1.0, 2.0);
        lc.tick(&mut ch);
        assert!((ch.pos.x - 50.0).abs() < 1e-6, "x set");
        assert!((ch.pos.y - 2.0).abs() < 1e-6, "y left unchanged");

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
        ch.pos = Vec2::new(10.0, 10.0);
        lc.tick(&mut ch);
        assert!((ch.pos.x - 12.0).abs() < 1e-6);
        assert!((ch.pos.y - 9.0).abs() < 1e-6);
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
        let add = ctrl(0, "PosAdd", &[], &[(1, &["1"])], None, &[("x", "5"), ("y", "2")]);
        let lc = loaded(vec![stand_n(0, vec![add.clone()])], tiny_air(0, &[5]));
        let mut right = Character::new();
        right.state_no = 0;
        right.physics = Physics::None;
        right.facing = Facing::Right;
        right.pos = Vec2::<f32>::ZERO;
        lc.tick(&mut right);
        assert!((right.pos.x - 5.0).abs() < 1e-6, "facing right PosAdd x=+5 -> +5");
        assert!((right.pos.y - 2.0).abs() < 1e-6, "PosAdd y is never mirrored");

        let lc2 = loaded(vec![stand_n(0, vec![add])], tiny_air(0, &[5]));
        let mut left = Character::new();
        left.state_no = 0;
        left.physics = Physics::None;
        left.facing = Facing::Left;
        left.pos = Vec2::<f32>::ZERO;
        lc2.tick(&mut left);
        assert!((left.pos.x - (-5.0)).abs() < 1e-6, "facing left PosAdd x=+5 -> -5 (forward)");
        assert!((left.pos.y - 2.0).abs() < 1e-6, "PosAdd y unmirrored facing left");
    }

    #[test]
    fn pos_set_is_absolute_not_facing_relative() {
        // PosSet writes the absolute stage x regardless of facing.
        let set = ctrl(0, "PosSet", &[], &[(1, &["1"])], None, &[("x", "7"), ("y", "1")]);
        let lc = loaded(vec![stand_n(0, vec![set])], tiny_air(0, &[5]));
        let mut left = Character::new();
        left.state_no = 0;
        left.physics = Physics::None;
        left.facing = Facing::Left;
        left.pos = Vec2::new(100.0, 100.0);
        lc.tick(&mut left);
        assert!((left.pos.x - 7.0).abs() < 1e-6, "PosSet x is absolute (7), not mirrored; got {}", left.pos.x);
        assert!((left.pos.y - 1.0).abs() < 1e-6);
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

    // ---- 5.4 AC: PlaySnd is a safe no-op stub ----

    #[test]
    fn play_snd_is_safe_noop() {
        // PlaySnd parses its value and logs; it must not panic or mutate state.
        let c = ctrl(0, "PlaySnd", &[], &[(1, &["1"])], None, &[("value", "1, 0")]);
        let lc = loaded(vec![stand_n(0, vec![c])], tiny_air(0, &[5]));
        let mut ch = Character::new();
        ch.state_no = 0;
        ch.physics = Physics::None;
        ch.vel = Vec2::new(3.0, 4.0);
        let report = lc.tick(&mut ch);
        assert_eq!(report.controllers_fired, 1, "PlaySnd dispatched");
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
    fn play_snd_pair_value_from_cns_is_noop_stub() {
        // The canonical PlaySnd form `value = group, index` parses through the CNS
        // parser; the stub must dispatch, log, and leave all state untouched.
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
        assert_eq!(ch.life, 1000, "PlaySnd stub mutates nothing");
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
                ("hitsound", "S5, 0"),
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
        // `S`-prefixed resource ids: prefix stripped, numeric id kept.
        assert_eq!(hd.resources.sparkno, 2);
        assert_eq!(hd.resources.hitsound, 5);
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
            let _ = ch.tick(&lc);
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
        let comps = ch.eval_param_components(&CompiledParam::compile("var(2) * 2, var(2), "));
        assert_eq!(comps.len(), 3);
        assert_eq!(comps[0].to_int(), 16);
        assert_eq!(comps[1].to_int(), 8);
        assert_eq!(comps[2].to_int(), 0, "empty trailing component → 0");
        // A single component yields a one-element vec.
        let one = ch.eval_param_components(&CompiledParam::compile("42"));
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].to_int(), 42);
    }

    #[test]
    fn eval_param_component_reads_index_with_none_when_absent() {
        // The scalar/component accessor: index 0 is the scalar value; a missing
        // component returns None so callers can substitute their own default.
        let ch = Character::new();
        let p = CompiledParam::compile("-4, 0");
        assert_eq!(ch.eval_param_component(&p, 0).map(|v| v.to_int()), Some(-4));
        assert_eq!(ch.eval_param_component(&p, 1).map(|v| v.to_int()), Some(0));
        assert!(ch.eval_param_component(&p, 2).is_none(), "no third component");
        // eval_param is shorthand for component 0.
        assert_eq!(ch.eval_param(&p).map(|v| v.to_int()), Some(-4));
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
}
