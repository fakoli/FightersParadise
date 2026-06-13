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
//!    then time-in-state and the animation element/time advance from the AIR
//!    action frame durations.
//!
//! ## Controller dispatch (this task)
//!
//! Only the controllers required for 5.3 are dispatched: `ChangeState`,
//! `VelSet`, `VelAdd`, `CtrlSet`, and `Null`. Any other controller type is a
//! safe no-op (debug-logged) and is deferred to task 5.4. The dispatch never
//! panics; a malformed parameter resolves to its safe default.
//!
//! [03 §3]: ../../../docs/knowledge-base/03-engine-architecture.md

use std::collections::HashMap;

use fp_formats::air::AirFile;
use fp_vm::{eval, EvalContext, Value};

use crate::loader::{CompiledController, CompiledExpr, CompiledState, CompiledTriggerGroup};
use crate::{Character, LoadedCharacter, MoveType, Physics, StateType};

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

        // Process the special states first, in MUGEN order, then the current
        // state. The current state number is re-read after each special state in
        // case one of them changed it via ChangeState.
        for special in [-3, -2, -1] {
            self.run_state(states, special, &mut report);
        }

        // Then the current numbered state. ChangeState within it re-enters the
        // destination in the same tick (bounded by run_current_with_transitions).
        self.run_current_with_transitions(states, &mut report);

        // ---- Per-tick physics, time, and animation advance -----------------
        self.apply_physics();
        self.advance_time();
        self.advance_animation(air);

        report
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
        // Snapshot the controllers count; if a controller changes self.state_no
        // we still finish this state's list (MUGEN runs the special states fully
        // each tick), but a ChangeState here is honored for the current state.
        let num = state.controllers.len();
        for idx in 0..num {
            // Re-fetch the state each iteration: a ChangeState may have entered a
            // new current state, but the special-state list itself is stable.
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

            // If no transition happened, we're done with the current state.
            if report.transitions == transitions_before || self.state_no == current {
                return;
            }

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
        let key = (self.state_no, idx);
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
    /// Handles only the controllers in scope for task 5.3
    /// (`ChangeState`/`VelSet`/`VelAdd`/`CtrlSet`/`Null`); every other type is a
    /// safe no-op, debug-logged and deferred to task 5.4.
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
        } else if kind.eq_ignore_ascii_case("Null") {
            // Null intentionally does nothing.
        } else {
            // Unrecognized in this task → safe no-op, deferred to 5.4.
            tracing::debug!(
                "tick: unhandled controller type {kind:?} in state {} (deferred to 5.4)",
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
        let Some(value_expr) = ctrl.params.get("value") else {
            tracing::debug!(
                "tick: ChangeState in state {} has no `value`; ignored",
                ctrl.state_number
            );
            return;
        };
        let target = self.eval_value(value_expr).to_int();
        // A self-transition still counts as a re-entry in MUGEN (resets time).
        self.enter_state(states, target);
        report.transitions += 1;

        // ChangeState's optional `ctrl` parameter overrides the statedef ctrl.
        if let Some(ctrl_expr) = ctrl.params.get("ctrl") {
            self.ctrl = self.eval_value(ctrl_expr).as_bool();
        }
    }

    /// `VelSet`: set x/y velocity components from the `x`/`y` parameters. A
    /// missing component leaves that axis unchanged.
    fn ctrl_vel_set(&mut self, ctrl: &CompiledController) {
        if let Some(expr) = ctrl.params.get("x") {
            self.vel.x = self.eval_value(expr).to_float();
        }
        if let Some(expr) = ctrl.params.get("y") {
            self.vel.y = self.eval_value(expr).to_float();
        }
    }

    /// `VelAdd`: add to the x/y velocity components from the `x`/`y` parameters.
    /// A missing component adds nothing on that axis.
    fn ctrl_vel_add(&mut self, ctrl: &CompiledController) {
        if let Some(expr) = ctrl.params.get("x") {
            self.vel.x += self.eval_value(expr).to_float();
        }
        if let Some(expr) = ctrl.params.get("y") {
            self.vel.y += self.eval_value(expr).to_float();
        }
    }

    /// `CtrlSet`: set the player control flag from the `value` parameter.
    fn ctrl_ctrl_set(&mut self, ctrl: &CompiledController) {
        if let Some(expr) = ctrl.params.get("value") {
            self.ctrl = self.eval_value(expr).as_bool();
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
    use crate::loader::{CompiledExpr, CompiledState, CompiledTriggerGroup, LoadedCharacter};
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
                .map(|(k, v)| (k.to_string(), CompiledExpr::compile(v)))
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
            params: [("x".to_string(), CompiledExpr::compile("1"))]
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
            params: [("x".to_string(), CompiledExpr::compile("1"))]
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
}
