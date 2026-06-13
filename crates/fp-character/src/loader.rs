//! # Character loader (task 5.2)
//!
//! Turns a character `.def` path into a ready-to-run [`LoadedCharacter`]: the
//! compiled state graph plus all the assets and constants the per-tick executor
//! (task 5.3) needs.
//!
//! ## What loading does
//!
//! [`LoadedCharacter::load`] performs the following, all relative to the `.def`
//! file's directory (MUGEN resolves every `[Files]` reference against the `.def`
//! location):
//!
//! 1. Parse the `.def` ([`fp_formats::def::DefFile`]) and read `[Info]`
//!    (`name`, `localcoord`) and `[Files]` (`cmd`, `cns`, `st`/`st0`..`st9`,
//!    `stcommon`, `sprite`, `anim`, `sound`, `pal*`).
//! 2. Load the referenced files: the [`SffFile`] sprites, the [`AirFile`]
//!    animations, every CNS state file ([`CnsFile`]) including the common
//!    states (`common1.cns`), the [`CmdFile`] command set, and the [`SndFile`]
//!    sounds.
//! 3. **Merge** all CNS state files in MUGEN order: a statedef defined in an
//!    earlier file is never overridden by a later one, and `stcommon` is loaded
//!    **last**, filling in only the common states not already defined by the
//!    character.
//! 4. Read the character constants from the CNS `[Data]`/`[Size]`/`[Velocity]`/
//!    `[Movement]` groups (these live in the same text file as the statedefs,
//!    but the [`CnsFile`] parser intentionally drops non-state sections, so they
//!    are re-read with the generic INI parser). `[Data]` supplies
//!    `life`/`power`/`attack`/`defence`; `[Size]` supplies
//!    `ground.front`/`ground.back`/`height`; `[Velocity]` supplies
//!    `walk.fwd`/`walk.back`/`run.fwd`/`jump.neu`(+`jump.up`); `[Movement]`
//!    supplies `yaccel`/`stand.friction`/`crouch.friction`. Every other key in
//!    those groups is not read yet. The first candidate file with a `[Data]`
//!    group is the constants source.
//! 5. **Compile** every trigger expression (`triggerall` + each numbered trigger
//!    group condition) and every controller parameter via
//!    [`fp_vm::parse_str`] at load time, storing the compiled
//!    [`Expr`] alongside each controller. A bad expression stores a
//!    const-`0` [`Expr`] and is `tracing::warn!`-logged — never a panic.
//!
//! ## Never crash on bad content
//!
//! Missing **optional** files (sound, palettes, an absent `st0`..`st9` slot) are
//! `tracing::warn!`-logged and skipped. A missing **required** sprite (`sff`) or
//! state file may return [`FpError`]. Malformed expressions and
//! malformed constant values fall back to safe defaults with a warning, mirroring
//! the engine-wide rule.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use fp_core::{FpError, FpResult};
use fp_formats::air::AirFile;
use fp_formats::cmd::CmdFile;
use fp_formats::cns::{CnsFile, StateController, Statedef};
use fp_formats::def::DefFile;
use fp_formats::snd::SndFile;
use fp_formats::sff::SffFile;
use fp_vm::Expr;

use fp_core::Vec2;

use crate::{CharacterConstants, MovementConstants, SizeConstants, VelocityConstants};

/// A trigger expression compiled at load time.
///
/// Wraps the parsed [`Expr`] together with the original source text
/// for diagnostics. When an expression fails to parse, the engine substitutes a
/// constant-`0` expression (so the trigger can never fire) and records that fact
/// via [`is_fallback`](CompiledExpr::is_fallback).
#[derive(Debug, Clone)]
pub struct CompiledExpr {
    /// The compiled abstract syntax tree (a constant `0` if compilation failed).
    pub expr: Expr,
    /// The original, raw source text the expression was compiled from.
    pub source: String,
    /// `true` if the source failed to parse and `expr` is the const-`0`
    /// fallback. The executor can treat fallbacks as "never fires".
    pub is_fallback: bool,
}

impl CompiledExpr {
    /// Compiles `source` into a [`CompiledExpr`], substituting a const-`0`
    /// expression (and warning) on a parse failure.
    ///
    /// Never panics: a malformed expression yields a [`CompiledExpr`] whose
    /// [`expr`](CompiledExpr::expr) is `Expr::Int(0)` and whose
    /// [`is_fallback`](CompiledExpr::is_fallback) is `true`.
    #[must_use]
    pub fn compile(source: &str) -> Self {
        match fp_vm::parse_str(source) {
            Ok(expr) => Self {
                expr,
                source: source.to_string(),
                is_fallback: false,
            },
            Err(err) => {
                tracing::warn!(
                    "character load: bad expression {source:?} -> const 0 ({err})"
                );
                Self {
                    expr: Expr::Int(0),
                    source: source.to_string(),
                    is_fallback: true,
                }
            }
        }
    }
}

/// A trigger group whose condition expressions have been compiled.
///
/// Mirrors [`fp_formats::cns::TriggerGroup`]: the controller fires if any group
/// is fully satisfied (OR across groups), and within a group every condition is
/// AND'd. Each condition here is a [`CompiledExpr`] rather than a raw string.
#[derive(Debug, Clone)]
pub struct CompiledTriggerGroup {
    /// The group number `N` from `triggerN`.
    pub number: u32,
    /// The compiled AND'd condition expressions for this group, in file order.
    pub conditions: Vec<CompiledExpr>,
}

/// A state controller whose triggers and parameters have been compiled.
///
/// Holds the controller `type` and the original [`fp_formats::cns::StateController`]
/// so the executor still has every raw field, plus the compiled
/// [`triggerall`](CompiledController::triggerall) conditions, the compiled
/// numbered [`triggers`](CompiledController::triggers) groups, and the compiled
/// [`params`](CompiledController::params).
#[derive(Debug, Clone)]
pub struct CompiledController {
    /// The owning state number (the `N` in `[State N, label]`).
    pub state_number: i32,
    /// The free-form label after the comma in the header.
    pub label: String,
    /// The controller `type` (e.g. `HitDef`, `ChangeState`); `None` if the
    /// source block had no `type` line.
    pub controller_type: Option<String>,
    /// Compiled `triggerall` conditions — all must be true.
    pub triggerall: Vec<CompiledExpr>,
    /// Compiled numbered trigger groups, in the parser's first-seen order.
    pub triggers: Vec<CompiledTriggerGroup>,
    /// Compiled `persistent` universal parameter, if present. `1` (the MUGEN
    /// default) re-fires every qualifying tick, `0` fires once per state entry,
    /// `n` fires every `n`th qualifying tick. See the executor for the applied
    /// semantics.
    pub persistent: Option<CompiledExpr>,
    /// Compiled `ignorehitpause` universal parameter, if present. Wired through
    /// for task 5.3 (there is no hitpause yet); the executor stores the flag.
    pub ignorehitpause: Option<CompiledExpr>,
    /// Compiled controller-specific parameters, keyed by the lowercased
    /// parameter name. Each value's expression is the parsed parameter
    /// expression (const-`0` on failure).
    pub params: HashMap<String, CompiledExpr>,
}

impl CompiledController {
    /// Compiles a parsed [`StateController`] into a [`CompiledController`],
    /// compiling every trigger condition and every parameter expression.
    pub(crate) fn from_parsed(ctrl: &StateController) -> Self {
        let triggerall = ctrl
            .triggerall
            .iter()
            .map(|s| CompiledExpr::compile(s))
            .collect();

        let triggers = ctrl
            .triggers
            .iter()
            .map(|g| CompiledTriggerGroup {
                number: g.number,
                conditions: g.conditions.iter().map(|s| CompiledExpr::compile(s)).collect(),
            })
            .collect();

        let params = ctrl
            .params
            .iter()
            .map(|(k, v)| (k.clone(), CompiledExpr::compile(v)))
            .collect();

        Self {
            state_number: ctrl.state_number,
            label: ctrl.label.clone(),
            controller_type: ctrl.controller_type.clone(),
            triggerall,
            triggers,
            persistent: ctrl.persistent.as_deref().map(CompiledExpr::compile),
            ignorehitpause: ctrl.ignorehitpause.as_deref().map(CompiledExpr::compile),
            params,
        }
    }
}

/// A fully compiled state definition: the `[Statedef N]` header plus its
/// controllers with compiled triggers and parameters.
///
/// The raw header fields (`type`, `movetype`, …) are preserved verbatim from the
/// parser; the executor interprets them. The `anim` entry expression and the
/// `velset` initial velocity are also compiled where present.
#[derive(Debug, Clone)]
pub struct CompiledState {
    /// The state number.
    pub number: i32,
    /// `type` — state-type (`S`, `C`, `A`, `L`), raw.
    pub state_type: Option<String>,
    /// `movetype` — move-type (`A`, `I`, `H`), raw.
    pub movetype: Option<String>,
    /// `physics` — physics mode (`S`, `C`, `A`, `N`), raw.
    pub physics: Option<String>,
    /// `anim` — animation to switch to on entry, compiled expression.
    pub anim: Option<CompiledExpr>,
    /// `ctrl` — control flag to set on entry, compiled expression.
    pub ctrl: Option<CompiledExpr>,
    /// `velset` — raw velocity-on-entry string (`x, y`), preserved verbatim.
    pub velset: Option<String>,
    /// The controllers belonging to this state, in file order, compiled.
    pub controllers: Vec<CompiledController>,
}

impl CompiledState {
    /// Compiles a parsed [`Statedef`] into a [`CompiledState`].
    pub(crate) fn from_parsed(def: &Statedef) -> Self {
        Self {
            number: def.number,
            state_type: def.state_type.clone(),
            movetype: def.movetype.clone(),
            physics: def.physics.clone(),
            anim: def.anim.as_deref().map(CompiledExpr::compile),
            ctrl: def.ctrl.as_deref().map(CompiledExpr::compile),
            velset: def.velset.clone(),
            controllers: def.controllers.iter().map(CompiledController::from_parsed).collect(),
        }
    }
}

/// A ready-to-run loaded character: the compiled state graph plus the assets and
/// constants the executor needs.
///
/// Produced by [`LoadedCharacter::load`] from a `.def` path. Holds:
///
/// - [`name`](LoadedCharacter::name) / [`localcoord`](LoadedCharacter::localcoord)
///   from `[Info]`,
/// - [`constants`](LoadedCharacter::constants) read from the CNS
///   `[Data]`/`[Size]`/`[Velocity]`/`[Movement]` groups,
/// - the merged, compiled [`states`](LoadedCharacter::states) (MUGEN merge order;
///   `stcommon` fills in missing common states),
/// - the loaded [`sff`](LoadedCharacter::sff) sprites and
///   [`air`](LoadedCharacter::air) animations (required),
/// - the optional [`cmd`](LoadedCharacter::cmd) command set and
///   [`snd`](LoadedCharacter::snd) sounds.
///
/// The executor (task 5.3) instantiates a live [`Character`](crate::Character)
/// from a `LoadedCharacter` and steps its state machine using
/// [`states`](LoadedCharacter::states).
#[derive(Debug)]
pub struct LoadedCharacter {
    /// Display name from `[Info] name` (empty if absent).
    pub name: String,
    /// Local coordinate space `(width, height)` from `[Info] localcoord`,
    /// defaulting to MUGEN's `(320, 240)` when absent or malformed.
    pub localcoord: (i32, i32),
    /// Authored constants read from the CNS `[Data]`/`[Size]`/`[Velocity]`/
    /// `[Movement]` groups.
    pub constants: CharacterConstants,
    /// The merged, compiled state graph keyed by state number. On a number
    /// collision the **first** definition wins (earlier CNS files and the
    /// character's own states beat `stcommon`).
    pub states: HashMap<i32, CompiledState>,
    /// Loaded sprite container (required).
    pub sff: SffFile,
    /// Loaded animations (required).
    pub air: AirFile,
    /// Loaded command set, if a `cmd` file was referenced and parsed.
    pub cmd: Option<CmdFile>,
    /// Loaded sounds, if a `sound` file was referenced and parsed.
    pub snd: Option<SndFile>,
}

impl LoadedCharacter {
    /// Loads a character from its `.def` file, producing a ready-to-run
    /// [`LoadedCharacter`].
    ///
    /// All `[Files]` references are resolved relative to the `.def` directory.
    /// CNS state files are merged in MUGEN order (`st`/`st0`..`st9` first, then
    /// `stcommon` last, fill-missing only). Every trigger and controller
    /// parameter expression is compiled at load time.
    ///
    /// # Errors
    ///
    /// Returns [`FpError`] when the `.def` cannot be read or
    /// parsed, or when a **required** asset is missing or unparsable: the sprite
    /// file (`sff`), the animation file (`anim`), or when no CNS state file could
    /// be loaded at all. Missing **optional** files (sound, palettes, absent
    /// `st0`..`st9` slots) are warn-logged and skipped. Malformed expressions and
    /// constant values fall back to safe defaults; they never fail the load.
    pub fn load(def_path: impl AsRef<Path>) -> FpResult<Self> {
        let def_path = def_path.as_ref();
        tracing::info!("loading character from {}", def_path.display());

        let def = DefFile::load(def_path).map_err(|e| {
            FpError::parse("DEF", format!("failed to read {}: {e}", def_path.display()))
        })?;

        // ---- [Info] ----
        let name = def.get("Info", "name").unwrap_or("").to_string();
        let localcoord = parse_localcoord(def.get("Info", "localcoord"));

        // ---- [Files]: required assets ----
        let sprite_ref = def.get("Files", "sprite").ok_or_else(|| {
            FpError::not_found("sprite", format!("{} has no [Files] sprite", def_path.display()))
        })?;
        let sff = SffFile::load(&DefFile::resolve_path(def_path, sprite_ref))?;

        let anim_ref = def.get("Files", "anim").ok_or_else(|| {
            FpError::not_found("animation", format!("{} has no [Files] anim", def_path.display()))
        })?;
        let air = AirFile::load(&DefFile::resolve_path(def_path, anim_ref))?;

        // ---- [Files]: optional assets ----
        let cmd = load_optional(def.get("Files", "cmd"), def_path, "CMD", CmdFile::load);
        let snd = load_optional(def.get("Files", "sound"), def_path, "SND", SndFile::load);

        // ---- CNS state files in MUGEN merge order ----
        // The character's own state files come first (st, st0..st9, plus the
        // legacy `cns` slot which also carries states for older characters);
        // `stcommon` is loaded LAST and only fills in missing common states.
        let mut state_refs: Vec<String> = Vec::new();
        push_ref(&mut state_refs, def.get("Files", "st"));
        for i in 0..=9 {
            push_ref(&mut state_refs, def.get("Files", &format!("st{i}")));
        }
        // `cns` is primarily the constants file, but for many characters it is
        // also a state file (it is KFM's `st` target). Include it so its
        // statedefs participate in the merge; the first-wins rule keeps it from
        // clobbering anything already loaded.
        push_ref(&mut state_refs, def.get("Files", "cns"));

        // Merge character states first (first definition of a number wins).
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        for rel in &state_refs {
            merge_cns(&mut states, &DefFile::resolve_path(def_path, rel), rel);
        }

        // `stcommon` last: fill-missing only (handled by the same first-wins
        // merge, since the character states are already in place).
        if let Some(common_ref) = def.get("Files", "stcommon") {
            merge_cns(
                &mut states,
                &DefFile::resolve_path(def_path, common_ref),
                common_ref,
            );
        }

        if states.is_empty() {
            return Err(FpError::not_found(
                "state",
                format!("{} loaded no CNS states", def_path.display()),
            ));
        }

        // ---- Constants from the CNS [Data]/[Size]/[Velocity]/[Movement] ----
        // These groups live in the `cns` file (KFM puts them in kfm.cns). The
        // CnsFile parser drops non-state sections, so re-read them with the
        // generic INI parser. The first constants file that parses wins.
        let constants = load_constants(&def, def_path, &state_refs);

        let compiled_states = states.len();
        tracing::info!(
            "loaded character {name:?}: {compiled_states} compiled states, \
             {} sprites, {} animations",
            sff.sprites.len(),
            air.actions.len(),
        );

        Ok(Self {
            name,
            localcoord,
            constants,
            states,
            sff,
            air,
            cmd,
            snd,
        })
    }

    /// Returns the compiled state with the given number, if present.
    #[must_use]
    pub fn state(&self, number: i32) -> Option<&CompiledState> {
        self.states.get(&number)
    }

    /// Number of compiled states in the merged graph.
    #[must_use]
    pub fn state_count(&self) -> usize {
        self.states.len()
    }
}

/// Pushes a non-empty file reference onto `refs`, ignoring `None`/empty values.
fn push_ref(refs: &mut Vec<String>, value: Option<&str>) {
    if let Some(v) = value {
        let v = v.trim();
        if !v.is_empty() && !refs.iter().any(|existing| existing.eq_ignore_ascii_case(v)) {
            refs.push(v.to_string());
        }
    }
}

/// Loads an optional asset referenced by `value`, returning `None` (with a
/// warning) when the reference is absent/empty or the file fails to load.
fn load_optional<T>(
    value: Option<&str>,
    def_path: &Path,
    label: &str,
    loader: impl Fn(&Path) -> FpResult<T>,
) -> Option<T> {
    let rel = value?.trim();
    if rel.is_empty() {
        return None;
    }
    let path = DefFile::resolve_path(def_path, rel);
    match loader(&path) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("optional {label} file {} skipped: {e}", path.display());
            None
        }
    }
}

/// Loads and merges a CNS state file into `states`, applying the MUGEN
/// first-wins rule (a statedef already present is never overridden).
///
/// A missing or unparsable CNS file is warn-logged and skipped (the caller
/// errors only if *no* state file at all could be loaded).
fn merge_cns(states: &mut HashMap<i32, CompiledState>, path: &Path, rel: &str) {
    let cns = match CnsFile::load(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("CNS state file {rel} ({}) skipped: {e}", path.display());
            return;
        }
    };
    let mut added = 0usize;
    for def in &cns.statedefs {
        // First definition of a number wins: earlier files and the character's
        // own states beat later files (and stcommon).
        if let std::collections::hash_map::Entry::Vacant(slot) = states.entry(def.number) {
            slot.insert(CompiledState::from_parsed(def));
            added += 1;
        }
    }
    tracing::info!(
        "merged {added} new states from {rel} ({} statedefs in file)",
        cns.statedefs.len()
    );
}

/// Parses `[Info] localcoord` (`"320,240"`) into `(width, height)`, defaulting
/// to MUGEN's `(320, 240)` when absent or malformed.
fn parse_localcoord(value: Option<&str>) -> (i32, i32) {
    const DEFAULT: (i32, i32) = (320, 240);
    let Some(raw) = value else {
        return DEFAULT;
    };
    let mut parts = raw.split(',').map(|p| p.trim());
    let w = parts.next().and_then(|p| p.parse::<i32>().ok());
    let h = parts.next().and_then(|p| p.parse::<i32>().ok());
    match (w, h) {
        (Some(w), Some(h)) => (w, h),
        _ => {
            tracing::warn!("malformed [Info] localcoord {raw:?}; using default {DEFAULT:?}");
            DEFAULT
        }
    }
}

/// Reads the character constants from the `[Data]`/`[Size]`/`[Velocity]`/
/// `[Movement]` groups of the constants file.
///
/// MUGEN keeps these groups in the `cns` file (which is also a state file). The
/// [`CnsFile`] parser drops non-state sections, so the file is re-read with the
/// generic INI parser. Each candidate file is tried in order; the first that
/// yields a `[Data]` group is used and all four groups are read from it.
/// Missing or malformed values fall back to the
/// [`CharacterConstants::default`] for that field — a bad value never fails the
/// load.
fn load_constants(def: &DefFile, def_path: &Path, state_refs: &[String]) -> CharacterConstants {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(cns_ref) = def.get("Files", "cns") {
        let cns_ref = cns_ref.trim();
        if !cns_ref.is_empty() {
            candidates.push(DefFile::resolve_path(def_path, cns_ref));
        }
    }
    for rel in state_refs {
        candidates.push(DefFile::resolve_path(def_path, rel));
    }

    let mut consts = CharacterConstants::default();
    for path in candidates {
        let ini = match DefFile::load(&path) {
            Ok(i) => i,
            Err(_) => continue,
        };
        // Only treat a file as the constants source if it actually has [Data].
        if !ini.sections.contains_key("data") {
            continue;
        }
        read_data_group(&ini, &mut consts);
        read_size_group(&ini, &mut consts.size);
        read_velocity_group(&ini, &mut consts.velocity);
        read_movement_group(&ini, &mut consts.movement);
        tracing::info!(
            "read constants from {}: life={} attack={} defence={} power={}; \
             size(front={},back={},h={}) walk.fwd={} yaccel={}",
            path.display(),
            consts.life_max,
            consts.attack,
            consts.defence,
            consts.power_max,
            consts.size.ground_front,
            consts.size.ground_back,
            consts.size.height,
            consts.velocity.walk_fwd.x,
            consts.movement.yaccel,
        );
        return consts;
    }
    tracing::warn!("no [Data] constants group found; using MUGEN defaults");
    consts
}

/// Reads the `[Data]` group into `consts`, leaving each field at its prior value
/// when absent or malformed.
fn read_data_group(ini: &DefFile, consts: &mut CharacterConstants) {
    if let Some(v) = ini.get_parsed::<i32>("Data", "life") {
        consts.life_max = v;
    }
    if let Some(v) = ini.get_parsed::<i32>("Data", "attack") {
        consts.attack = v;
    }
    if let Some(v) = ini.get_parsed::<i32>("Data", "defence") {
        consts.defence = v;
    }
    // `power` lives in [Data] as `power` on some characters; KFM omits it and
    // MUGEN defaults to 3000. Honor an explicit value when present.
    if let Some(v) = ini.get_parsed::<i32>("Data", "power") {
        consts.power_max = v;
    }
}

/// Reads the `[Size]` group: player widths and height. Missing/malformed fields
/// keep their default.
fn read_size_group(ini: &DefFile, size: &mut SizeConstants) {
    if let Some(v) = ini.get_parsed::<i32>("Size", "ground.front") {
        size.ground_front = v;
    }
    if let Some(v) = ini.get_parsed::<i32>("Size", "ground.back") {
        size.ground_back = v;
    }
    if let Some(v) = ini.get_parsed::<i32>("Size", "height") {
        size.height = v;
    }
}

/// Reads the `[Velocity]` group: walk and jump velocities. Each entry may be a
/// bare scalar (x only, y defaults to `0`) or an `x, y` pair; missing/malformed
/// fields keep their default.
fn read_velocity_group(ini: &DefFile, vel: &mut VelocityConstants) {
    if let Some(v) = ini.get("Velocity", "walk.fwd").and_then(parse_vec2) {
        vel.walk_fwd = v;
    }
    if let Some(v) = ini.get("Velocity", "walk.back").and_then(parse_vec2) {
        vel.walk_back = v;
    }
    if let Some(v) = ini.get("Velocity", "run.fwd").and_then(parse_vec2) {
        vel.run_fwd = v;
    }
    if let Some(v) = ini.get("Velocity", "jump.neu").and_then(parse_vec2) {
        vel.jump_neu = v;
        // MUGEN derives the upward jump speed from jump.neu's y unless an
        // explicit jump.up is authored.
        vel.jump_up = v.y;
    }
    // An explicit `jump.up` (a bare upward y-velocity on some characters)
    // overrides the jump.neu-derived value.
    if let Some(v) = ini.get("Velocity", "jump.up").and_then(parse_vec2) {
        // `jump.up` is conventionally a single (y) value; treat the first parsed
        // component as the upward speed.
        vel.jump_up = v.x;
    }
}

/// Reads the `[Movement]` group: gravity and friction. Missing/malformed fields
/// keep their default.
fn read_movement_group(ini: &DefFile, mv: &mut MovementConstants) {
    if let Some(v) = ini.get_parsed::<f32>("Movement", "yaccel") {
        mv.yaccel = v;
    }
    if let Some(v) = ini.get_parsed::<f32>("Movement", "stand.friction") {
        mv.stand_friction = v;
    }
    if let Some(v) = ini.get_parsed::<f32>("Movement", "crouch.friction") {
        mv.crouch_friction = v;
    }
}

/// Parses a velocity entry that is either a bare scalar (`"2.4"` → `(2.4, 0)`)
/// or an `x, y` pair (`"0,-8.4"` → `(0, -8.4)`). Returns `None` when the first
/// component is not a valid float (a fully malformed value keeps the default).
fn parse_vec2(raw: &str) -> Option<Vec2<f32>> {
    let mut parts = raw.split(',').map(str::trim);
    let x = parts.next().and_then(|p| p.parse::<f32>().ok())?;
    let y = parts.next().and_then(|p| p.parse::<f32>().ok()).unwrap_or(0.0);
    Some(Vec2::new(x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_good_expression_is_not_fallback() {
        let c = CompiledExpr::compile("AnimElem = 3 && ctrl");
        assert!(!c.is_fallback);
        assert_eq!(c.source, "AnimElem = 3 && ctrl");
        // Not the const-0 fallback.
        assert_ne!(c.expr, Expr::Int(0));
    }

    #[test]
    fn compile_bad_expression_is_const_zero_fallback() {
        // A malformed expression (dangling operator) must compile to const 0 and
        // never panic.
        let c = CompiledExpr::compile("1 +");
        assert!(c.is_fallback);
        assert_eq!(c.expr, Expr::Int(0));
        assert_eq!(c.source, "1 +");
    }

    #[test]
    fn compile_empty_expression_is_fallback() {
        // An empty trigger value (real MUGEN content can produce these) must not
        // panic; it becomes the const-0 fallback.
        let c = CompiledExpr::compile("");
        assert!(c.is_fallback);
        assert_eq!(c.expr, Expr::Int(0));
    }

    #[test]
    fn localcoord_parsing_and_defaults() {
        assert_eq!(parse_localcoord(Some("320,240")), (320, 240));
        assert_eq!(parse_localcoord(Some(" 640 , 480 ")), (640, 480));
        // Absent → MUGEN default.
        assert_eq!(parse_localcoord(None), (320, 240));
        // Malformed → default, no panic.
        assert_eq!(parse_localcoord(Some("garbage")), (320, 240));
        assert_eq!(parse_localcoord(Some("320")), (320, 240));
    }

    #[test]
    fn push_ref_dedups_and_skips_empty() {
        let mut refs = Vec::new();
        push_ref(&mut refs, Some("kfm.cns"));
        push_ref(&mut refs, Some("KFM.CNS")); // case-insensitive dup
        push_ref(&mut refs, Some("  ")); // empty after trim
        push_ref(&mut refs, None);
        push_ref(&mut refs, Some("common1.cns"));
        assert_eq!(refs, vec!["kfm.cns".to_string(), "common1.cns".to_string()]);
    }

    #[test]
    fn merge_is_first_wins() {
        // Build two synthetic CnsFiles via from_str and merge them; the first
        // definition of a number must survive.
        let first = CnsFile::from_str("[Statedef 0]\ntype = S\nanim = 1\n").unwrap();
        let second = CnsFile::from_str("[Statedef 0]\ntype = C\nanim = 2\n").unwrap();
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        for d in &first.statedefs {
            states.insert(d.number, CompiledState::from_parsed(d));
        }
        // Emulate the fill-missing merge for the second file.
        for d in &second.statedefs {
            states.entry(d.number).or_insert_with(|| CompiledState::from_parsed(d));
        }
        // The first file's state 0 (type S) wins.
        assert_eq!(states.get(&0).unwrap().state_type.as_deref(), Some("S"));
    }

    #[test]
    fn compiled_controller_compiles_triggers_and_params() {
        let cns = CnsFile::from_str(
            "[Statedef 200]\n\
             type = S\n\
             anim = 200\n\
             ctrl = 0\n\
             \n\
             [State 200, hit]\n\
             type = HitDef\n\
             triggerall = !pause\n\
             trigger1 = AnimElem = 3\n\
             trigger2 = Time > 5\n\
             damage = 23, 0\n\
             bad = 1 +\n",
        )
        .unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        assert_eq!(state.number, 200);
        // anim/ctrl entry expressions compiled.
        assert!(state.anim.as_ref().is_some_and(|c| !c.is_fallback));
        assert!(state.ctrl.as_ref().is_some_and(|c| !c.is_fallback));
        let ctrl = &state.controllers[0];
        assert_eq!(ctrl.controller_type.as_deref(), Some("HitDef"));
        assert_eq!(ctrl.triggerall.len(), 1);
        assert!(!ctrl.triggerall[0].is_fallback);
        assert_eq!(ctrl.triggers.len(), 2);
        assert_eq!(ctrl.triggers[0].number, 1);
        assert!(!ctrl.triggers[0].conditions[0].is_fallback);
        // `damage` param compiles; `bad` (1 +) falls back to const 0.
        assert!(ctrl.params.contains_key("damage"));
        assert!(ctrl.params.get("bad").is_some_and(|c| c.is_fallback));
    }

    // ---- AC4: real-fixture load, gated to skip when test-assets is absent ----

    /// Resolves a path under the workspace's `test-assets/` directory.
    /// `CARGO_MANIFEST_DIR` points at `crates/fp-character`; go up two levels.
    fn test_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join(rel)
    }

    #[test]
    fn real_fixture_kfm_loads() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let loaded = LoadedCharacter::load(&def).expect("kfm.def should load");

        // [Info]
        assert_eq!(loaded.name, "Kung Fu Man");
        assert_eq!(loaded.localcoord, (320, 240));

        // Constants read from kfm.cns [Data].
        assert_eq!(loaded.constants.life_max, 1000);
        assert_eq!(loaded.constants.attack, 100);
        assert_eq!(loaded.constants.defence, 100);

        // Sprites and animations loaded.
        assert!(!loaded.sff.sprites.is_empty(), "kfm.sff should have sprites");
        assert!(!loaded.air.actions.is_empty(), "kfm.air should have actions");

        // >0 compiled states; the merge folded in common1.cns common states.
        assert!(loaded.state_count() > 0, "should have compiled states");
        // KFM defines [Statedef -3] (its own) and common1.cns defines the common
        // states like [Statedef 0] (stand). Both must be present after the merge.
        assert!(loaded.state(-3).is_some(), "kfm's [Statedef -3] should load");
        assert!(loaded.state(0).is_some(), "common Stand [Statedef 0] should load");

        // The optional cmd and snd files exist in the fixture and should parse.
        assert!(loaded.cmd.is_some(), "kfm.cmd should load");
        assert!(loaded.snd.is_some(), "kfm.snd should load");

        // Every compiled controller exists and its triggers are present; spot
        // check that at least one non-fallback trigger compiled somewhere.
        let any_compiled = loaded.states.values().any(|s| {
            s.controllers.iter().any(|c| {
                c.triggerall.iter().any(|e| !e.is_fallback)
                    || c.triggers
                        .iter()
                        .any(|g| g.conditions.iter().any(|e| !e.is_fallback))
            })
        });
        assert!(any_compiled, "at least one real trigger should compile");
    }

    #[test]
    fn load_missing_def_errors() {
        // A nonexistent .def must Err, not panic.
        let result = LoadedCharacter::load("/nonexistent/definitely/not/here.def");
        assert!(result.is_err());
    }

    // =====================================================================
    // Proctor (task 5.2): edge-case, error-path, and MUGEN-semantics coverage
    // for the loader, layered on top of Forge's tests. Each block is annotated
    // with the acceptance criterion it exercises. All synthetic; the only
    // file-backed test is the gated real-fixture load above.
    // =====================================================================

    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Process-unique scratch directory for synthetic on-disk fixtures, so the
    /// file-loading paths (`merge_cns`, `load_optional`, `load_constants`,
    /// `LoadedCharacter::load`) are exercised without depending on test-assets.
    /// Each call returns a fresh, empty directory; the caller cleans it up.
    fn scratch_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("fp_char_loader_{pid}_{tag}_{n}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// Writes `contents` to `dir/name`, returning the full path.
    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).expect("write scratch file");
        path
    }

    // ---- AC1: CompiledExpr stores the compiled AST and round-trips derives ----

    #[test]
    fn compiled_expr_is_clone_and_debug() {
        // CompiledExpr is part of the public surface; the executor clones it.
        let c = CompiledExpr::compile("Time > 5");
        let cloned = c.clone();
        assert_eq!(cloned.source, c.source);
        assert_eq!(cloned.is_fallback, c.is_fallback);
        assert_eq!(cloned.expr, c.expr);
        // Debug must not panic and must mention the source.
        let dbg = format!("{c:?}");
        assert!(dbg.contains("Time > 5"));
    }

    #[test]
    fn compile_whitespace_only_expression_is_fallback() {
        // Whitespace-only trigger values appear in messy content; they must fold
        // to the const-0 fallback, not panic.
        let c = CompiledExpr::compile("   \t ");
        assert!(c.is_fallback);
        assert_eq!(c.expr, Expr::Int(0));
    }

    #[test]
    fn compile_const_zero_source_is_not_a_fallback() {
        // A literal `0` is a *valid* expression that happens to equal the
        // fallback AST; is_fallback must distinguish "author wrote 0" from
        // "we substituted 0 after a parse error".
        let c = CompiledExpr::compile("0");
        assert!(!c.is_fallback, "literal 0 parses, so it is not a fallback");
        assert_eq!(c.expr, Expr::Int(0));
    }

    // ---- AC1/AC2: localcoord edge cases beyond the happy path ----

    #[test]
    fn localcoord_handles_negative_and_extra_parts() {
        // Negative coords parse as-is (the loader does not range-check; that is
        // the renderer's job) — the point is no panic and the values pass through.
        assert_eq!(parse_localcoord(Some("-320,-240")), (-320, -240));
        // Extra trailing parts are ignored once width+height are read.
        assert_eq!(parse_localcoord(Some("640,480,extra")), (640, 480));
        // Only-comma / empty fields fall back to the default.
        assert_eq!(parse_localcoord(Some(",")), (320, 240));
        assert_eq!(parse_localcoord(Some("")), (320, 240));
        // Float-looking values are not valid i32 → default.
        assert_eq!(parse_localcoord(Some("320.0,240.0")), (320, 240));
    }

    // ---- AC2: merge_cns applies the real first-wins rule across files ----

    #[test]
    fn merge_cns_first_file_wins_on_collision() {
        let dir = scratch_dir("merge_first_wins");
        // Two files both define [Statedef 0]; the FIRST merged must survive.
        let a = write_file(&dir, "a.cns", "[Statedef 0]\ntype = S\nanim = 1\n");
        let b = write_file(&dir, "b.cns", "[Statedef 0]\ntype = C\nanim = 2\n");

        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        merge_cns(&mut states, &a, "a.cns");
        merge_cns(&mut states, &b, "b.cns");

        assert_eq!(states.len(), 1);
        // First file's type S wins; b.cns is fill-missing only.
        assert_eq!(states[&0].state_type.as_deref(), Some("S"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_cns_fills_missing_states_from_later_file() {
        let dir = scratch_dir("merge_fill_missing");
        let a = write_file(&dir, "a.cns", "[Statedef 0]\ntype = S\n");
        // b.cns redefines 0 (ignored) and adds a NEW state 200 (kept).
        let b = write_file(
            &dir,
            "b.cns",
            "[Statedef 0]\ntype = C\n[Statedef 200]\ntype = A\n",
        );

        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        merge_cns(&mut states, &a, "a.cns");
        merge_cns(&mut states, &b, "b.cns");

        assert_eq!(states.len(), 2);
        assert_eq!(states[&0].state_type.as_deref(), Some("S")); // unchanged
        assert!(states.contains_key(&200)); // newly filled in
        assert_eq!(states[&200].state_type.as_deref(), Some("A"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_cns_missing_file_is_skipped_not_fatal() {
        // A missing CNS file must be warn-skipped, leaving `states` untouched and
        // never panicking (the caller errors only if NO state file loads at all).
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        merge_cns(
            &mut states,
            Path::new("/nonexistent/missing.cns"),
            "missing.cns",
        );
        assert!(states.is_empty());
    }

    #[test]
    fn merge_cns_negative_statedef_numbers_are_preserved() {
        // KFM's own logic lives in negative statedefs (-1, -2, -3); the merge must
        // key them correctly and not collide with the common states.
        let dir = scratch_dir("merge_negative");
        let a = write_file(
            &dir,
            "neg.cns",
            "[Statedef -3]\ntype = S\n[Statedef -2]\ntype = S\n[Statedef -1]\ntype = S\n",
        );
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        merge_cns(&mut states, &a, "neg.cns");
        assert!(states.contains_key(&-3));
        assert!(states.contains_key(&-2));
        assert!(states.contains_key(&-1));
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- AC2: load_optional never panics on absent / empty / unloadable refs --

    #[test]
    fn load_optional_absent_or_empty_ref_is_none() {
        let dir = scratch_dir("opt_absent");
        let def = write_file(&dir, "c.def", "[Files]\n");
        // None reference → None.
        assert!(load_optional(None, &def, "SND", SndFile::load).is_none());
        // Empty / whitespace reference → None (no attempt to load "").
        assert!(load_optional(Some(""), &def, "SND", SndFile::load).is_none());
        assert!(load_optional(Some("   "), &def, "SND", SndFile::load).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_optional_unloadable_file_is_warned_and_none() {
        let dir = scratch_dir("opt_unloadable");
        let def = write_file(&dir, "c.def", "[Files]\n");
        // A referenced-but-missing optional file → None (warn-logged), not Err.
        let got = load_optional(Some("nope.snd"), &def, "SND", SndFile::load);
        assert!(got.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- AC1: load_constants reads [Data] and falls back per-field ----

    #[test]
    fn load_constants_reads_data_group() {
        let dir = scratch_dir("consts_data");
        // .def points `cns` at the constants file.
        let def_text = "[Files]\ncns = chr.cns\n";
        let def = write_file(&dir, "chr.def", def_text);
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1200\nattack = 110\ndefence = 90\npower = 4000\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert_eq!(consts.life_max, 1200);
        assert_eq!(consts.attack, 110);
        assert_eq!(consts.defence, 90);
        assert_eq!(consts.power_max, 4000);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_partial_data_uses_defaults_for_missing_fields() {
        let dir = scratch_dir("consts_partial");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        // Only life is specified; attack/defence/power default.
        write_file(&dir, "chr.cns", "[Data]\nlife = 500\n");
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        let d = CharacterConstants::default();
        assert_eq!(consts.life_max, 500);
        assert_eq!(consts.attack, d.attack);
        assert_eq!(consts.defence, d.defence);
        assert_eq!(consts.power_max, d.power_max);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_skips_files_without_data_group() {
        let dir = scratch_dir("consts_skip");
        // cns slot has NO [Data]; the state file (in state_refs) carries [Data].
        let def = write_file(&dir, "chr.def", "[Files]\ncns = nodata.cns\n");
        write_file(&dir, "nodata.cns", "[Statedef 0]\ntype = S\n");
        write_file(&dir, "stats.cns", "[Data]\nlife = 777\n");
        let parsed = DefFile::load(&def).unwrap();
        // First candidate (nodata.cns) lacks [Data] and is skipped; the second
        // (stats.cns from state_refs) supplies the constants.
        let consts = load_constants(&parsed, &def, &["stats.cns".to_string()]);
        assert_eq!(consts.life_max, 777);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_all_defaults_when_no_data_anywhere() {
        let dir = scratch_dir("consts_none");
        let def = write_file(&dir, "chr.def", "[Files]\n");
        // No candidate file has [Data] → MUGEN defaults, no panic.
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &[]);
        assert_eq!(consts, CharacterConstants::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_malformed_value_keeps_default_for_that_field() {
        let dir = scratch_dir("consts_malformed");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        // `life` is non-numeric (messy content); that field keeps its default
        // while a sibling valid field (attack) is still read.
        write_file(&dir, "chr.cns", "[Data]\nlife = lots\nattack = 150\n");
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert_eq!(consts.life_max, CharacterConstants::default().life_max);
        assert_eq!(consts.attack, 150);
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- AC2: CompiledController preserves type, params (lowercased), gaps ----

    #[test]
    fn compiled_controller_without_type_is_none() {
        // A controller block lacking a `type` line is malformed but must not
        // crash; controller_type is None and its triggers still compile.
        let cns = CnsFile::from_str(
            "[Statedef 0]\ntype = S\n[State 0, mystery]\ntrigger1 = Time > 0\nvalue = 1\n",
        )
        .unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        let ctrl = &state.controllers[0];
        assert_eq!(ctrl.controller_type, None);
        assert_eq!(ctrl.label, "mystery");
        assert_eq!(ctrl.state_number, 0);
        assert_eq!(ctrl.triggers.len(), 1);
        assert!(!ctrl.triggers[0].conditions[0].is_fallback);
        let _ = cns;
    }

    #[test]
    fn compiled_controller_same_number_triggers_are_anded_in_one_group() {
        // Two `trigger1` lines AND together into a single group with two
        // conditions (MUGEN AND-within-group semantics), both compiled.
        let cns = CnsFile::from_str(
            "[Statedef 0]\ntype = S\n\
             [State 0, x]\ntype = Null\ntrigger1 = Time > 5\ntrigger1 = Time < 20\n",
        )
        .unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        let ctrl = &state.controllers[0];
        assert_eq!(ctrl.triggers.len(), 1, "same number → one group");
        assert_eq!(ctrl.triggers[0].number, 1);
        assert_eq!(ctrl.triggers[0].conditions.len(), 2);
        assert!(ctrl.triggers[0].conditions.iter().all(|c| !c.is_fallback));
    }

    #[test]
    fn compiled_controller_preserves_trigger_group_gap() {
        // The CNS parser preserves post-gap groups (its documented deviation);
        // the loader must carry every group through compilation untouched so the
        // executor can apply the contiguity rule later.
        let cns = CnsFile::from_str(
            "[Statedef 0]\ntype = S\n\
             [State 0, x]\ntype = Null\n\
             trigger1 = Time > 1\ntrigger2 = Time > 2\ntrigger4 = Time > 4\n",
        )
        .unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        let nums: Vec<u32> = state.controllers[0]
            .triggers
            .iter()
            .map(|g| g.number)
            .collect();
        assert_eq!(nums, vec![1, 2, 4], "post-gap group 4 must be preserved");
    }

    #[test]
    fn compiled_controller_params_are_lowercased_keys() {
        // The CNS parser lowercases parameter keys; the loader copies them as-is,
        // so lookups must use the lowercase name.
        let cns = CnsFile::from_str(
            "[Statedef 0]\ntype = S\n\
             [State 0, x]\ntype = VelSet\nTrigger1 = 1\nX = 4\nY = -2\n",
        )
        .unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        let params = &state.controllers[0].params;
        assert!(params.contains_key("x"));
        assert!(params.contains_key("y"));
        assert!(!params.contains_key("X"), "keys are lowercased");
        // The compiled value is the parsed parameter expression.
        assert!(params["x"].source.contains('4'));
    }

    #[test]
    fn compiled_state_without_anim_or_ctrl_leaves_them_none() {
        // A statedef with no `anim`/`ctrl` header line yields None for those
        // compiled fields (the executor inherits the prior value).
        let cns = CnsFile::from_str("[Statedef 5]\ntype = S\n").unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        assert_eq!(state.number, 5);
        assert!(state.anim.is_none());
        assert!(state.ctrl.is_none());
        assert!(state.velset.is_none());
        assert!(state.controllers.is_empty());
    }

    #[test]
    fn compiled_state_velset_is_preserved_verbatim() {
        let cns = CnsFile::from_str("[Statedef 0]\ntype = S\nvelset = 4, -8\n").unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        // velset is kept raw (the executor splits/interprets it).
        assert_eq!(state.velset.as_deref(), Some("4, -8"));
    }

    // ---- AC2: end-to-end CNS merge ordering via the loader's helpers ----

    #[test]
    fn stcommon_fills_missing_common_states_only() {
        // Emulate the loader's exact merge order: character state files first,
        // then stcommon LAST. A common state already authored by the character
        // must NOT be overridden by stcommon, but stcommon's other commons fill in.
        let dir = scratch_dir("stcommon_order");
        // Character's own file: defines its special states AND overrides common 0.
        let chr = write_file(
            &dir,
            "chr.cns",
            "[Statedef -3]\ntype = S\n[Statedef 0]\ntype = S\nanim = 999\n",
        );
        // stcommon: standard common states, including its own 0 (must lose to chr).
        let common = write_file(
            &dir,
            "common1.cns",
            "[Statedef 0]\ntype = S\nanim = 0\n[Statedef 20]\ntype = S\n[Statedef 40]\ntype = A\n",
        );

        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        // Character files first…
        merge_cns(&mut states, &chr, "chr.cns");
        // …stcommon last (fill-missing only).
        merge_cns(&mut states, &common, "common1.cns");

        // Character's special state survived.
        assert!(states.contains_key(&-3));
        // Common state 0 keeps the CHARACTER's anim (999), not stcommon's 0.
        assert_eq!(states[&0].anim.as_ref().unwrap().source, "999");
        // stcommon's other commons (20, 40) were filled in.
        assert!(states.contains_key(&20));
        assert!(states.contains_key(&40));
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- AC1/AC2/AC5: full LoadedCharacter::load against a SYNTHETIC, on-disk
    // character that does not require test-assets. Uses a real (tiny) SFF binary
    // built from a known-good fixture only if present; otherwise this still
    // exercises the required-file error path below. ----

    #[test]
    fn load_errors_when_sprite_missing() {
        // A .def with no [Files] sprite must Err (required asset), not panic.
        let dir = scratch_dir("load_no_sprite");
        let def = write_file(
            &dir,
            "chr.def",
            "[Info]\nname = Test\n[Files]\nanim = chr.air\ncns = chr.cns\n",
        );
        write_file(&dir, "chr.air", "[Begin Action 0]\n0,0, 0,0, 1\n");
        write_file(&dir, "chr.cns", "[Statedef 0]\ntype = S\n");
        let result = LoadedCharacter::load(&def);
        assert!(result.is_err(), "missing required sprite must Err");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_errors_when_referenced_sprite_file_absent() {
        // sprite is declared but the file does not exist on disk → Err from the
        // SFF loader, never a panic.
        let dir = scratch_dir("load_sff_absent");
        let def = write_file(
            &dir,
            "chr.def",
            "[Files]\nsprite = chr.sff\nanim = chr.air\ncns = chr.cns\n",
        );
        write_file(&dir, "chr.air", "[Begin Action 0]\n0,0, 0,0, 1\n");
        write_file(&dir, "chr.cns", "[Statedef 0]\ntype = S\n");
        // chr.sff intentionally not written.
        let result = LoadedCharacter::load(&def);
        assert!(result.is_err(), "absent referenced sprite must Err");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_errors_when_anim_missing() {
        // sprite present but no [Files] anim → required-asset Err. We reuse a real
        // SFF from test-assets when available; otherwise skip (the sprite loader
        // would error first, which is also a valid required-file failure but for a
        // different reason, so we gate to keep the assertion precise).
        let sff_src = test_asset("kfm/kfm.sff");
        if !sff_src.exists() {
            eprintln!("skipping load_errors_when_anim_missing: kfm.sff absent");
            return;
        }
        let dir = scratch_dir("load_no_anim");
        let bytes = fs::read(&sff_src).expect("read kfm.sff");
        fs::write(dir.join("chr.sff"), bytes).expect("copy sff");
        let def = write_file(
            &dir,
            "chr.def",
            "[Files]\nsprite = chr.sff\ncns = chr.cns\n",
        );
        write_file(&dir, "chr.cns", "[Statedef 0]\ntype = S\n");
        let result = LoadedCharacter::load(&def);
        assert!(result.is_err(), "missing required anim must Err");
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- AC4: full happy-path load using real SFF/AIR bytes, but a SYNTHETIC
    // tiny CNS/constants set so we exercise merge + constants + compile + the
    // optional-file warn path end-to-end. Gated on the real binary SFF/AIR being
    // present (they cannot be synthesized in-tree). ----

    #[test]
    fn load_full_pipeline_with_synthetic_states_when_binaries_present() {
        let sff_src = test_asset("kfm/kfm.sff");
        let air_src = test_asset("kfm/kfm.air");
        if !sff_src.exists() || !air_src.exists() {
            eprintln!("skipping full-pipeline load: kfm binaries absent");
            return;
        }
        let dir = scratch_dir("load_full");
        fs::write(dir.join("chr.sff"), fs::read(&sff_src).unwrap()).unwrap();
        fs::write(dir.join("chr.air"), fs::read(&air_src).unwrap()).unwrap();

        // Character's own states + constants in one cns, a separate stcommon, and
        // a referenced-but-MISSING optional sound (exercises the warn-skip path).
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1234\nattack = 105\n\
             [Statedef -3]\ntype = S\n\
             [Statedef 0]\ntype = S\nanim = 0\nctrl = 1\n\
             [State 0, walk]\ntype = ChangeState\ntrigger1 = command = \"holdfwd\"\nvalue = 20\n",
        );
        write_file(
            &dir,
            "common1.cns",
            "[Statedef 20]\ntype = S\n[Statedef 0]\ntype = C\n",
        );
        let def = write_file(
            &dir,
            "chr.def",
            "[Info]\nname = Synth Fighter\nlocalcoord = 640,480\n\
             [Files]\nsprite = chr.sff\nanim = chr.air\ncns = chr.cns\n\
             stcommon = common1.cns\nsound = missing.snd\n",
        );

        let loaded = LoadedCharacter::load(&def).expect("synthetic character should load");

        // [Info] read.
        assert_eq!(loaded.name, "Synth Fighter");
        assert_eq!(loaded.localcoord, (640, 480));
        // Constants read from [Data]; unspecified fields default.
        assert_eq!(loaded.constants.life_max, 1234);
        assert_eq!(loaded.constants.attack, 105);
        assert_eq!(loaded.constants.defence, CharacterConstants::default().defence);
        // Required assets loaded.
        assert!(!loaded.sff.sprites.is_empty());
        assert!(!loaded.air.actions.is_empty());
        // States merged: character's -3 and 0 present; stcommon filled in 20 but
        // did NOT override the character's state 0 (still type S, not C).
        assert!(loaded.state(-3).is_some());
        assert!(loaded.state(20).is_some(), "stcommon fills missing 20");
        assert_eq!(loaded.state(0).unwrap().state_type.as_deref(), Some("S"));
        assert!(loaded.state_count() >= 3);
        // The walk controller's trigger compiled (not a fallback).
        let walk = loaded
            .state(0)
            .unwrap()
            .controllers
            .iter()
            .find(|c| c.controller_type.as_deref() == Some("ChangeState"))
            .expect("walk controller present");
        assert!(!walk.triggers[0].conditions[0].is_fallback);
        assert_eq!(walk.params["value"].source, "20");
        // The missing optional sound was warn-skipped → None, no error.
        assert!(loaded.snd.is_none());
        // No cmd referenced → None.
        assert!(loaded.cmd.is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_errors_when_no_cns_states_loadable() {
        // sprite + anim present and valid, but every CNS reference is missing →
        // the loader must Err ("loaded no CNS states"), not produce an empty char.
        let sff_src = test_asset("kfm/kfm.sff");
        let air_src = test_asset("kfm/kfm.air");
        if !sff_src.exists() || !air_src.exists() {
            eprintln!("skipping no-cns-states test: kfm binaries absent");
            return;
        }
        let dir = scratch_dir("load_no_states");
        fs::write(dir.join("chr.sff"), fs::read(&sff_src).unwrap()).unwrap();
        fs::write(dir.join("chr.air"), fs::read(&air_src).unwrap()).unwrap();
        let def = write_file(
            &dir,
            "chr.def",
            "[Files]\nsprite = chr.sff\nanim = chr.air\ncns = missing.cns\n",
        );
        // missing.cns intentionally absent.
        let result = LoadedCharacter::load(&def);
        assert!(result.is_err(), "no loadable CNS states must Err");
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- AC1: accessors behave on present/absent state numbers ----

    #[test]
    fn state_accessor_and_count_via_synthetic_load() {
        // Exercise the public `state()` / `state_count()` accessors on a real
        // LoadedCharacter (private fields preclude hand-building one). Gated on the
        // binary SFF/AIR fixtures, which cannot be synthesized in-tree.
        let sff_src = test_asset("kfm/kfm.sff");
        let air_src = test_asset("kfm/kfm.air");
        if !sff_src.exists() || !air_src.exists() {
            eprintln!("skipping state-accessor test: kfm binaries absent");
            return;
        }
        let dir = scratch_dir("accessor");
        fs::write(dir.join("chr.sff"), fs::read(&sff_src).unwrap()).unwrap();
        fs::write(dir.join("chr.air"), fs::read(&air_src).unwrap()).unwrap();
        write_file(
            &dir,
            "chr.cns",
            "[Statedef 0]\ntype = S\n[Statedef 7]\ntype = S\n",
        );
        let def = write_file(
            &dir,
            "chr.def",
            "[Files]\nsprite = chr.sff\nanim = chr.air\ncns = chr.cns\n",
        );
        let loaded = LoadedCharacter::load(&def).expect("synthetic character should load");
        // Present number → Some; unknown number → None (Option contract).
        assert!(loaded.state(7).is_some());
        assert!(loaded.state(123_456).is_none());
        // state_count agrees with the map length and is non-zero.
        assert_eq!(loaded.state_count(), loaded.states.len());
        assert!(loaded.state_count() >= 2);
        let _ = fs::remove_dir_all(&dir);
    }

    // =====================================================================
    // Proctor (task 5.3 Part B): [Size]/[Velocity]/[Movement] constant-group
    // expansion. Forge added the fields and the reader functions; these tests
    // exercise them through load_constants (the loader's real entry point) plus
    // the parse_vec2 helper directly, all synthetic and on-disk-free where
    // possible.
    // =====================================================================

    // ---- AC2: [Size] group is read into SizeConstants ----

    #[test]
    fn load_constants_reads_size_group() {
        let dir = scratch_dir("consts_size");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n\
             [Size]\nground.front = 22\nground.back = 18\nheight = 70\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert_eq!(consts.size.ground_front, 22);
        assert_eq!(consts.size.ground_back, 18);
        assert_eq!(consts.size.height, 70);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_size_partial_keeps_defaults() {
        // Only ground.front is authored; the other [Size] fields keep defaults.
        let dir = scratch_dir("consts_size_partial");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(&dir, "chr.cns", "[Data]\nlife = 1000\n[Size]\nground.front = 30\n");
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        let d = SizeConstants::default();
        assert_eq!(consts.size.ground_front, 30);
        assert_eq!(consts.size.ground_back, d.ground_back);
        assert_eq!(consts.size.height, d.height);
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- AC2: [Velocity] group, including scalar vs pair and jump.up override --

    #[test]
    fn load_constants_reads_velocity_group() {
        let dir = scratch_dir("consts_velocity");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n\
             [Velocity]\nwalk.fwd = 2.4\nwalk.back = -2.2\nrun.fwd = 4.6, 0\njump.neu = 0, -8.4\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        let v = consts.velocity;
        // Scalar walk.fwd → (2.4, 0).
        assert!((v.walk_fwd.x - 2.4).abs() < 1e-6);
        assert!((v.walk_fwd.y - 0.0).abs() < 1e-6);
        assert!((v.walk_back.x - (-2.2)).abs() < 1e-6);
        // Pair run.fwd → (4.6, 0).
        assert!((v.run_fwd.x - 4.6).abs() < 1e-6);
        // jump.neu pair, and jump.up DERIVED from jump.neu.y when no explicit one.
        assert!((v.jump_neu.y - (-8.4)).abs() < 1e-6);
        assert!((v.jump_up - (-8.4)).abs() < 1e-6, "jump.up derived from jump.neu.y");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_explicit_jump_up_overrides_jump_neu() {
        // An explicit jump.up overrides the jump.neu-derived value (its first
        // parsed component is the upward speed).
        let dir = scratch_dir("consts_jumpup");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Velocity]\njump.neu = 0, -8.4\njump.up = -9.5\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        // jump.neu.y is still -8.4, but jump.up is the explicit -9.5.
        assert!((consts.velocity.jump_neu.y - (-8.4)).abs() < 1e-6);
        assert!((consts.velocity.jump_up - (-9.5)).abs() < 1e-6, "explicit jump.up wins");
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- AC2: [Movement] group: gravity + friction ----

    #[test]
    fn load_constants_reads_movement_group() {
        let dir = scratch_dir("consts_movement");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        // KFM-style leading-dot floats (.44) must parse.
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n\
             [Movement]\nyaccel = .44\nstand.friction = .85\ncrouch.friction = .82\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert!((consts.movement.yaccel - 0.44).abs() < 1e-6);
        assert!((consts.movement.stand_friction - 0.85).abs() < 1e-6);
        assert!((consts.movement.crouch_friction - 0.82).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_movement_malformed_keeps_default() {
        let dir = scratch_dir("consts_movement_bad");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        // yaccel non-numeric → keeps default; stand.friction valid → read.
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Movement]\nyaccel = fast\nstand.friction = 0.5\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        let d = MovementConstants::default();
        assert!((consts.movement.yaccel - d.yaccel).abs() < 1e-6, "bad yaccel keeps default");
        assert!((consts.movement.stand_friction - 0.5).abs() < 1e-6, "valid sibling still read");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_all_four_groups_together() {
        // A single .cns carrying all four groups is read in one pass.
        let dir = scratch_dir("consts_all_groups");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1100\nattack = 95\n\
             [Size]\nground.front = 17\nheight = 65\n\
             [Velocity]\nwalk.fwd = 2.5\n\
             [Movement]\nyaccel = .5\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert_eq!(consts.life_max, 1100);
        assert_eq!(consts.attack, 95);
        assert_eq!(consts.size.ground_front, 17);
        assert_eq!(consts.size.height, 65);
        assert!((consts.velocity.walk_fwd.x - 2.5).abs() < 1e-6);
        assert!((consts.movement.yaccel - 0.5).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- AC2: parse_vec2 helper: scalar, pair, garbage, leading-dot ----

    #[test]
    fn parse_vec2_scalar_pair_and_garbage() {
        assert_eq!(parse_vec2("2.4"), Some(Vec2::new(2.4, 0.0)));
        assert_eq!(parse_vec2("0, -8.4"), Some(Vec2::new(0.0, -8.4)));
        assert_eq!(parse_vec2(" 4.6 , 0 "), Some(Vec2::new(4.6, 0.0)));
        // Leading-dot float (KFM style).
        assert_eq!(parse_vec2(".44"), Some(Vec2::new(0.44, 0.0)));
        // Non-numeric first component → None (caller keeps default).
        assert_eq!(parse_vec2("fast"), None);
        // Non-numeric second component → y defaults to 0.
        assert_eq!(parse_vec2("3, nope"), Some(Vec2::new(3.0, 0.0)));
    }

    // ---- AC2: defaults match KFM's authored baseline values ----

    #[test]
    fn constant_group_defaults_are_kfm_baseline() {
        // The documented per-field defaults are KFM's values; assert them so a
        // future change to a default is caught and the docs stay honest.
        let s = SizeConstants::default();
        assert_eq!((s.ground_front, s.ground_back, s.height), (16, 15, 60));
        let v = VelocityConstants::default();
        assert!((v.walk_fwd.x - 2.4).abs() < 1e-6);
        assert!((v.walk_back.x - (-2.2)).abs() < 1e-6);
        assert!((v.run_fwd.x - 4.6).abs() < 1e-6);
        assert!((v.jump_neu.y - (-8.4)).abs() < 1e-6);
        assert!((v.jump_up - (-8.4)).abs() < 1e-6);
        let m = MovementConstants::default();
        assert!((m.yaccel - 0.44).abs() < 1e-6);
        assert!((m.stand_friction - 0.85).abs() < 1e-6);
        assert!((m.crouch_friction - 0.82).abs() < 1e-6);
    }

    // ---- AC5: real KFM constants read end-to-end (gated on test-assets) ----

    #[test]
    fn real_kfm_constants_all_groups() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let loaded = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: kfm.def failed to load: {e}");
                return;
            }
        };
        let c = &loaded.constants;
        // [Size]
        assert_eq!(c.size.ground_front, 16);
        assert_eq!(c.size.ground_back, 15);
        assert_eq!(c.size.height, 60);
        // [Velocity]
        assert!((c.velocity.walk_fwd.x - 2.4).abs() < 1e-4);
        assert!((c.velocity.walk_back.x - (-2.2)).abs() < 1e-4);
        assert!((c.velocity.run_fwd.x - 4.6).abs() < 1e-4);
        assert!((c.velocity.jump_neu.y - (-8.4)).abs() < 1e-4);
        // [Movement]
        assert!((c.movement.yaccel - 0.44).abs() < 1e-4);
        assert!((c.movement.stand_friction - 0.85).abs() < 1e-4);
        assert!((c.movement.crouch_friction - 0.82).abs() < 1e-4);
    }
}
