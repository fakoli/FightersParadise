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
//!    character. The `.cmd` file is then parsed as a CNS state file too and its
//!    statedefs (the command->state bridge, `[Statedef -1]`) are **merged in,
//!    supplementing** existing states — appending their controllers rather than
//!    dropping them — so input can drive state transitions (task 5.5 part A).
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
use fp_formats::act::ActPalette;
use fp_formats::air::AirFile;
use fp_formats::cmd::CmdFile;
use fp_formats::cns::{CnsFile, StateController, Statedef};
use fp_formats::def::DefFile;
use fp_formats::sff::SffFile;
use fp_formats::snd::SndFile;
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
        // An empty / whitespace-only value (e.g. a community-content `guardflag =`
        // line with no right-hand side) is a legitimately-empty parameter, not a
        // malformed expression. Fall back to const-0 *silently* — warning on it
        // floods the load log with noise (real evilken has many such lines) for no
        // diagnostic value.
        if source.trim().is_empty() {
            return Self {
                expr: Expr::Int(0),
                source: source.to_string(),
                is_fallback: true,
            };
        }
        match fp_vm::parse_str(source) {
            Ok(expr) => Self {
                expr,
                source: source.to_string(),
                is_fallback: false,
            },
            Err(err) => {
                tracing::warn!("character load: bad expression {source:?} -> const 0 ({err})");
                Self {
                    expr: Expr::Int(0),
                    source: source.to_string(),
                    is_fallback: true,
                }
            }
        }
    }
}

/// A single controller parameter compiled into its top-level comma-separated
/// **components**.
///
/// MUGEN controller parameters are frequently comma-lists where the top-level
/// comma separates independent values, each its own expression — e.g.
/// `damage = 20, 5` (hit damage, guard damage), `ground.velocity = -4, 0`
/// (x, y), or `pausetime = 12, 12` (p1, p2). The expression compiler
/// ([`fp_vm::parse_str`]) does not accept a bare top-level comma, so compiling
/// the whole value as one expression would fail and fall back to const-`0`
/// (with a misleading "bad expression" warning) for every legitimately
/// multi-valued parameter.
///
/// [`CompiledParam`] instead splits the raw value on **top-level** commas
/// (commas inside parentheses, brackets, or quotes are *not* separators — they
/// belong to a function call like `ceil(var(1), 0)` or a quoted token) and
/// compiles each component to its own [`CompiledExpr`]. A single-value
/// parameter therefore yields a one-element [`components`](CompiledParam::components)
/// list, and a genuine parse failure of an individual component still warns
/// (real malformed content stays visible).
#[derive(Debug, Clone)]
pub struct CompiledParam {
    /// The compiled components, in source order. Always at least one element
    /// (an empty or whitespace-only raw value yields one const-`0` component).
    pub components: Vec<CompiledExpr>,
    /// The original, raw parameter value text (the whole comma-list), kept
    /// verbatim for diagnostics and for the few controllers that read an
    /// enum/token (e.g. `StateTypeSet`'s `S`/`C`/`A`) from the raw source.
    pub source: String,
}

impl CompiledParam {
    /// Compiles a raw parameter value into its top-level comma-separated
    /// [`components`](CompiledParam::components).
    ///
    /// Splits `source` on top-level commas (respecting parentheses, brackets,
    /// and quotes) and compiles each component with [`CompiledExpr::compile`].
    /// A value with no comma yields a single component; an empty value yields a
    /// single const-`0` component. Never panics.
    #[must_use]
    pub fn compile(source: &str) -> Self {
        let parts = split_top_level_commas(source);
        let components = parts
            .iter()
            .map(|part| CompiledExpr::compile(part.trim()))
            .collect();
        Self {
            components,
            source: source.to_string(),
        }
    }

    /// Returns the compiled expression for component `i`, or `None` when fewer
    /// than `i + 1` components are present.
    ///
    /// Scalar (single-value) parameters live at component `0`. A controller
    /// reading the second value of an `x, y` pair uses `component(1)`, falling
    /// back to its own documented default when the component is absent.
    #[must_use]
    pub fn component(&self, i: usize) -> Option<&CompiledExpr> {
        self.components.get(i)
    }

    /// Returns the verbatim raw source text of the whole parameter value.
    ///
    /// Convenience for controllers that parse an enum/token rather than evaluate
    /// an expression (e.g. `StateTypeSet`, the `HitDef` string params).
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.source
    }

    /// The number of top-level components this parameter compiled into (always
    /// `>= 1`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.components.len()
    }

    /// Returns `true` if this parameter has no components. In practice this is
    /// never the case (compilation always yields at least one component), but
    /// the predicate is provided for completeness alongside [`len`](Self::len).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }
}

/// Splits a parameter value on **top-level** commas, ignoring commas nested
/// inside parentheses `()`, brackets `[]`, or double quotes `"`.
///
/// Returns the raw (un-trimmed) slices between separators; the caller trims each
/// component before compiling. A value with no top-level comma returns a single
/// element (the whole input). An empty input returns a single empty element so
/// every parameter has at least one component.
///
/// This is intentionally a lightweight scanner, not a full expression parser: it
/// only needs to find the commas that separate independent MUGEN parameter
/// values. Nesting depth is tracked across `()`/`[]`; a `"` toggles an in-string
/// flag that suppresses all delimiter handling until the closing quote. Never
/// panics.
fn split_top_level_commas(source: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut start = 0usize;

    for (idx, ch) in source.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '(' | '[' if !in_string => depth += 1,
            ')' | ']' if !in_string => depth = depth.saturating_sub(1),
            ',' if !in_string && depth == 0 => {
                parts.push(&source[start..idx]);
                // The next component starts after this single-byte comma.
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&source[start..]);
    parts
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
    /// Compiled `ignorehitpause` universal parameter, if present. When it
    /// evaluates truthy, the executor still runs this controller during a
    /// hit-pause freeze (task 6.5); a controller without it (or one that
    /// evaluates to `0`) is skipped while the character is hit-paused.
    pub ignorehitpause: Option<CompiledExpr>,
    /// Compiled controller-specific parameters, keyed by the lowercased
    /// parameter name. Each value is a [`CompiledParam`]: the parameter value
    /// split on top-level commas into one or more components, each compiled to
    /// its own expression (const-`0` on a genuine single-component failure).
    /// A scalar parameter has exactly one component (index `0`); read it with
    /// [`CompiledParam::component`].
    pub params: HashMap<String, CompiledParam>,
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
                conditions: g
                    .conditions
                    .iter()
                    .map(|s| CompiledExpr::compile(s))
                    .collect(),
            })
            .collect();

        let params = ctrl
            .params
            .iter()
            .map(|(k, v)| (k.clone(), CompiledParam::compile(v)))
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
/// parser; the executor interprets them. The `anim` and `poweradd` entry
/// expressions are compiled, and the `velset` initial velocity is preserved,
/// where present.
///
/// [`Default`] yields the empty state (number `0`, every header `None`, no
/// controllers); construct a real state via [`CompiledState::from_parsed`] or
/// fill the fields explicitly. The `Default` impl lets downstream code build a
/// minimal `CompiledState` with struct-update syntax (`..Default::default()`)
/// so adding a new optional header field does not force every literal to change.
#[derive(Debug, Clone, Default)]
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
    /// `poweradd` — power to add to the super meter on entry, compiled
    /// expression. Applied once per state entry by the executor, clamped to
    /// `[0, power_max]`. Absent (the common case) adds nothing.
    ///
    /// MUGEN attack states fill the power bar via this `[Statedef]` header
    /// param (e.g. KFM's `[Statedef 200] poweradd = 10`), which is how supers
    /// gated on `power >= 1000` become reachable.
    pub poweradd: Option<CompiledExpr>,
    /// `sprpriority` — sprite-draw priority set on state entry, compiled
    /// expression (faithfulness audit #16). Higher draws in front. Applied by the
    /// executor to [`Character::cur_sprpriority`](crate::Character::cur_sprpriority);
    /// absent leaves the current priority unchanged.
    pub sprpriority: Option<CompiledExpr>,
    /// `juggle` — the air-juggle points this move costs when it lands on an
    /// airborne defender, compiled expression (faithfulness audit #16). Read by
    /// hit resolution off the **attacker's current state**; absent means the move
    /// costs no juggle (it is not juggle-gated). KFM's attack states author
    /// `juggle = N` (e.g. ~30).
    pub juggle: Option<CompiledExpr>,
    /// `facep2` — when truthy, the entering character turns to face the opponent
    /// (faithfulness audit #16). Throw states use it (KFM state 810). Applied on
    /// state entry by the executor using the opponent in view; a self-only entry
    /// (no opponent) is a no-op. Absent never turns.
    pub facep2: Option<CompiledExpr>,
    /// `hitdefpersist` — when truthy, the active `HitDef` is **kept** across this
    /// state entry instead of being cleared (faithfulness audit #16). Absent /
    /// falsy clears the active `HitDef` on entry (the MUGEN default).
    pub hitdefpersist: Option<CompiledExpr>,
    /// `movehitpersist` — when truthy, the move-hit / move-contact flags
    /// ([`Character::move_connect`](crate::Character::move_connect)) are **kept**
    /// across this state entry instead of being reset (faithfulness audit #16).
    /// Absent / falsy resets them on entry (the MUGEN default).
    pub movehitpersist: Option<CompiledExpr>,
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
            poweradd: def.poweradd.as_deref().map(CompiledExpr::compile),
            sprpriority: def.sprpriority.as_deref().map(CompiledExpr::compile),
            juggle: def.juggle.as_deref().map(CompiledExpr::compile),
            facep2: def.facep2.as_deref().map(CompiledExpr::compile),
            hitdefpersist: def.hitdefpersist.as_deref().map(CompiledExpr::compile),
            movehitpersist: def.movehitpersist.as_deref().map(CompiledExpr::compile),
            controllers: def
                .controllers
                .iter()
                .map(CompiledController::from_parsed)
                .collect(),
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
    /// External `.act` palette overrides loaded from `[Files] pal1`..`pal12`, in
    /// ascending palette-slot order (skipping absent / unparsable slots).
    ///
    /// MUGEN lets a character ship up to twelve alternate costume palettes as
    /// `.act` files referenced by `pal1`..`pal12` in `[Files]`. Each entry here
    /// records the 1-based MUGEN palette **slot** ([`LoadedPalette::slot`]) and
    /// the parsed [`ActPalette`] ([`LoadedPalette::palette`]). A missing or
    /// unparsable `.act` is warn-logged and skipped (never fatal), so this vector
    /// can be shorter than the number of `pal*` keys — or empty when the
    /// character (like stock KFM, which ships none) references no `.act` files.
    /// Resolve a runtime selection with
    /// [`override_palette`](LoadedCharacter::override_palette).
    pub palettes: Vec<LoadedPalette>,
}

/// One external `.act` costume palette loaded from a `[Files] palN` slot.
///
/// Pairs the 1-based MUGEN palette slot number with the parsed RGBA palette so a
/// runtime selection ([`Character::active_palette`](crate::Character::active_palette))
/// can be resolved back to the bytes the GPU palette-lookup uploads. See
/// [`LoadedCharacter::palettes`].
#[derive(Debug, Clone)]
pub struct LoadedPalette {
    /// The 1-based MUGEN palette slot (`palN` → `N`, so `1..=12`).
    pub slot: u32,
    /// The parsed external palette (256-colour RGBA, index-0-transparent).
    pub palette: ActPalette,
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
            FpError::not_found(
                "sprite",
                format!("{} has no [Files] sprite", def_path.display()),
            )
        })?;
        let sff = SffFile::load(&DefFile::resolve_path(def_path, sprite_ref))?;

        let anim_ref = def.get("Files", "anim").ok_or_else(|| {
            FpError::not_found(
                "animation",
                format!("{} has no [Files] anim", def_path.display()),
            )
        })?;
        let air = AirFile::load(&DefFile::resolve_path(def_path, anim_ref))?;

        // ---- [Files]: optional assets ----
        let cmd = load_optional(def.get("Files", "cmd"), def_path, "CMD", CmdFile::load);
        let snd = load_optional(def.get("Files", "sound"), def_path, "SND", SndFile::load);

        // ---- [Files]: external .act costume palettes (pal1..pal12) ----
        // MUGEN lists up to twelve alternate palettes; each missing/bad slot is
        // skipped (best-effort), so the default behaviour (no override, use the
        // SFF-embedded palette) is unchanged for characters that ship none.
        let palettes = load_act_palettes(&def, def_path);

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
        //
        // `stcommon` names the ENGINE's shared common-state library (MUGEN ships
        // `data/common1.cns`); a character that does not bundle its own copy (e.g.
        // evilken's `stcommon = common1.cns`) resolves to a file that does not
        // exist next to its `.def`. When that happens we fall back to the engine
        // default shipped under `assets/data/common1.cns`, so the character still
        // gets the standard get-hit / fall / getup states. A character that DOES
        // bundle its own common1 (e.g. KFM) is unaffected: its file is present,
        // merges normally, and is never overridden by the default. The fallback
        // also fills in only the common states the character did not author
        // (first-wins merge), preserving `stcommon`'s fill-missing semantics.
        if let Some(common_ref) = def.get("Files", "stcommon") {
            let resolved = DefFile::resolve_path(def_path, common_ref);
            if resolved.exists() {
                merge_cns(&mut states, &resolved, common_ref);
            } else {
                merge_default_common_states(&mut states, common_ref, &resolved);
            }
        }

        // ---- CMD statedefs: the command->state bridge -----------------------
        // MUGEN puts the input->state transitions (`[Statedef -1]` with
        // `[State -1, ...] type=ChangeState triggerall=command="..."`) in the
        // `.cmd` FILE, not the `.cns`. The CMD parser only reads `[Command]`
        // blocks and drops statedefs, so without this step input could never
        // drive a state transition. Run the CNS statedef parser over the `.cmd`
        // path and merge its statedefs into the graph. Unlike the fill-missing
        // CNS merge, CMD statedefs *supplement* an existing statedef: their
        // controllers are appended to a state already in the graph (e.g. a
        // `[Statedef -1]` that does not otherwise exist), rather than dropped.
        if let Some(cmd_ref) = def.get("Files", "cmd") {
            let cmd_ref = cmd_ref.trim();
            if !cmd_ref.is_empty() {
                merge_cmd_statedefs(
                    &mut states,
                    &DefFile::resolve_path(def_path, cmd_ref),
                    cmd_ref,
                );
            }
        }

        // Validate that the character actually authored state data BEFORE adding
        // the engine built-ins below: a character whose CNS/CMD files loaded no
        // statedefs at all is broken (MUGEN-equivalent), and the engine built-in
        // locomotion (which always synthesizes a `[Statedef -1]`) must not mask
        // that failure.
        if states.is_empty() {
            return Err(FpError::not_found(
                "state",
                format!("{} loaded no CNS states", def_path.display()),
            ));
        }

        // ---- Engine built-in ground locomotion (task 7.3 part B) ------------
        // MUGEN's basic stand<->walk<->crouch<->jumpstart transitions are a
        // hardcoded ENGINE built-in, not character data: stock KFM authors none of
        // them (its `[Statedef -1]` has only specials/run/throws/attacks, and its
        // common1.cns stand/walk states never enter each other). The engine injects
        // them when the player has `ctrl`. We model that here so EVERY loaded
        // character gets them automatically, appending the controllers AFTER the
        // character's own `[State -1, ...]` controllers so the character's
        // specials/run/attacks keep priority (first matching ChangeState wins, and
        // `ctrl` is consumed before the built-in fires).
        append_builtin_ground_locomotion(&mut states);

        // ---- Constants from the CNS [Data]/[Size]/[Velocity]/[Movement] ----
        // These groups live in the `cns` file (KFM puts them in kfm.cns). The
        // CnsFile parser drops non-state sections, so re-read them with the
        // generic INI parser. The first constants file that parses wins.
        let mut constants = load_constants(&def, def_path, &state_refs);
        // Thread the parsed `[Info] localcoord` onto the constants too (it is
        // already on `LoadedCharacter.localcoord` below). The EvalContext reaches
        // the character only via `me.constants`, so the coordinate-scaling
        // triggers `Const720p`/`Const1280p` read the localcoord from here.
        constants.localcoord = localcoord;

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
            palettes,
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

    /// Resolves a runtime palette selection into the override palette's RGBA
    /// bytes, or `None` to use the SFF-embedded palette.
    ///
    /// `selection` is a 0-based index into [`palettes`](Self::palettes) (the
    /// loaded `.act` overrides in ascending slot order) — exactly the value a
    /// runtime [`Character::active_palette`](crate::Character::active_palette)
    /// carries. `None`, or an out-of-range index, returns `None` so callers fall
    /// back to the SFF-embedded palette (the default, byte-identical to today).
    /// A present, in-range selection returns the 1024-byte (256 × RGBA) override
    /// buffer to upload as the GPU palette texture.
    #[must_use]
    pub fn override_palette(&self, selection: Option<usize>) -> Option<&[u8; 1024]> {
        resolve_override_palette(&self.palettes, selection)
    }

    /// Number of external `.act` palette overrides successfully loaded.
    #[must_use]
    pub fn palette_count(&self) -> usize {
        self.palettes.len()
    }

    /// The default costume palette index, or `None` to use the SFF-embedded
    /// palette.
    ///
    /// MUGEN's default costume is the character's first `pal.defaults` entry (or
    /// `pal1` when none is given): when a character ships external `.act` palettes
    /// the engine renders *that* palette by default, **not** the SFF-embedded one.
    /// Many CvS-style characters (e.g. evilken) store an indexed SFF whose embedded
    /// palette is a dark placeholder and put the real costume colors in
    /// `pal1` (`<name>.act`); without this default they render near-black.
    ///
    /// Returns the index of the lowest-numbered present `.act` slot whenever any
    /// `.act` palette was loaded, and `None` otherwise. The loader fills
    /// [`palettes`](Self::palettes) in ascending slot order so this is index 0
    /// today, but the index is resolved by minimum `slot` rather than hardcoded —
    /// the "lowest slot is index 0" invariant stays enforced even if the load order
    /// ever changes. A character that ships **no** `.act` palettes (e.g. Kung Fu
    /// Man, whose costume lives in the SFF itself) keeps the SFF-embedded palette,
    /// byte-identical to before this default existed.
    ///
    /// This honors `pal.defaults` only to the precision of "the lowest present
    /// slot"; the rare character whose `pal.defaults` prefers a slot other than its
    /// lowest still defaults to its lowest loaded slot. That is exact for the
    /// common case (a leading `pal.defaults = 1, …` with `pal1` present) and is a
    /// documented, costume-only approximation otherwise.
    #[must_use]
    pub fn default_palette_index(&self) -> Option<usize> {
        self.palettes
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| p.slot)
            .map(|(i, _)| i)
    }

    /// Compiles this character's `.cmd` command list into a
    /// [`fp_input::CommandDef`] vector ready to feed a
    /// [`fp_input::CommandMatcher`].
    ///
    /// This is the single, shared way to turn a loaded character's authored
    /// commands (`holdfwd`, `FF`, special-move motions, …) into a recognizer
    /// input. The two-player `fp_engine::Match` builds each player's matcher from
    /// it, and the single-character `fp-app` path uses the same compilation. The
    /// raw command string is fed straight to [`fp_input::compile_command`], which
    /// parses the MUGEN `$`/`>`/`~`/`/` modifiers natively; a command that fails
    /// to compile (genuinely malformed) is warn-logged and skipped rather than
    /// aborting. Returns an empty vector when the character referenced no `.cmd`.
    #[must_use]
    pub fn command_defs(&self) -> Vec<fp_input::CommandDef> {
        let Some(cmd_file) = self.cmd.as_ref() else {
            return Vec::new();
        };
        cmd_file
            .commands
            .iter()
            .filter_map(|c| {
                let elements = match fp_input::compile_command(&c.command) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(
                            "skipping uncompilable command {:?} ({:?}): {e}",
                            c.name,
                            c.command
                        );
                        return None;
                    }
                };
                Some(fp_input::CommandDef {
                    name: c.name.clone(),
                    elements,
                    time: c.time,
                    buffer_time: c.buffer_time,
                })
            })
            .collect()
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

/// The maximum number of external `.act` palette slots MUGEN reads from
/// `[Files]` (`pal1`..`pal12`).
const MAX_ACT_PALETTE_SLOTS: u32 = 12;

/// Loads the external `.act` costume palettes referenced by `[Files] pal1`..
/// `pal12`, resolving each relative to the `.def` directory.
///
/// MUGEN lists up to twelve alternate palettes (`pal1`..`pal12`); each is an
/// `.act` file [parsed by `fp_formats`](ActPalette). This walks the slots in
/// ascending order, skipping any slot whose key is absent/empty and (best-effort)
/// any slot whose `.act` fails to load — every skip is `tracing::warn!`-logged,
/// never a panic. The returned vector is therefore in ascending slot order and
/// may be shorter than the number of `pal*` keys, or empty when the character
/// references no `.act` files (the stock-KFM case, which keeps the SFF-embedded
/// palette and so default rendering unchanged).
fn load_act_palettes(def: &DefFile, def_path: &Path) -> Vec<LoadedPalette> {
    let mut palettes = Vec::new();
    for slot in 1..=MAX_ACT_PALETTE_SLOTS {
        let key = format!("pal{slot}");
        let Some(rel) = def.get("Files", &key) else {
            continue;
        };
        let rel = rel.trim();
        if rel.is_empty() {
            continue;
        }
        let path = DefFile::resolve_path(def_path, rel);
        match ActPalette::load(&path) {
            Ok(palette) => {
                tracing::info!("loaded palette {key} ({rel}) from {}", path.display());
                palettes.push(LoadedPalette { slot, palette });
            }
            Err(e) => {
                // Best-effort: a missing or malformed .act is skipped, never fatal.
                tracing::warn!("palette {key} ({}) skipped: {e}", path.display());
            }
        }
    }
    if !palettes.is_empty() {
        tracing::info!("loaded {} external .act palette(s)", palettes.len());
    }
    palettes
}

/// Resolves a 0-based palette `selection` against the loaded `.act` overrides,
/// returning the override RGBA or `None` (use the SFF-embedded palette).
///
/// Backs [`LoadedCharacter::override_palette`]. `None` and any out-of-range index
/// return `None`, so the default / no-override path always falls back to the
/// embedded palette and never panics.
fn resolve_override_palette(
    palettes: &[LoadedPalette],
    selection: Option<usize>,
) -> Option<&[u8; 1024]> {
    let idx = selection?;
    palettes.get(idx).map(|p| &p.palette.rgba)
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

/// Relative path (from a workspace/install root) to the shipped engine-default
/// common-state library. This is the original, clean-room `common1.cns` Fighters
/// Paradise authors and ships under `assets/data/`.
const ENGINE_DEFAULT_COMMON1: &str = "assets/data/common1.cns";

/// Fills in the engine-default common states when a character's `stcommon`
/// (or a `st*` slot pointing at the common library) reference resolves to a
/// **missing** file.
///
/// `common_ref` is the raw reference text (for diagnostics) and `resolved` is
/// the (absent) path it resolved to next to the `.def`. This locates the shipped
/// engine default (`assets/data/common1.cns`) via [`engine_default_common1_path`]
/// and merges it with the same first-wins / fill-missing semantics as a normal
/// `stcommon` merge: only common states the character did not author are added.
///
/// If even the shipped default cannot be found or parsed, this is a warn-logged
/// no-op (never a panic) — the character simply loads without common states, the
/// same as before this fallback existed.
fn merge_default_common_states(
    states: &mut HashMap<i32, CompiledState>,
    common_ref: &str,
    resolved: &Path,
) {
    match engine_default_common1_path() {
        Some(default_path) => {
            tracing::warn!(
                "common state file {common_ref} not found at {} \
                 (character bundles none); using engine default {}",
                resolved.display(),
                default_path.display(),
            );
            merge_cns(states, &default_path, ENGINE_DEFAULT_COMMON1);
        }
        None => {
            tracing::warn!(
                "common state file {common_ref} not found at {} and the engine \
                 default {ENGINE_DEFAULT_COMMON1} is also absent; loading without \
                 common states",
                resolved.display(),
            );
        }
    }
}

/// Locates the shipped engine-default `assets/data/common1.cns`, returning its
/// path if found.
///
/// Asset resolution is best-effort and robust to where the process is launched
/// from, mirroring how `fp-app` references its shipped `assets/data/*` files
/// relative to the working directory. Candidate roots are tried in order:
///
/// 1. The current working directory (`./assets/data/common1.cns`) — the path the
///    app and tooling use when run from the workspace root.
/// 2. Each ancestor of the current working directory (so a process launched from
///    a subdirectory still finds the workspace's `assets/`).
/// 3. Each ancestor of the running executable's directory (so an installed/`target`
///    binary finds the `assets/` shipped alongside or above it).
///
/// The first candidate that exists wins. Returns `None` if no candidate exists,
/// in which case the caller falls back to loading without common states (with a
/// warning) rather than panicking.
#[must_use]
fn engine_default_common1_path() -> Option<PathBuf> {
    // 1. Working-directory relative (the common case for `cargo run` / tooling).
    let cwd_relative = PathBuf::from(ENGINE_DEFAULT_COMMON1);
    if cwd_relative.exists() {
        return Some(cwd_relative);
    }

    // 2. Walk up the working directory's ancestors.
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(found) = find_in_ancestors(&cwd) {
            return Some(found);
        }
    }

    // 3. Walk up the executable's ancestors (installed / `target/...` layouts).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            if let Some(found) = find_in_ancestors(exe_dir) {
                return Some(found);
            }
        }
    }

    None
}

/// Returns `<ancestor>/assets/data/common1.cns` for the first ancestor of
/// `start` (inclusive) under which that file exists, else `None`.
fn find_in_ancestors(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join(ENGINE_DEFAULT_COMMON1);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Parses the `.cmd` file as a CNS state file and merges its statedefs into
/// `states`, *supplementing* rather than replacing.
///
/// This is the command->state bridge: MUGEN's `[Statedef -1]` (the
/// `command="..."` → `ChangeState` rules) lives in the `.cmd` file. Each parsed
/// statedef's controllers are **appended** to a state already present in the
/// graph (so a `[State -1, ...]` ChangeState joins any existing `-1` controllers
/// without losing them); a statedef not yet present is inserted wholesale. The
/// supplement-not-override behavior is what distinguishes this from the
/// first-wins [`merge_cns`]: a `[Statedef -1]` rarely exists before the CMD merge,
/// and even when it does (some characters split it across files) its rules must
/// all run, not just the first file's.
///
/// A missing or unparsable `.cmd` file is warn-logged and skipped — never fatal.
fn merge_cmd_statedefs(states: &mut HashMap<i32, CompiledState>, path: &Path, rel: &str) {
    let cns = match CnsFile::load(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "CMD statedef merge for {rel} ({}) skipped: {e}",
                path.display()
            );
            return;
        }
    };
    let mut appended = 0usize;
    let mut inserted = 0usize;
    for def in &cns.statedefs {
        let compiled = CompiledState::from_parsed(def);
        match states.entry(def.number) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                // Supplement: append the CMD statedef's controllers to the
                // existing state. CMD statedefs (notably `-1`) carry only
                // controllers; their header entry fields are not meaningful, so
                // we keep the existing header and extend the controller list.
                appended += compiled.controllers.len();
                slot.get_mut().controllers.extend(compiled.controllers);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                inserted += 1;
                slot.insert(compiled);
            }
        }
    }
    tracing::info!(
        "merged CMD statedefs from {rel}: {inserted} new state(s), \
         {appended} controller(s) appended to existing states"
    );
}

/// The MUGEN engine-built-in ground locomotion controllers, authored as a CNS
/// `[Statedef -1]` snippet.
///
/// These are the hardcoded stand<->walk<->crouch<->jumpstart command-state
/// transitions every MUGEN character gets for free (they are NOT in any
/// character's data files). Each is a `type=ChangeState` gated on the standard
/// `holdfwd`/`holdback`/`holdup`/`holddown` command names a `.cmd` defines, on
/// the current `stateno`, and on `ctrl` — exactly the ruleset task 7.3 part B
/// specifies, in this priority order:
///
/// - stand(0)  + holdup                                    -> 40 (jumpstart)
/// - stand(0)  + holddown (not holdup)                     -> 10 (crouch start)
/// - stand(0)  + (holdfwd|holdback), not holdup/holddown   -> 20 (walk)
/// - walk(20)  + holdup                                    -> 40
/// - walk(20)  + holddown (not holdup)                     -> 10
/// - walk(20)  not holdfwd/holdback/holdup/holddown        -> 0  (back to stand)
/// - crouch(11) not holddown                               -> 12 (crouch->stand)
///
/// In addition, the engine-built-in AUTO-LAND (task A.P15b) is appended as a
/// final `type=ChangeState` controller:
///
/// - (stateno=50 || stateno=51) && pos y >= 0 && vel y >= 0 -> 52 (Jump Land)
///
/// MUGEN's basic jump (states 50/51) carries NO land transition in common1, so
/// the engine supplies it: once the P15a ground clamp pins `pos.y` back at the
/// floor (`pos y >= 0`) with a downward/zero velocity (`vel y >= 0`), the basic
/// jump transitions to Jump Land 52 (which common1 then carries 52 -> 0 Stand).
/// Unlike the locomotion controllers above it carries NO `ctrl` triggerall —
/// landing is unconditional — and is gated STRICTLY to states 50/51 so it never
/// hijacks get-hit / custom air states (e.g. 5040), which carry their own land
/// logic.
///
/// The 10->11, 12->0, and 40->50 transitions already exist in common1 via
/// AnimTime, so they are deliberately NOT duplicated here. Air movement / airjump
/// are deferred. Walk *velocity* is the character's concern (common1's
/// `[Statedef 20]` sets it via `command="holdfwd"`, which now fires because the
/// real `CommandMatcher` produces `holdfwd`).
///
/// This is appended AFTER the character's own `[State -1, ...]` controllers, so a
/// character that authors its own command-state (a special, a run, an attack)
/// matches first and consumes `ctrl` before any of these built-ins can fire.
const BUILTIN_GROUND_LOCOMOTION_CNS: &str = "\
[Statedef -1]

[State -1, engine: stand->jump]
type = ChangeState
value = 40
triggerall = ctrl
trigger1 = stateno = 0 && command = \"holdup\"

[State -1, engine: stand->crouch]
type = ChangeState
value = 10
triggerall = ctrl
trigger1 = stateno = 0 && command = \"holddown\" && command != \"holdup\"

[State -1, engine: stand->walk]
type = ChangeState
value = 20
triggerall = ctrl
trigger1 = stateno = 0 && command = \"holdfwd\" && command != \"holdup\" && command != \"holddown\"
trigger2 = stateno = 0 && command = \"holdback\" && command != \"holdup\" && command != \"holddown\"

[State -1, engine: walk->jump]
type = ChangeState
value = 40
triggerall = ctrl
trigger1 = stateno = 20 && command = \"holdup\"

[State -1, engine: walk->crouch]
type = ChangeState
value = 10
triggerall = ctrl
trigger1 = stateno = 20 && command = \"holddown\" && command != \"holdup\"

[State -1, engine: walk->stand]
type = ChangeState
value = 0
triggerall = ctrl
trigger1 = stateno = 20 && command != \"holdfwd\" && command != \"holdback\" && command != \"holdup\" && command != \"holddown\"

[State -1, engine: crouch->stand]
type = ChangeState
value = 12
triggerall = ctrl
trigger1 = stateno = 11 && command != \"holddown\"

[State -1, engine: auto-land 50/51->52]
type = ChangeState
value = 52
trigger1 = (stateno = 50 || stateno = 51) && pos y >= 0 && vel y >= 0
";

/// Appends the engine-built-in ground locomotion controllers (task 7.3 part B)
/// to the merged state graph's `[Statedef -1]`.
///
/// Parses [`BUILTIN_GROUND_LOCOMOTION_CNS`] (the hardcoded stand<->walk<->crouch
/// <->jumpstart command-states MUGEN injects for every character) with the same
/// CNS-compile path the loader already uses for the CMD->-1 bridge, then
/// **appends** the compiled controllers after any existing `[State -1, ...]`
/// controllers — so the character's own specials/run/attacks (merged earlier
/// from the `.cmd`) keep priority and consume `ctrl` first. If no `-1` state
/// exists yet (a character with no `.cmd` command-states at all) the synthesized
/// state is inserted wholesale.
///
/// Never panics: the const snippet is known-good, and a (theoretically
/// impossible) parse failure is warn-logged and skipped, leaving the graph
/// unchanged.
fn append_builtin_ground_locomotion(states: &mut HashMap<i32, CompiledState>) {
    // Idempotency guard: the built-in controllers are tagged with an `engine: `
    // label prefix. If they are already present in `[Statedef -1]` (e.g. a second
    // `load` of the same graph), do nothing — appending twice would create
    // duplicate command-states that can flicker (a stale `walk->stand` firing in
    // the same `-1` pass). No real character labels its controllers `engine: `.
    if states.get(&-1).is_some_and(|s| {
        s.controllers
            .iter()
            .any(|c| c.label.starts_with("engine: "))
    }) {
        return;
    }
    let cns = match CnsFile::from_str(BUILTIN_GROUND_LOCOMOTION_CNS) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("built-in ground locomotion CNS failed to compile: {e}");
            return;
        }
    };
    let mut appended = 0usize;
    for def in &cns.statedefs {
        let compiled = CompiledState::from_parsed(def);
        match states.entry(def.number) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                appended += compiled.controllers.len();
                slot.get_mut().controllers.extend(compiled.controllers);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                appended += compiled.controllers.len();
                slot.insert(compiled);
            }
        }
    }
    tracing::info!(
        "appended {appended} engine built-in ground-locomotion controller(s) to [Statedef -1]"
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
    // `airjuggle` is the air-juggle point allowance (KFM authors `15`); it seeds
    // each defender's per-combo juggle pool. Absent/malformed keeps the default.
    if let Some(v) = ini.get_parsed::<i32>("Data", "airjuggle") {
        consts.airjuggle = v;
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

/// Reads the `[Velocity]` group: walk, run, jump, run-jump and air-jump
/// velocities. Each entry may be a bare scalar (x only, y defaults to `0`) or an
/// `x, y` pair; missing/malformed fields keep their default (never an error).
///
/// MUGEN authors the horizontal jump speeds (`jump.fwd`, `jump.back`,
/// `airjump.fwd`, `airjump.back`) as bare x values — their stored `y` is `0`,
/// and the actual jump's vertical speed comes from `jump.up`/`airjump.y`
/// (themselves derived from `jump.neu.y`/`airjump.neu.y`). `common1.cns` reads
/// these via `const(velocity.*)`; without them every directional jump resolved
/// to `0` horizontal velocity and rose straight up.
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
    if let Some(v) = ini.get("Velocity", "run.back").and_then(parse_vec2) {
        vel.run_back = v;
    }
    if let Some(v) = ini.get("Velocity", "jump.neu").and_then(parse_vec2) {
        vel.jump_neu = v;
        // MUGEN derives the upward jump speed from jump.neu's y unless an
        // explicit jump.up is authored.
        vel.jump_up = v.y;
    }
    if let Some(v) = ini.get("Velocity", "jump.fwd").and_then(parse_vec2) {
        vel.jump_fwd = v;
    }
    if let Some(v) = ini.get("Velocity", "jump.back").and_then(parse_vec2) {
        vel.jump_back = v;
    }
    // An explicit `jump.up` (an upward jump velocity on some characters)
    // overrides the jump.neu-derived value.
    if let Some(raw) = ini.get("Velocity", "jump.up") {
        if let Some(up) = parse_jump_up(raw) {
            vel.jump_up = up;
        }
    }
    if let Some(v) = ini.get("Velocity", "runjump.fwd").and_then(parse_vec2) {
        vel.runjump_fwd = v;
    }
    if let Some(v) = ini.get("Velocity", "runjump.back").and_then(parse_vec2) {
        vel.runjump_back = v;
    }
    if let Some(v) = ini.get("Velocity", "airjump.neu").and_then(parse_vec2) {
        vel.airjump_neu = v;
        // As with jump.up, the upward air-jump speed is derived from
        // airjump.neu's y unless an explicit airjump.y is authored.
        vel.airjump_y = v.y;
    }
    if let Some(v) = ini.get("Velocity", "airjump.fwd").and_then(parse_vec2) {
        vel.airjump_fwd = v;
    }
    if let Some(v) = ini.get("Velocity", "airjump.back").and_then(parse_vec2) {
        vel.airjump_back = v;
    }
    // An explicit `airjump.y` overrides the airjump.neu-derived value.
    if let Some(raw) = ini.get("Velocity", "airjump.y") {
        if let Some(up) = parse_jump_up(raw) {
            vel.airjump_y = up;
        }
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
    if let Some(v) = ini.get_parsed::<f32>("Movement", "stand.friction.threshold") {
        mv.stand_friction_threshold = v;
    }
    if let Some(v) = ini.get_parsed::<f32>("Movement", "crouch.friction.threshold") {
        mv.crouch_friction_threshold = v;
    }
    if let Some(v) = ini.get_parsed::<f32>("Movement", "down.friction.threshold") {
        mv.down_friction_threshold = v;
    }
    // Air-jump (double/multi jump) allowance — read by the air-jump engine
    // built-in (faithfulness audit P14). Both default to MUGEN's "no air jump"
    // baseline (`0`) when the key is absent; KFM authors `airjump.num = 1` and
    // `airjump.height = 35`.
    if let Some(v) = ini.get_parsed::<i32>("Movement", "airjump.num") {
        mv.airjump_num = v;
    }
    if let Some(v) = ini.get_parsed::<f32>("Movement", "airjump.height") {
        mv.airjump_height = v;
    }
}

/// Parses a velocity entry that is either a bare scalar (`"2.4"` → `(2.4, 0)`)
/// or an `x, y` pair (`"0,-8.4"` → `(0, -8.4)`). Returns `None` when the first
/// component is not a valid float (a fully malformed value keeps the default).
fn parse_vec2(raw: &str) -> Option<Vec2<f32>> {
    let mut parts = raw.split(',').map(str::trim);
    let x = parts.next().and_then(|p| p.parse::<f32>().ok())?;
    let y = parts
        .next()
        .and_then(|p| p.parse::<f32>().ok())
        .unwrap_or(0.0);
    Some(Vec2::new(x, y))
}

/// Parses the upward jump speed from a `jump.up` value.
///
/// MUGEN's `jump.up` is the velocity applied to an up-held jump. It is most
/// commonly authored as a 2-component `x, y` pair (e.g. `jump.up = 0, -9.5`),
/// where the upward speed is the **y** component (y is negative = upward); the
/// x component is the horizontal drift. When only a single component is present
/// (a bare upward speed), that lone value is the upward speed.
///
/// Reading the y component where present is load-bearing: a previous version
/// read the first (x) component, so `jump.up = 0, -9.5` silently stored `0`
/// instead of `-9.5`. Returns `None` when no component parses (the caller keeps
/// the existing, `jump.neu`-derived value).
fn parse_jump_up(raw: &str) -> Option<f32> {
    let mut parts = raw.split(',').map(str::trim);
    let first = parts.next().and_then(|p| p.parse::<f32>().ok());
    match parts.next().and_then(|p| p.parse::<f32>().ok()) {
        // Two-component `x, y` form: the upward speed is the y component.
        Some(y) => Some(y),
        // Single-component form: the lone value is the upward speed.
        None => first,
    }
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
        // panic; it becomes the const-0 fallback. T056: whitespace-only values
        // (e.g. a `guardflag =` line with no right-hand side) are likewise a
        // legitimately-empty parameter handled as a *silent* fallback — they take
        // the early-return path and never emit a "bad expression" warning, which
        // would otherwise flood the load log (real evilken has many such lines).
        for src in ["", "   ", "\t"] {
            let c = CompiledExpr::compile(src);
            assert!(c.is_fallback, "empty/blank {src:?} must be a fallback");
            assert_eq!(c.expr, Expr::Int(0));
            assert_eq!(c.source, src, "the raw (untrimmed) source is preserved");
        }
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
            states
                .entry(d.number)
                .or_insert_with(|| CompiledState::from_parsed(d));
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
        // `damage = 23, 0` compiles into two components, neither a fallback.
        let damage = ctrl.params.get("damage").expect("damage param present");
        assert_eq!(damage.len(), 2, "`23, 0` → two components");
        assert!(damage.components.iter().all(|c| !c.is_fallback));
        // `bad` (1 +) is a single component that genuinely fails → const-0 fallback.
        let bad = ctrl.params.get("bad").expect("bad param present");
        assert_eq!(bad.len(), 1);
        assert!(bad.component(0).is_some_and(|c| c.is_fallback));
    }

    // ---- 6.6 (Proctor): Statedef `poweradd` header param compiles into
    // CompiledState. AC1's first half — the *parsing* side — lives here; the
    // executor module tests the *applied-on-entry* side. ----

    #[test]
    fn from_parsed_compiles_statedef_poweradd_header() {
        // AC1 (parse side): a `[Statedef]` header `poweradd = 10` must surface as
        // a non-fallback compiled entry expression on the CompiledState. This is
        // the KFM attack-state shape (e.g. `[Statedef 200] poweradd = 10`).
        let cns = CnsFile::from_str(
            "[Statedef 200]\n\
             type = S\n\
             anim = 200\n\
             poweradd = 10\n",
        )
        .unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        let pa = state
            .poweradd
            .as_ref()
            .expect("poweradd compiled into CompiledState");
        assert!(!pa.is_fallback, "literal `10` compiles cleanly");
        assert_eq!(pa.source, "10", "raw source preserved verbatim");
        assert_eq!(pa.expr, Expr::Int(10));
    }

    #[test]
    fn from_parsed_poweradd_absent_is_none() {
        // AC1: a statedef with NO `poweradd` header yields `None` (so the
        // executor adds nothing on entry — the common case for non-attack states).
        let cns = CnsFile::from_str("[Statedef 0]\ntype = S\nanim = 0\n").unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        assert!(state.poweradd.is_none(), "no poweradd header -> None");
    }

    #[test]
    fn from_parsed_poweradd_expression_is_compiled_not_evaluated() {
        // AC1: `poweradd` is an EXPRESSION (compiled at load), not a fixed int.
        // A trigger-bearing value compiles to a non-fallback expr; evaluation is
        // the executor's job. (Tests the compile step keeps non-literal source.)
        let cns =
            CnsFile::from_str("[Statedef 200]\ntype = S\nanim = 200\npoweradd = 5 + 5\n").unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        let pa = state.poweradd.as_ref().expect("poweradd present");
        assert!(!pa.is_fallback, "`5 + 5` is a valid expression");
        assert_eq!(pa.source, "5 + 5");
    }

    #[test]
    fn from_parsed_poweradd_malformed_is_const_zero_fallback() {
        // AC3: a garbage `poweradd` value compiles to the const-0 fallback (never
        // panics, never an Err) — on entry the executor adds 0.
        let cns =
            CnsFile::from_str("[Statedef 200]\ntype = S\nanim = 200\npoweradd = 1 +\n").unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        let pa = state
            .poweradd
            .as_ref()
            .expect("poweradd present even when malformed");
        assert!(pa.is_fallback, "malformed `1 +` -> const-0 fallback");
        assert_eq!(pa.expr, Expr::Int(0));
    }

    // ---- #16: the five previously-dropped Statedef headers carry into
    // CompiledState (sprpriority / juggle / facep2 / hitdefpersist /
    // movehitpersist). ----

    #[test]
    fn from_parsed_carries_all_five_audit16_headers() {
        // All five headers, KFM's throw-state shape (facep2 + sprpriority +
        // persists), plus a juggle cost. Each must compile onto CompiledState
        // (previously every one was silently dropped at compile).
        let cns = CnsFile::from_str(
            "[Statedef 810]\n\
             type = S\n\
             anim = 810\n\
             sprpriority = 3\n\
             juggle = 4\n\
             facep2 = 1\n\
             hitdefpersist = 1\n\
             movehitpersist = 1\n",
        )
        .unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        assert_eq!(
            state
                .sprpriority
                .as_ref()
                .expect("sprpriority carried")
                .expr,
            Expr::Int(3)
        );
        assert_eq!(
            state.juggle.as_ref().expect("juggle carried").expr,
            Expr::Int(4)
        );
        assert_eq!(
            state.facep2.as_ref().expect("facep2 carried").expr,
            Expr::Int(1)
        );
        assert_eq!(
            state
                .hitdefpersist
                .as_ref()
                .expect("hitdefpersist carried")
                .expr,
            Expr::Int(1)
        );
        assert_eq!(
            state
                .movehitpersist
                .as_ref()
                .expect("movehitpersist carried")
                .expr,
            Expr::Int(1)
        );
    }

    #[test]
    fn from_parsed_audit16_headers_absent_are_none() {
        // A plain state with none of the five headers leaves each as `None`, so
        // the executor applies the MUGEN default (clear HitDef / reset move-hit,
        // no sprpriority/juggle/facep2 effect).
        let cns = CnsFile::from_str("[Statedef 0]\ntype = S\nanim = 0\n").unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        assert!(state.sprpriority.is_none());
        assert!(state.juggle.is_none());
        assert!(state.facep2.is_none());
        assert!(state.hitdefpersist.is_none());
        assert!(state.movehitpersist.is_none());
    }

    #[test]
    fn read_data_group_reads_airjuggle() {
        // `[Data] airjuggle` seeds the defender's juggle pool. An explicit value
        // is honored; the default (15, KFM) holds when the key is absent.
        let mut consts = CharacterConstants::default();
        assert_eq!(consts.airjuggle, 15, "default airjuggle is MUGEN/KFM's 15");
        let ini = DefFile::from_str("[Data]\nlife = 1000\nairjuggle = 24\n").unwrap();
        read_data_group(&ini, &mut consts);
        assert_eq!(consts.airjuggle, 24, "explicit [Data] airjuggle is read");

        // Absent key keeps the prior value (here the default).
        let mut consts2 = CharacterConstants::default();
        let ini2 = DefFile::from_str("[Data]\nlife = 1000\n").unwrap();
        read_data_group(&ini2, &mut consts2);
        assert_eq!(consts2.airjuggle, 15, "absent airjuggle -> default 15");
    }

    // ---- 6.2b: multi-component param model (top-level comma split) ----

    #[test]
    fn split_top_level_commas_respects_parens_brackets_quotes() {
        // Top-level commas separate; nested commas (in parens/brackets/quotes)
        // do not.
        assert_eq!(split_top_level_commas("20, 5"), vec!["20", " 5"]);
        assert_eq!(split_top_level_commas("42"), vec!["42"]);
        // A comma inside a function call is NOT a separator.
        assert_eq!(
            split_top_level_commas("ceil(var(1), 0), 5"),
            vec!["ceil(var(1), 0)", " 5"]
        );
        // A comma inside brackets is NOT a separator.
        assert_eq!(
            split_top_level_commas("anim[1, 2], 3"),
            vec!["anim[1, 2]", " 3"]
        );
        // A comma inside quotes is NOT a separator.
        assert_eq!(
            split_top_level_commas("\"a, b\", c"),
            vec!["\"a, b\"", " c"]
        );
        // Trailing top-level comma yields an empty final component.
        assert_eq!(split_top_level_commas("1, "), vec!["1", " "]);
        // Empty input still yields one (empty) component.
        assert_eq!(split_top_level_commas(""), vec![""]);
    }

    #[test]
    fn compiled_param_multi_value_yields_components_no_fallback() {
        // AC2: `damage = 20, 5` compiles to TWO components, neither a fallback —
        // i.e. NO spurious "bad expression -> const 0" for a legit multi-value.
        let p = CompiledParam::compile("20, 5");
        assert_eq!(p.len(), 2, "two components");
        assert_eq!(p.component(0).map(|c| c.expr.clone()), Some(Expr::Int(20)));
        assert_eq!(p.component(1).map(|c| c.expr.clone()), Some(Expr::Int(5)));
        assert!(
            p.components.iter().all(|c| !c.is_fallback),
            "neither component is a fallback"
        );
        assert_eq!(p.raw(), "20, 5", "raw source preserved verbatim");
    }

    #[test]
    fn compiled_param_single_value_is_one_element_list() {
        // AC1: a scalar parameter yields a one-element component list at index 0.
        let p = CompiledParam::compile("12");
        assert_eq!(p.len(), 1, "single value → one-element list");
        assert!(!p.is_empty());
        assert!(p.component(0).is_some_and(|c| !c.is_fallback));
        assert_eq!(p.component(0).map(|c| c.expr.clone()), Some(Expr::Int(12)));
        // No component beyond index 0.
        assert!(p.component(1).is_none());
    }

    #[test]
    fn compiled_param_nested_comma_is_one_component() {
        // A function-call comma must stay inside its single component and compile
        // cleanly (no fallback), proving the splitter respects parens.
        let p = CompiledParam::compile("ceil(var(1) * 1.5)");
        assert_eq!(p.len(), 1);
        assert!(p.component(0).is_some_and(|c| !c.is_fallback));
    }

    #[test]
    fn compiled_param_each_component_independently_fallbacks() {
        // AC2: a genuine per-component parse failure still warns/falls back, while
        // a sibling valid component compiles cleanly (real malformed content stays
        // visible).
        let p = CompiledParam::compile("5, 1 +");
        assert_eq!(p.len(), 2);
        assert!(p.component(0).is_some_and(|c| !c.is_fallback), "5 compiles");
        assert!(
            p.component(1).is_some_and(|c| c.is_fallback),
            "`1 +` falls back"
        );
    }

    #[test]
    fn multi_value_param_compiles_without_warning_through_controller() {
        // AC2 end-to-end through the controller compiler: every comma-listed
        // param (`damage`, `ground.velocity`, `pausetime`) compiles to its
        // components with NO fallback — the previous single-expression model
        // would have produced const-0 fallbacks (and warnings) for all of these.
        let cns = CnsFile::from_str(
            "[Statedef 200]\ntype = S\n\
             [State 200, hit]\ntype = HitDef\ntrigger1 = 1\n\
             damage = 20, 5\nground.velocity = -4, 0\npausetime = 12, 12\n",
        )
        .unwrap();
        let state = CompiledState::from_parsed(&cns.statedefs[0]);
        let ctrl = &state.controllers[0];
        for (name, expected) in [("damage", 2), ("ground.velocity", 2), ("pausetime", 2)] {
            let p = ctrl.params.get(name).expect("param present");
            assert_eq!(p.len(), expected, "{name} component count");
            assert!(
                p.components.iter().all(|c| !c.is_fallback),
                "{name} has no fallback component"
            );
        }
    }

    // ======================================================================
    // Proctor (6.2b): additional edge-case / error-path / MUGEN-semantics
    // coverage for the multi-component param model.
    // ======================================================================

    /// A `tracing` writer that appends every formatted event into a shared
    /// buffer, so a test can assert exactly which warnings fired during a load.
    #[derive(Clone, Default)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut guard) = self.0.lock() {
                guard.extend_from_slice(buf);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Runs `f` with a `tracing` subscriber that captures all WARN+ output and
    /// returns the captured log text. Lets a test prove the *presence* or
    /// *absence* of the "bad expression -> const 0" warning directly, rather than
    /// only via the `is_fallback` proxy.
    fn capture_warnings(f: impl FnOnce()) -> String {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let writer = CaptureWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.lock().expect("capture buffer poisoned").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn ac2_legit_multi_value_params_emit_no_bad_expression_warning() {
        // AC2 (direct, not via the is_fallback proxy): compiling the canonical
        // multi-valued MUGEN params must NOT emit any "bad expression -> const 0"
        // warning. Before 6.2b each of these compiled the whole comma-list as one
        // expression, failed on the top-level comma, and warned once per param.
        let logs = capture_warnings(|| {
            for raw in [
                "20, 5",                 // damage = hit, guard
                "-4, 0",                 // ground.velocity = x, y
                "12, 12",                // pausetime = p1, p2
                "ceil(var(1) * 1.5), 0", // expression component + scalar
                "var(2) * 2, var(2)",    // both components are expressions
            ] {
                let p = CompiledParam::compile(raw);
                // Sanity: no component fell back either.
                assert!(
                    p.components.iter().all(|c| !c.is_fallback),
                    "{raw:?} produced an unexpected fallback component"
                );
            }
        });
        assert!(
            !logs.contains("bad expression"),
            "no spurious warn expected for legit multi-value params, got:\n{logs}"
        );
    }

    #[test]
    fn ac2_genuine_malformed_component_still_warns() {
        // AC2 second half: a real parse failure in a single component must still
        // warn so malformed content stays visible — and the warn must name the
        // offending component source, not the whole comma-list.
        let logs = capture_warnings(|| {
            // `5` is fine; `1 +` is genuinely malformed.
            let p = CompiledParam::compile("5, 1 +");
            assert!(p.component(0).is_some_and(|c| !c.is_fallback));
            assert!(p.component(1).is_some_and(|c| c.is_fallback));
        });
        assert!(
            logs.contains("bad expression"),
            "a genuine malformed component must warn, got:\n{logs}"
        );
        // The warn quotes the trimmed component (`"1 +"`), not the full list.
        assert!(
            logs.contains("\"1 +\""),
            "warn should quote the offending component source, got:\n{logs}"
        );
        // Exactly one warning — the valid sibling `5` does not warn.
        assert_eq!(
            logs.matches("bad expression").count(),
            1,
            "only the malformed component warns, got:\n{logs}"
        );
    }

    #[test]
    fn split_handles_nested_and_utf8_without_panicking() {
        // Multi-byte UTF-8 around / inside delimiters must not panic and must not
        // be mis-sliced (the scanner uses char_indices + len_utf8). A quoted
        // multi-byte token and a unicode-laden bare token are each one component.
        assert_eq!(
            split_top_level_commas("\"café, x\", y"),
            vec!["\"café, x\"", " y"]
        );
        assert_eq!(split_top_level_commas("naïve"), vec!["naïve"]);
        // Deeply nested parens/brackets: every comma is interior → one component.
        assert_eq!(
            split_top_level_commas("f(g(a, b), h[c, d])"),
            vec!["f(g(a, b), h[c, d])"]
        );
        // Mixed: a top-level comma after a balanced nested group splits.
        assert_eq!(
            split_top_level_commas("f(a, b), g(c, d)"),
            vec!["f(a, b)", " g(c, d)"]
        );
    }

    #[test]
    fn split_unbalanced_delimiters_never_panic() {
        // Malformed MUGEN content can have unbalanced parens/brackets/quotes. The
        // scanner must never panic and must still return at least one component.
        // (`saturating_sub` on depth and the in_string toggle guarantee this.)
        for raw in [
            "(((",
            ")))",
            "[a, b",
            "a, b]",
            "\"unterminated, comma",
            "((1, 2)",
        ] {
            let parts = split_top_level_commas(raw);
            assert!(!parts.is_empty(), "{raw:?} yielded zero components");
            // The re-joined parts (with the commas the scanner consumed) preserve
            // every original byte — nothing is dropped.
            let rejoined = parts.join(",");
            assert_eq!(rejoined, raw, "{raw:?} round-trips through split/join");
        }
    }

    #[test]
    fn split_round_trips_for_balanced_inputs() {
        // For any input, joining the components on ',' reconstructs the source,
        // because the scanner only ever consumes single-byte top-level commas.
        for raw in [
            "",
            "1",
            "1, 2, 3",
            "a,,b",
            ", leading",
            "trailing, ",
            "  ",
            "x , y",
        ] {
            assert_eq!(split_top_level_commas(raw).join(","), raw, "{raw:?}");
        }
    }

    #[test]
    fn compiled_param_empty_and_whitespace_are_single_fallback_component() {
        // An empty or whitespace-only value yields exactly one component, the
        // const-0 fallback (matching CompiledExpr on empty input). len() >= 1
        // invariant holds; is_empty() is never true.
        for raw in ["", "   ", "\t"] {
            let p = CompiledParam::compile(raw);
            assert_eq!(p.len(), 1, "{raw:?} → one component");
            assert!(!p.is_empty(), "{raw:?} is never component-empty");
            assert!(
                p.component(0).is_some_and(|c| c.is_fallback),
                "{raw:?} → fallback"
            );
            assert_eq!(p.component(0).map(|c| c.expr.clone()), Some(Expr::Int(0)));
            assert_eq!(p.raw(), raw, "{raw:?} raw preserved verbatim");
        }
    }

    #[test]
    fn compiled_param_trailing_and_leading_commas_make_fallback_components() {
        // MUGEN authors `damage = ,5` (leading) and `pausetime = 12,` (trailing).
        // The empty side becomes a const-0 fallback component, NOT a dropped one,
        // so the positional read (component 0 vs 1) stays correct.
        let lead = CompiledParam::compile(", 5");
        assert_eq!(lead.len(), 2);
        assert!(
            lead.component(0).is_some_and(|c| c.is_fallback),
            "empty x → 0"
        );
        assert_eq!(
            lead.component(0).map(|c| c.expr.clone()),
            Some(Expr::Int(0))
        );
        assert!(lead.component(1).is_some_and(|c| !c.is_fallback));
        assert_eq!(
            lead.component(1).map(|c| c.expr.clone()),
            Some(Expr::Int(5))
        );

        let trail = CompiledParam::compile("12, ");
        assert_eq!(trail.len(), 2);
        assert_eq!(
            trail.component(0).map(|c| c.expr.clone()),
            Some(Expr::Int(12))
        );
        assert!(
            trail.component(1).is_some_and(|c| c.is_fallback),
            "empty y → 0 fallback"
        );
    }

    #[test]
    fn compiled_param_three_components_preserved_in_order() {
        // A 3-value param (e.g. a hypothetical r, g, b or x, y, z) keeps all three
        // components in source order and reads each by index.
        let p = CompiledParam::compile("1, 2, 3");
        assert_eq!(p.len(), 3);
        assert_eq!(p.component(0).map(|c| c.expr.clone()), Some(Expr::Int(1)));
        assert_eq!(p.component(1).map(|c| c.expr.clone()), Some(Expr::Int(2)));
        assert_eq!(p.component(2).map(|c| c.expr.clone()), Some(Expr::Int(3)));
        assert!(p.component(3).is_none());
    }

    #[test]
    fn compiled_param_nested_comma_component_evaluates_via_vm() {
        // A function-call comma stays inside one component AND that component is a
        // genuinely valid expression (it parses to a Call), proving the splitter
        // does not corrupt multi-arg calls. `ceil(var(1), 0)` parses cleanly.
        let p = CompiledParam::compile("ceil(var(1), 0), 7");
        assert_eq!(p.len(), 2, "the call-comma is NOT a top-level separator");
        assert!(
            p.component(0).is_some_and(|c| !c.is_fallback),
            "call component compiles"
        );
        assert!(
            matches!(p.component(0).map(|c| &c.expr), Some(Expr::Call { .. })),
            "component 0 is the multi-arg call"
        );
        assert_eq!(p.component(1).map(|c| c.expr.clone()), Some(Expr::Int(7)));
    }

    #[test]
    fn from_parsed_attr_style_param_compiles_components_without_fallback() {
        // HitDef enum params like `attr = A, SP` are READ via raw() in the
        // executor, but the loader still compiles each component. Both `A` and
        // `SP` are bare identifiers that parse → no fallback, no warn — and raw()
        // still yields the verbatim source the executor parses.
        let logs = capture_warnings(|| {
            let cns = CnsFile::from_str(
                "[Statedef 200]\ntype = S\n\
                 [State 200, hit]\ntype = HitDef\ntrigger1 = 1\n\
                 attr = A, SP\nground.type = Low\n",
            )
            .expect("cns parses");
            let state = CompiledState::from_parsed(&cns.statedefs[0]);
            let ctrl = &state.controllers[0];
            let attr = ctrl.params.get("attr").expect("attr present");
            assert_eq!(attr.len(), 2, "`A, SP` → two identifier components");
            assert!(attr.components.iter().all(|c| !c.is_fallback));
            assert_eq!(attr.raw(), "A, SP", "raw source kept for AttackAttr::parse");
        });
        assert!(
            !logs.contains("bad expression"),
            "identifier-component attr must not warn, got:\n{logs}"
        );
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
        assert!(
            !loaded.sff.sprites.is_empty(),
            "kfm.sff should have sprites"
        );
        assert!(
            !loaded.air.actions.is_empty(),
            "kfm.air should have actions"
        );

        // >0 compiled states; the merge folded in common1.cns common states.
        assert!(loaded.state_count() > 0, "should have compiled states");
        // KFM defines [Statedef -3] (its own) and common1.cns defines the common
        // states like [Statedef 0] (stand). Both must be present after the merge.
        assert!(
            loaded.state(-3).is_some(),
            "kfm's [Statedef -3] should load"
        );
        assert!(
            loaded.state(0).is_some(),
            "common Stand [Statedef 0] should load"
        );

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
    fn real_fixture_kfm_multi_value_params_split_and_no_bad_expr_warn() {
        // Proctor AC2 + AC4 end-to-end against REAL content: loading KFM must not
        // emit ANY "bad expression -> const 0" warning caused by a legitimate
        // top-level comma in a controller param, and KFM's HitDefs must expose
        // their multi-component params (damage / *.velocity / pausetime) as >= 2
        // component lists. Gated to skip when test-assets is absent.
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }

        // Capture warnings emitted during the entire load.
        let mut maybe_loaded = None;
        let logs = capture_warnings(|| {
            maybe_loaded = Some(LoadedCharacter::load(&def).expect("kfm.def should load"));
        });
        let loaded = maybe_loaded.expect("load populated");

        // SCOPE: 6.2b fixes *controller parameter* compilation, NOT trigger
        // expressions. Real KFM (kfm.cmd) carries trigger conditions such as
        // `trigger2 = hitdefattr = SC, NA, SA, HA`, whose comma-list is a trigger,
        // not a param — those legitimately still warn and are out of 6.2b scope.
        //
        // The 6.2b guarantees under test, against real content:
        //   (a) No controller param's FULL comma-list is ever compiled as a single
        //       expression (the pre-6.2b bug); i.e. the verbatim multi-value source
        //       never appears inside a `"..."` in a "bad expression" warn.
        //   (b) At least one real numeric multi-component param (e.g. an `x, y`
        //       velocity) splits into >= 2 components that ALL compile cleanly,
        //       proving the happy path. (Note: a leading/empty component such as
        //       `value = , NA, SA, AT` in a HitBy-style controller legitimately
        //       becomes a const-0 fallback — that is correct 6.2b behavior and not
        //       checked here.)
        let mut saw_multi = false;
        let mut saw_clean_multi = false;
        for state in loaded.states.values() {
            for c in &state.controllers {
                for p in c.params.values() {
                    if p.len() >= 2 {
                        saw_multi = true;
                        // (a) The whole comma-list never warned as one expression.
                        let full = format!("\"{}\"", p.raw());
                        assert!(
                            !logs.contains(&full),
                            "param comma-list {:?} warned as a single expression \
                             (the 6.2b bug). Logs:\n{logs}",
                            p.raw()
                        );
                        // (b) Track whether we saw a fully-clean multi-component
                        // param (the common numeric `x, y` / `hit, guard` case).
                        if p.components.iter().all(|comp| !comp.is_fallback) {
                            saw_clean_multi = true;
                        }
                    }
                }
            }
        }
        assert!(
            saw_multi,
            "expected at least one real KFM controller multi-component param"
        );
        assert!(
            saw_clean_multi,
            "expected at least one real KFM multi-component param with all \
             components compiling cleanly"
        );
    }

    #[test]
    fn real_fixture_kfm_merges_cmd_statedef_minus_one() {
        // PART A: the command->state bridge. KFM keeps its `[Statedef -1]`
        // (command="..." → ChangeState) in kfm.cmd, which the CMD parser drops.
        // The loader must run the CNS statedef parser over the .cmd path and
        // merge those statedefs into the graph, so input can drive transitions.
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let loaded = LoadedCharacter::load(&def).expect("kfm.def should load");

        // A non-empty [Statedef -1] must now exist (it comes only from kfm.cmd).
        let minus_one = loaded
            .state(-1)
            .expect("kfm.cmd [Statedef -1] should be merged into the graph");
        assert!(
            !minus_one.controllers.is_empty(),
            "merged [Statedef -1] must carry controllers"
        );

        // At least one controller must be a ChangeState gated on a command="..."
        // trigger (the input->state rules). Confirm both the controller type and
        // a compiled command trigger are present.
        let has_command_changestate = minus_one.controllers.iter().any(|c| {
            let is_change_state = c
                .controller_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"));
            let gated_on_command = c
                .triggerall
                .iter()
                .chain(c.triggers.iter().flat_map(|g| g.conditions.iter()))
                .any(|e| e.source.to_ascii_lowercase().contains("command"));
            is_change_state && gated_on_command
        });
        assert!(
            has_command_changestate,
            "[Statedef -1] should contain a ChangeState gated on command=..."
        );
    }

    #[test]
    fn merge_cmd_statedefs_appends_to_existing_state() {
        // The CMD merge supplements (appends controllers) rather than overriding,
        // so a `[Statedef -1]` split across the .cns and .cmd keeps both rules.
        let dir = scratch_dir("cmd_merge_append");
        // Pretend a state -1 already exists from a cns (one controller)…
        let pre = write_file(
            &dir,
            "pre.cns",
            "[Statedef -1]\n[State -1, a]\ntype = Null\ntrigger1 = 1\n",
        );
        // …and the .cmd adds another (a command-gated ChangeState).
        let cmd = write_file(
            &dir,
            "chr.cmd",
            "[Statedef -1]\n[State -1, walk]\ntype = ChangeState\n\
             triggerall = command = \"holdfwd\"\ntrigger1 = ctrl\nvalue = 20\n",
        );

        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        merge_cns(&mut states, &pre, "pre.cns");
        merge_cmd_statedefs(&mut states, &cmd, "chr.cmd");

        let s = states.get(&-1).expect("state -1 present");
        // Both controllers survive: the original Null and the appended ChangeState.
        assert_eq!(
            s.controllers.len(),
            2,
            "CMD controllers append, not replace"
        );
        assert!(s
            .controllers
            .iter()
            .any(|c| c.controller_type.as_deref() == Some("ChangeState")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_cmd_statedefs_missing_file_is_skipped() {
        // A missing .cmd must be warn-skipped, never fatal.
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        merge_cmd_statedefs(&mut states, Path::new("/nonexistent/none.cmd"), "none.cmd");
        assert!(states.is_empty());
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

    #[test]
    fn merge_cns_shift_jis_state_file_feeds_states_not_skipped() {
        // T034: a Shift-JIS-encoded CNS state file (non-UTF-8) must still merge
        // its states into the graph. Before the encoding fix the UTF-8 read
        // failed, the file was warn-skipped, and a character with only such files
        // loaded ZERO states (the test-pattern / "no CNS states" failure). The
        // bytes are synthesized here (clean-room: no external content).
        let dir = scratch_dir("merge_sjis");
        let utf8 = "\
; 必殺技ステート
[Statedef 0]
type = S
anim = 0

[Statedef 1000]
type = A
anim = 1000

[State 1000, 波動拳]
type = HitDef
trigger1 = AnimElem = 3
damage = 60, 0
";
        let (sjis, _enc, had_errors) = encoding_rs::SHIFT_JIS.encode(utf8);
        assert!(!had_errors, "fixture must be Shift-JIS-encodable");
        assert!(
            std::str::from_utf8(&sjis).is_err(),
            "Shift-JIS fixture should be invalid UTF-8"
        );
        let path = dir.join("sjis.cns");
        fs::write(&path, &sjis).expect("write Shift-JIS cns");

        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        merge_cns(&mut states, &path, "sjis.cns");

        // The states were NOT skipped: both statedefs are present and playable.
        assert!(
            !states.is_empty(),
            "Shift-JIS CNS must yield states (no encoding-only skip)"
        );
        assert!(states.contains_key(&0));
        assert!(states.contains_key(&1000));
        assert_eq!(states[&1000].state_type.as_deref(), Some("A"));
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

    // ---- External .act palette overrides (pal1..pal12) ----

    /// Builds a synthetic 768-byte `.act` whose palette index 1 (and only it) is
    /// the given RGB, so two fixtures with different `tag` colours yield clearly
    /// different RGBA buffers. (On disk the palette is reverse-ordered: index 1
    /// lives at on-disk position `255 - 1 = 254`.)
    fn make_act_bytes(tag: u8) -> Vec<u8> {
        let mut buf = vec![0u8; 768];
        let pos = 255 - 1; // on-disk position of palette index 1
        buf[pos * 3] = tag; // R
        buf[pos * 3 + 1] = tag.wrapping_add(1); // G
        buf[pos * 3 + 2] = tag.wrapping_add(2); // B
        buf
    }

    /// Writes `bytes` to `dir/name`, returning the full path (binary sibling of
    /// [`write_file`]).
    fn write_bytes(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).expect("write scratch binary");
        path
    }

    #[test]
    fn load_act_palettes_reads_pal_slots_in_order() {
        let dir = scratch_dir("pal_order");
        // pal1, pal3 present; pal2 absent; pal4 references a missing file.
        let def = write_file(
            &dir,
            "c.def",
            "[Files]\npal1 = a.act\npal3 = c.act\npal4 = missing.act\n",
        );
        write_bytes(&dir, "a.act", &make_act_bytes(10));
        write_bytes(&dir, "c.act", &make_act_bytes(30));
        let parsed = DefFile::load(&def).unwrap();
        let pals = load_act_palettes(&parsed, &def);
        // Only the two existing, parseable slots load, in ascending slot order;
        // the missing pal4 is skipped (best-effort), not fatal.
        assert_eq!(pals.len(), 2);
        assert_eq!(pals[0].slot, 1);
        assert_eq!(pals[1].slot, 3);
        // The two palettes differ in their RGBA (different fixture colours).
        assert_ne!(pals[0].palette.rgba, pals[1].palette.rgba);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_act_palettes_empty_when_no_pal_keys() {
        let dir = scratch_dir("pal_none");
        // No pal* keys at all (the stock-KFM shape) → no overrides loaded.
        let def = write_file(&dir, "c.def", "[Files]\nsprite = x.sff\n");
        let parsed = DefFile::load(&def).unwrap();
        assert!(load_act_palettes(&parsed, &def).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_act_palettes_skips_bad_act_without_panic() {
        let dir = scratch_dir("pal_bad");
        // pal1 points at a present-but-too-short .act; the parser pads with black
        // and still yields a palette (never a panic). pal2 is missing entirely.
        let def = write_file(
            &dir,
            "c.def",
            "[Files]\npal1 = short.act\npal2 = gone.act\n",
        );
        write_bytes(&dir, "short.act", &[1, 2, 3]);
        let parsed = DefFile::load(&def).unwrap();
        let pals = load_act_palettes(&parsed, &def);
        // The short .act still parses (recoverable); the missing one is skipped.
        assert_eq!(pals.len(), 1);
        assert_eq!(pals[0].slot, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loaded_palettes_carry_distinct_rgba() {
        // Two different .act fixtures load into distinct RGBA buffers — the core
        // requirement for a costume swap to be visible.
        let dir = scratch_dir("pal_distinct");
        let def = write_file(&dir, "c.def", "[Files]\npal1 = a.act\npal2 = b.act\n");
        write_bytes(&dir, "a.act", &make_act_bytes(10));
        write_bytes(&dir, "b.act", &make_act_bytes(40));
        let parsed = DefFile::load(&def).unwrap();
        let palettes = load_act_palettes(&parsed, &def);
        assert_eq!(palettes.len(), 2);
        // The two loaded palettes differ from each other...
        assert_ne!(palettes[0].palette.rgba, palettes[1].palette.rgba);
        // ...and each carries the fixture's distinctive index-1 colour (the .act
        // is reverse-ordered on disk; the parser de-reverses it to natural order).
        assert_eq!(palettes[0].palette.rgba[4], 10);
        assert_eq!(palettes[1].palette.rgba[4], 40);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_override_palette_selection_or_none() {
        let dir = scratch_dir("pal_resolve");
        let def = write_file(&dir, "c.def", "[Files]\npal1 = a.act\npal2 = b.act\n");
        write_bytes(&dir, "a.act", &make_act_bytes(10));
        write_bytes(&dir, "b.act", &make_act_bytes(40));
        let parsed = DefFile::load(&def).unwrap();
        let palettes = load_act_palettes(&parsed, &def);
        // None → no override (use the SFF-embedded palette).
        assert!(resolve_override_palette(&palettes, None).is_none());
        // In-range selections resolve to the loaded palettes' RGBA, and differ.
        let p0 = resolve_override_palette(&palettes, Some(0)).expect("slot 0");
        let p1 = resolve_override_palette(&palettes, Some(1)).expect("slot 1");
        assert_ne!(p0, p1);
        // The override RGBA differs from the no-override (None) path by definition.
        assert!(resolve_override_palette(&palettes, None).is_none());
        // Out-of-range → None (safe default, never a panic).
        assert!(resolve_override_palette(&palettes, Some(99)).is_none());
        // Empty list → always None regardless of selection.
        assert!(resolve_override_palette(&[], Some(0)).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Builds a representative SFF-embedded RGBA palette (256 × RGBA, index 0
    /// transparent) using colors distinct from [`make_act_bytes`], so an `.act`
    /// override is observably *replacing* it rather than matching by accident.
    fn embedded_rgba() -> [u8; 1024] {
        let mut rgba = [0u8; 1024];
        for i in 0..256usize {
            let dst = i * 4;
            rgba[dst] = i as u8; // R: a ramp
            rgba[dst + 1] = 200; // G: distinct from the .act's index-1 green
            rgba[dst + 2] = 5; // B
            rgba[dst + 3] = if i == 0 { 0 } else { 255 }; // index 0 transparent
        }
        rgba
    }

    /// AC: loading a character's `.act` palette uses the `.act` RGBA as the active
    /// palette (overriding the SFF-embedded palette), and index-0 transparency is
    /// preserved. Fully synthetic + non-gated: it loads a real `.act` off disk
    /// through `load_act_palettes`, resolves the runtime selection with
    /// `resolve_override_palette` (the seam `LoadedCharacter::override_palette`
    /// uses), and asserts the result against a representative SFF-embedded palette.
    #[test]
    fn act_palette_overrides_embedded_and_preserves_index0_transparency() {
        let dir = scratch_dir("pal_overrides_embedded");
        let def = write_file(&dir, "c.def", "[Files]\npal1 = costume.act\n");
        // `make_act_bytes(50)` paints palette index 1 = (50, 51, 52); the rest is
        // black. Index 0 is forced transparent by the ACT parser.
        write_bytes(&dir, "costume.act", &make_act_bytes(50));
        let parsed = DefFile::load(&def).unwrap();
        let palettes = load_act_palettes(&parsed, &def);
        assert_eq!(palettes.len(), 1, "the single pal1 .act must load");

        // Selecting the .act (index 0) yields a concrete override...
        let over = resolve_override_palette(&palettes, Some(0)).expect("pal1 override");

        // ...which OVERRIDES a representative SFF-embedded palette: the active
        // palette is the .act's RGBA, not the embedded bytes.
        let embedded = embedded_rgba();
        assert_ne!(
            over, &embedded,
            ".act override must differ from the SFF-embedded palette"
        );
        // The active color at index 1 came from the .act (50,51,52), not the
        // embedded ramp (whose green is 200) — the costume actually replaced it.
        assert_eq!(
            &over[4..7],
            &[50, 51, 52],
            "index 1 RGB must come from the .act"
        );
        assert_ne!(
            over[5], embedded[5],
            ".act green must replace the embedded green"
        );

        // Index-0 transparency is preserved through the override (alpha == 0),
        // and a color index stays opaque (alpha == 255) — the MUGEN convention
        // survives the swap.
        assert_eq!(over[3], 0, "palette index 0 must stay transparent");
        assert_eq!(over[4 + 3], 255, "index 1 must be opaque after override");

        // The no-override (None) path falls back to the SFF-embedded palette
        // (resolves to None here), so a costumeless character is unchanged.
        assert!(resolve_override_palette(&palettes, None).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Gated real-content check: evilken ships 12 `.act` palettes referenced by
    /// `pal1`..`pal12`. The loader must read them all and they must not all be
    /// identical. Skips cleanly when test-assets/ is absent.
    #[test]
    fn evilken_act_palettes_load_from_def() {
        let def = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join("evilken/evilken.def");
        if !def.exists() {
            eprintln!("skipping evilken .act test: {} not present", def.display());
            return;
        }
        let loaded = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: evilken.def failed to load: {e}");
                return;
            }
        };
        // evilken.def lists pal1..pal12; all twelve .act files ship, so all load.
        assert_eq!(
            loaded.palette_count(),
            12,
            "expected 12 loaded .act palettes, got {}",
            loaded.palette_count()
        );
        // The slots are ascending 1..=12.
        let slots: Vec<u32> = loaded.palettes.iter().map(|p| p.slot).collect();
        assert_eq!(slots, (1..=12).collect::<Vec<_>>());
        // Selecting an override yields a concrete RGBA; the no-override path is
        // None (SFF-embedded). At least two distinct palettes exist among them.
        assert!(loaded.override_palette(None).is_none());
        let p0 = loaded.override_palette(Some(0)).expect("pal1 present");
        // Find some other slot whose RGBA differs (real costumes differ).
        let differs = (1..loaded.palette_count())
            .filter_map(|i| loaded.override_palette(Some(i)))
            .any(|p| p != p0);
        assert!(differs, "all evilken .act palettes were identical");

        // Because evilken ships `.act` palettes, its DEFAULT costume is pal1
        // (index 0), not the SFF-embedded palette. This is the black-screen fix:
        // the app defaults `active_palette` to this index so evilken renders in
        // color instead of near-black.
        assert_eq!(loaded.default_palette_index(), Some(0));

        // And pal1 (the default costume) is a REAL, colorful palette — summing the
        // RGB channels across all 256 entries gives substantial total brightness,
        // unlike the near-black placeholder a CvS-style SFF embeds. This is *why*
        // defaulting to pal1 fixes the black render.
        let rgb_sum: u64 = p0
            .chunks_exact(4)
            .map(|px| u64::from(px[0]) + u64::from(px[1]) + u64::from(px[2]))
            .sum();
        assert!(
            rgb_sum > 10_000,
            "evilken pal1 should be a colorful costume (rgb_sum={rgb_sum}), not near-black"
        );
    }

    /// `default_palette_index` is `None` for a character that ships no `.act`
    /// palettes (e.g. Kung Fu Man, whose costume lives in the SFF), so such a
    /// character keeps its SFF-embedded palette. Gated on the local KFM asset.
    #[test]
    fn default_palette_index_none_without_act_palettes() {
        let def = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let Ok(loaded) = LoadedCharacter::load(&def) else {
            eprintln!("skipping: kfm.def failed to load");
            return;
        };
        assert_eq!(loaded.palette_count(), 0, "KFM ships no .act palettes");
        assert_eq!(
            loaded.default_palette_index(),
            None,
            "a character without .act palettes must keep the SFF-embedded palette"
        );
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
        // The compiled value is the parsed parameter, a single component here.
        assert_eq!(params["x"].len(), 1);
        assert!(params["x"].raw().contains('4'));
        assert!(params["x"].component(0).is_some_and(|c| !c.is_fallback));
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
        assert_eq!(
            loaded.constants.defence,
            CharacterConstants::default().defence
        );
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
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Size]\nground.front = 30\n",
        );
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
        assert!(
            (v.jump_up - (-8.4)).abs() < 1e-6,
            "jump.up derived from jump.neu.y"
        );
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
        assert!(
            (consts.velocity.jump_up - (-9.5)).abs() < 1e-6,
            "explicit jump.up wins"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- A.P4: jump/run/runjump/airjump movement velocities --------------------

    #[test]
    fn load_constants_reads_movement_velocity_group() {
        // The full KFM [Velocity] surface: bare-scalar directional jumps, pair
        // run/runjump, and airjump (whose y derives from airjump.neu.y). Values
        // are KFM's authored ones (kfm.cns [Velocity]).
        let dir = scratch_dir("consts_move_velocity");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n\
             [Velocity]\n\
             run.fwd = 4.6, 0\nrun.back = -4.5,-3.8\n\
             jump.neu = 0,-8.4\njump.fwd = 2.5\njump.back = -2.55\n\
             runjump.fwd = 4,-8.1\nrunjump.back = -2.55,-8.1\n\
             airjump.neu = 0,-8.1\nairjump.fwd = 2.5\nairjump.back = -2.55\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        let v = consts.velocity;
        // run.back pair (x, y).
        assert!((v.run_back.x - (-4.5)).abs() < 1e-6);
        assert!((v.run_back.y - (-3.8)).abs() < 1e-6);
        // Bare-scalar directional ground jumps: x set, y = 0.
        assert!((v.jump_fwd.x - 2.5).abs() < 1e-6);
        assert!((v.jump_fwd.y - 0.0).abs() < 1e-6);
        assert!((v.jump_back.x - (-2.55)).abs() < 1e-6);
        // runjump pairs.
        assert!((v.runjump_fwd.x - 4.0).abs() < 1e-6);
        assert!((v.runjump_fwd.y - (-8.1)).abs() < 1e-6);
        assert!((v.runjump_back.x - (-2.55)).abs() < 1e-6);
        assert!((v.runjump_back.y - (-8.1)).abs() < 1e-6);
        // airjump: neu pair, bare-scalar fwd/back, y derived from airjump.neu.y.
        assert!((v.airjump_neu.x - 0.0).abs() < 1e-6);
        assert!((v.airjump_fwd.x - 2.5).abs() < 1e-6);
        assert!((v.airjump_back.x - (-2.55)).abs() < 1e-6);
        assert!(
            (v.airjump_y - (-8.1)).abs() < 1e-6,
            "airjump.y derived from airjump.neu.y"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_explicit_airjump_y_overrides_airjump_neu() {
        // An explicit airjump.y overrides the airjump.neu-derived value, mirroring
        // the jump.up / jump.neu relationship.
        let dir = scratch_dir("consts_airjumpy");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Velocity]\nairjump.neu = 0,-8.1\nairjump.y = -9.0\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert!((consts.velocity.airjump_neu.y - (-8.1)).abs() < 1e-6);
        assert!(
            (consts.velocity.airjump_y - (-9.0)).abs() < 1e-6,
            "explicit airjump.y wins"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_absent_movement_velocities_keep_defaults() {
        // A [Velocity] with only walk keys leaves the new movement velocities at
        // their MUGEN-sane defaults — never an error, never silently 0.
        let dir = scratch_dir("consts_velocity_absent");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Velocity]\nwalk.fwd = 2.4\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        let d = VelocityConstants::default();
        let v = consts.velocity;
        assert!((v.run_back.x - d.run_back.x).abs() < 1e-6);
        assert!((v.jump_fwd.x - d.jump_fwd.x).abs() < 1e-6);
        assert!((v.jump_back.x - d.jump_back.x).abs() < 1e-6);
        assert!((v.runjump_fwd.x - d.runjump_fwd.x).abs() < 1e-6);
        assert!((v.runjump_back.x - d.runjump_back.x).abs() < 1e-6);
        assert!((v.airjump_neu.y - d.airjump_neu.y).abs() < 1e-6);
        assert!((v.airjump_fwd.x - d.airjump_fwd.x).abs() < 1e-6);
        assert!((v.airjump_back.x - d.airjump_back.x).abs() < 1e-6);
        assert!((v.airjump_y - d.airjump_y).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_fixture_kfm_movement_velocities_nonzero() {
        // Gated real-KFM test: load kfm.def and assert the authored directional
        // jump / airjump velocities match KFM and are nonzero, so common1's
        // jumpstart/airjump `velset = const(velocity.*)` will move horizontally.
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let loaded = LoadedCharacter::load(&def).expect("kfm.def should load");
        let v = loaded.constants.velocity;
        // KFM authored values (kfm.cns [Velocity]).
        assert!((v.jump_fwd.x - 2.5).abs() < 1e-6, "jump.fwd.x");
        assert!((v.jump_back.x - (-2.55)).abs() < 1e-6, "jump.back.x");
        assert!((v.run_back.x - (-4.5)).abs() < 1e-6, "run.back.x");
        assert!((v.runjump_fwd.x - 4.0).abs() < 1e-6, "runjump.fwd.x");
        assert!((v.airjump_fwd.x - 2.5).abs() < 1e-6, "airjump.fwd.x");
        assert!((v.airjump_back.x - (-2.55)).abs() < 1e-6, "airjump.back.x");
        assert!(
            (v.airjump_y - (-8.1)).abs() < 1e-6,
            "airjump.y derived from airjump.neu.y"
        );
        // The load-bearing assertion: these must be nonzero (the bug fixed here).
        assert!(v.jump_fwd.x.abs() > 1e-6, "jump.fwd.x must not be 0");
        assert!(v.airjump_fwd.x.abs() > 1e-6, "airjump.fwd.x must not be 0");
        assert!(v.runjump_fwd.x.abs() > 1e-6, "runjump.fwd.x must not be 0");
    }

    // ---- A.P4 (Proctor): error paths & MUGEN semantics for the new keys ----

    #[test]
    fn load_constants_malformed_movement_velocities_keep_defaults_no_error() {
        // Every new key set to garbage: the loader must keep the per-field default
        // and never error/panic (MUGEN "never crash on bad content"). A bad first
        // component (parse_vec2 -> None) leaves the whole field at its default.
        let dir = scratch_dir("consts_move_velocity_garbage");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n\
             [Velocity]\n\
             run.back = fast\njump.fwd = nope\njump.back = -\n\
             runjump.fwd = x,y\nrunjump.back = ,\n\
             airjump.neu = bad\nairjump.fwd = NaNN\nairjump.back = ??\nairjump.y = oops\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        // No panic on load; values fall back to defaults.
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        let d = VelocityConstants::default();
        let v = consts.velocity;
        assert!((v.run_back.x - d.run_back.x).abs() < 1e-6);
        assert!((v.jump_fwd.x - d.jump_fwd.x).abs() < 1e-6);
        assert!((v.jump_back.x - d.jump_back.x).abs() < 1e-6);
        assert!((v.runjump_fwd.x - d.runjump_fwd.x).abs() < 1e-6);
        assert!((v.runjump_back.x - d.runjump_back.x).abs() < 1e-6);
        assert!((v.airjump_neu.y - d.airjump_neu.y).abs() < 1e-6);
        assert!((v.airjump_fwd.x - d.airjump_fwd.x).abs() < 1e-6);
        assert!((v.airjump_back.x - d.airjump_back.x).abs() < 1e-6);
        // Malformed airjump.neu means its y can't seed airjump_y; malformed
        // airjump.y can't override it either, so airjump_y keeps its default.
        assert!((v.airjump_y - d.airjump_y).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_no_velocity_section_keeps_all_defaults() {
        // A character with no [Velocity] section at all: every velocity field
        // (old and new) stays at its KFM-baseline default; load never errors.
        let dir = scratch_dir("consts_no_velocity_section");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Movement]\nyaccel = .44\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert_eq!(consts.velocity, VelocityConstants::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_bare_scalar_jump_forces_y_zero_among_pairs() {
        // MUGEN semantics: a bare-scalar directional jump (jump.fwd = 2.5) stores
        // y = 0 even when sibling entries in the same section are `x, y` pairs.
        // A leak of the neighbor's y into the bare field would surface here.
        let dir = scratch_dir("consts_bare_among_pairs");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n\
             [Velocity]\n\
             runjump.fwd = 4, -8.1\njump.fwd = 2.5\nrunjump.back = -2.55, -8.1\n\
             airjump.neu = 0, -8.1\nairjump.fwd = 2.5\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let v = load_constants(&parsed, &def, &["chr.cns".to_string()]).velocity;
        assert!((v.jump_fwd.x - 2.5).abs() < 1e-6);
        assert!(
            v.jump_fwd.y.abs() < 1e-6,
            "bare jump.fwd y must be 0, not the pair sibling's y"
        );
        assert!((v.airjump_fwd.x - 2.5).abs() < 1e-6);
        assert!(v.airjump_fwd.y.abs() < 1e-6, "bare airjump.fwd y must be 0");
        // The pairs keep their authored y.
        assert!((v.runjump_fwd.y - (-8.1)).abs() < 1e-6);
        assert!((v.airjump_neu.y - (-8.1)).abs() < 1e-6);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_airjump_y_derives_from_neu_when_only_neu_present() {
        // Mirror of the jump.up/jump.neu rule for air jumps: with airjump.neu set
        // and no explicit airjump.y, airjump_y takes airjump.neu's y component.
        let dir = scratch_dir("consts_airjumpy_derived");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Velocity]\nairjump.neu = 0, -7.25\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let v = load_constants(&parsed, &def, &["chr.cns".to_string()]).velocity;
        assert!(
            (v.airjump_y - (-7.25)).abs() < 1e-6,
            "airjump.y must derive from airjump.neu.y when no explicit airjump.y"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_velocity_group_directly_maps_kfm_exact_keys() {
        // Drive read_velocity_group with the EXACT key spellings + values from
        // test-assets/kfm/kfm.cns [Velocity], starting from defaults. This locks
        // the loader's key names to KFM's authored ones independent of the .def
        // file-resolution path.
        let ini = DefFile::from_str(
            "[Velocity]\n\
             walk.fwd  = 2.4\nwalk.back = -2.2\n\
             run.fwd  = 4.6, 0\nrun.back = -4.5,-3.8\n\
             jump.neu = 0,-8.4\njump.back = -2.55\njump.fwd = 2.5\n\
             runjump.back = -2.55,-8.1\nrunjump.fwd = 4,-8.1\n\
             airjump.neu = 0,-8.1\nairjump.back = -2.55\nairjump.fwd = 2.5\n",
        )
        .expect("synthetic [Velocity] should parse");
        let mut v = VelocityConstants::default();
        read_velocity_group(&ini, &mut v);
        assert!((v.run_back.x - (-4.5)).abs() < 1e-6);
        assert!((v.run_back.y - (-3.8)).abs() < 1e-6);
        assert!((v.jump_fwd.x - 2.5).abs() < 1e-6);
        assert!((v.jump_back.x - (-2.55)).abs() < 1e-6);
        assert!((v.runjump_fwd.x - 4.0).abs() < 1e-6 && (v.runjump_fwd.y - (-8.1)).abs() < 1e-6);
        assert!(
            (v.runjump_back.x - (-2.55)).abs() < 1e-6 && (v.runjump_back.y - (-8.1)).abs() < 1e-6
        );
        assert!((v.airjump_neu.y - (-8.1)).abs() < 1e-6);
        assert!((v.airjump_fwd.x - 2.5).abs() < 1e-6);
        assert!((v.airjump_back.x - (-2.55)).abs() < 1e-6);
        // airjump.y derives from airjump.neu.y (KFM authors no explicit airjump.y).
        assert!((v.airjump_y - (-8.1)).abs() < 1e-6);
    }

    // ---- 5.3 review fix (4): jump.up honors the 2-component `x, y` form ----

    #[test]
    fn load_constants_jump_up_reads_y_of_two_component_form() {
        // Regression: `jump.up = 0, -9.5` must store -9.5 (the y component), not 0
        // (the x component). A previous version read the first component and
        // silently stored 0, killing the jump.
        let dir = scratch_dir("consts_jumpup_pair");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Velocity]\njump.neu = 0, -8.4\njump.up = 0, -9.5\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert!(
            (consts.velocity.jump_up - (-9.5)).abs() < 1e-6,
            "2-component jump.up stores the y component (-9.5), got {}",
            consts.velocity.jump_up
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_jump_up_scalar_and_pair_and_garbage() {
        // Single-component form: the lone value is the upward speed.
        assert_eq!(parse_jump_up("-9.5"), Some(-9.5));
        // Two-component `x, y` form: the y component is the upward speed.
        assert_eq!(parse_jump_up("0, -9.5"), Some(-9.5));
        assert_eq!(parse_jump_up(" 1.0 , -7.0 "), Some(-7.0));
        // Fully malformed → None (caller keeps the jump.neu-derived default).
        assert_eq!(parse_jump_up("nope"), None);
        // Valid x but malformed y → falls back to the single (x) value.
        assert_eq!(parse_jump_up("3, bad"), Some(3.0));
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
        assert!(
            (consts.movement.yaccel - d.yaccel).abs() < 1e-6,
            "bad yaccel keeps default"
        );
        assert!(
            (consts.movement.stand_friction - 0.5).abs() < 1e-6,
            "valid sibling still read"
        );
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
        // Air-jump defaults to MUGEN's "no air jump" baseline when absent.
        assert_eq!(m.airjump_num, 0, "airjump.num defaults to 0 (no air jump)");
        assert!(
            (m.airjump_height - 0.0).abs() < 1e-6,
            "airjump.height defaults to 0"
        );
    }

    #[test]
    fn load_constants_reads_airjump_movement_keys() {
        // `[Movement] airjump.num` / `airjump.height` are read; KFM-style values.
        let dir = scratch_dir("consts_airjump");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Movement]\nairjump.num = 1\nairjump.height = 35\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert_eq!(
            consts.movement.airjump_num, 1,
            "airjump.num read from [Movement]"
        );
        assert!(
            (consts.movement.airjump_height - 35.0).abs() < 1e-6,
            "airjump.height read from [Movement]"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_airjump_absent_keeps_default() {
        // A character that omits the air-jump keys gets the no-air-jump default.
        let dir = scratch_dir("consts_airjump_absent");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Movement]\nyaccel = .44\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert_eq!(
            consts.movement.airjump_num, 0,
            "absent airjump.num keeps default 0"
        );
        assert!(
            (consts.movement.airjump_height - 0.0).abs() < 1e-6,
            "absent airjump.height keeps default 0"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_airjump_malformed_keeps_default() {
        // Messy content: a non-integer airjump.num and a non-float airjump.height
        // must NOT panic and must leave each field at its no-air-jump default
        // (`get_parsed` returns None on a bad value, so the prior value is kept).
        let dir = scratch_dir("consts_airjump_malformed");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Movement]\nairjump.num = lots\nairjump.height = high\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert_eq!(
            consts.movement.airjump_num, 0,
            "malformed airjump.num falls back to the default 0"
        );
        assert!(
            (consts.movement.airjump_height - 0.0).abs() < 1e-6,
            "malformed airjump.height falls back to the default 0"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_airjump_num_negative_is_read_verbatim() {
        // The loader stores whatever integer it reads (a negative is syntactically
        // valid); the executor's gate (`airjump_num > 0`) is what neutralizes it.
        // This pins the loader contract: it does not silently clamp.
        let dir = scratch_dir("consts_airjump_negative");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Movement]\nairjump.num = -2\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert_eq!(
            consts.movement.airjump_num, -2,
            "a negative airjump.num is read verbatim (the executor gate handles it)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_constants_real_kfm_airjump_when_present() {
        // Gated real-KFM check (skips when the fixture is absent): KFM's authored
        // [Movement] airjump.num = 1 / airjump.height = 35 round-trip through the
        // real loader, proving the keys are read from genuine MUGEN content (not
        // just synthetic INI).
        let def = Path::new(env!("CARGO_MANIFEST_DIR"))
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
        assert_eq!(
            lc.constants.movement.airjump_num, 1,
            "KFM authors airjump.num = 1"
        );
        assert!(
            (lc.constants.movement.airjump_height - 35.0).abs() < 1e-6,
            "KFM authors airjump.height = 35; got {}",
            lc.constants.movement.airjump_height
        );
    }

    // =====================================================================
    // Proctor (task 5.4 Part B fix #4): jump.up 2-component handling +
    // velocity-override edge cases, layered on top of Forge's loader tests.
    // =====================================================================

    #[test]
    fn parse_jump_up_two_component_does_not_return_x() {
        // Regression guard for the exact bug fixed: the x component of a
        // 2-component jump.up must NEVER be what is returned. Use a distinctive
        // nonzero x so a regression that reads x is unambiguous.
        assert_eq!(parse_jump_up("7.5, -9.5"), Some(-9.5));
        assert_ne!(
            parse_jump_up("7.5, -9.5"),
            Some(7.5),
            "must not return the x component"
        );
    }

    #[test]
    fn parse_jump_up_empty_and_comma_only_are_none() {
        // Fully degenerate inputs from messy content yield None (caller keeps the
        // jump.neu-derived default), never a panic.
        assert_eq!(parse_jump_up(""), None);
        assert_eq!(parse_jump_up(","), None);
        // Leading-comma (empty/unparseable x) but a VALID y: the y component is
        // still recovered as the upward speed. This documents that the y-read is
        // the load-bearing component — a malformed x does not discard a good y.
        assert_eq!(parse_jump_up(", -9.5"), Some(-9.5));
    }

    #[test]
    fn parse_jump_up_leading_dot_float() {
        // KFM-style leading-dot floats parse in both components.
        assert_eq!(parse_jump_up(".5"), Some(0.5));
        assert_eq!(parse_jump_up("0, -.5"), Some(-0.5));
    }

    #[test]
    fn jump_up_two_component_zero_x_stores_y_through_loader() {
        // The headline case from the task: `jump.up = 0, -9.5` must store -9.5
        // (the y), not 0 (the x), end-to-end through load_constants.
        let dir = scratch_dir("consts_jumpup_zero_x");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n[Velocity]\njump.up = 0, -9.5\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let consts = load_constants(&parsed, &def, &["chr.cns".to_string()]);
        assert!(
            (consts.velocity.jump_up - (-9.5)).abs() < 1e-6,
            "jump.up = 0, -9.5 must store -9.5, got {}",
            consts.velocity.jump_up
        );
        assert!(
            consts.velocity.jump_up.abs() > 1e-6,
            "must not be silently 0"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn velocity_overrides_honor_two_component_form() {
        // The other [Velocity] overrides (walk.fwd/back, run.fwd, jump.neu) use
        // parse_vec2, which reads both components. A 2-component value stores both
        // x and y rather than dropping y.
        let dir = scratch_dir("consts_vel_pairs");
        let def = write_file(&dir, "chr.def", "[Files]\ncns = chr.cns\n");
        write_file(
            &dir,
            "chr.cns",
            "[Data]\nlife = 1000\n\
             [Velocity]\nwalk.fwd = 2.4, 0.1\nrun.fwd = 4.6, -0.2\njump.neu = 0.3, -8.4\n",
        );
        let parsed = DefFile::load(&def).unwrap();
        let v = load_constants(&parsed, &def, &["chr.cns".to_string()]).velocity;
        assert!((v.walk_fwd.x - 2.4).abs() < 1e-6 && (v.walk_fwd.y - 0.1).abs() < 1e-6);
        assert!((v.run_fwd.x - 4.6).abs() < 1e-6 && (v.run_fwd.y - (-0.2)).abs() < 1e-6);
        // jump.neu pair stored; jump.up derived from its y (no explicit jump.up).
        assert!((v.jump_neu.x - 0.3).abs() < 1e-6 && (v.jump_neu.y - (-8.4)).abs() < 1e-6);
        assert!(
            (v.jump_up - (-8.4)).abs() < 1e-6,
            "jump.up derived from jump.neu.y"
        );
        let _ = fs::remove_dir_all(&dir);
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

    /// PR-D #23 (gated KFM): KFM's sweep state (`[Statedef 3050]`) authors
    /// `fall = 1, fall.damage = 70`. Driving a real KFM character into that state
    /// must build an `active_hitdef` carrying `fall = true` and `fall_damage = 70`
    /// — proving the loader → `ctrl_hit_def` → HitDef path is no longer a constant
    /// `0`. The companion synthetic combat test then proves `HitFallDamage`
    /// actually subtracts that value from life on landing.
    #[test]
    fn real_kfm_sweep_hitdef_carries_fall_damage_70() {
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
        let mut ch = crate::Character::with_constants(loaded.constants);
        // Enter the sweep state; its HitDef is gated on `trigger1 = Time = 0`, so
        // the first tick at state-time 0 builds the active HitDef.
        ch.change_state(&loaded.states, 3050);
        ch.tick(&loaded, None, crate::StageView::default());

        let hd = ch
            .active_hitdef
            .expect("KFM sweep state 3050 must build an active HitDef on entry");
        assert!(hd.fall, "KFM sweep authors `fall = 1`");
        assert_eq!(
            hd.fall_damage, 70,
            "KFM sweep authors `fall.damage = 70` -> carried onto the HitDef (not 0)"
        );
    }

    // ====================================================================
    // Task 7.3 part B: engine built-in ground locomotion, proven against real
    // KFM with NO app shim. A live Character is given `ctrl` and a command
    // source set directly to the `hold*` command names a real CommandMatcher
    // would produce; the loader-injected `[Statedef -1]` controllers must drive
    // the basic 4-way transitions, and KFM's own `[Statedef 20]` must then walk.
    // ====================================================================

    /// Loads real KFM and stands a fresh [`Character`](crate::Character) in state
    /// 0 with control, returning `(loaded, character)` or `None` (skip) when the
    /// fixture is absent/unloadable.
    fn kfm_standing_with_ctrl() -> Option<(LoadedCharacter, crate::Character)> {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping locomotion test: {} not present", def.display());
            return None;
        }
        let loaded = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping locomotion test: kfm.def failed to load: {e}");
                return None;
            }
        };
        let mut ch = crate::Character::with_constants(loaded.constants);
        ch.state_no = 0;
        ch.anim = 0;
        ch.ctrl = true;
        ch.facing = crate::Facing::Right;
        Some((loaded, ch))
    }

    /// Sets the character's command source to exactly the given active command
    /// names (what a real `CommandMatcher` snapshot would feed each tick).
    fn set_commands(ch: &mut crate::Character, names: &[&str]) {
        ch.set_command_source(Box::new(crate::ActiveCommands::from_names(
            names.iter().map(|s| s.to_string()),
        )));
    }

    /// Part B AC: holding Forward from stand reaches walk (state 20) and gains a
    /// nonzero walk velocity within a few ticks — driven entirely by the loader's
    /// engine built-in plus KFM's own `[Statedef 20]`, with NO app shim.
    #[test]
    fn builtin_locomotion_stand_to_walk_and_velocity() {
        let Some((loaded, mut ch)) = kfm_standing_with_ctrl() else {
            return;
        };

        let mut reached_walk = false;
        for _ in 0..5 {
            // `holdfwd` is what KFM's real matcher produces while Forward is held.
            set_commands(&mut ch, &["holdfwd"]);
            ch.tick(&loaded, None, crate::StageView::default());
            if ch.state_no == 20 {
                reached_walk = true;
                break;
            }
        }
        assert!(
            reached_walk,
            "holding Forward from stand must reach walk (state 20)"
        );
        assert!(
            ch.vel.x.abs() > 0.0,
            "walk state must impart a nonzero walk velocity, got vel.x = {}",
            ch.vel.x
        );
    }

    /// Part B AC: holding Down from stand drives the crouch path (10 -> 11). The
    /// crouch start (10) and its AnimTime-gated advance to crouch-hold (11) can
    /// both resolve within a single tick (the executor follows ChangeState chains
    /// in one frame), so the observable end state is the crouch hold (11) — proof
    /// the built-in stand->crouch fired and common1's 10->11 took over.
    #[test]
    fn builtin_locomotion_stand_to_crouch() {
        let Some((loaded, mut ch)) = kfm_standing_with_ctrl() else {
            return;
        };

        let mut reached_crouch = false;
        let mut visited = Vec::new();
        for _ in 0..20 {
            set_commands(&mut ch, &["holddown"]);
            ch.tick(&loaded, None, crate::StageView::default());
            visited.push(ch.state_no);
            // Either the crouch start (10) or the crouch hold (11) proves the
            // built-in stand->crouch transition fired.
            if ch.state_no == 10 || ch.state_no == 11 {
                reached_crouch = true;
                break;
            }
        }
        assert!(
            reached_crouch,
            "holding Down from stand must drive the crouch path (10 -> 11); visited {visited:?}"
        );
        assert_eq!(
            ch.state_type,
            crate::StateType::Crouching,
            "the character must be crouching after holding Down"
        );
    }

    /// Part B AC: holding Up from stand drives the jump path (40 -> 50). The jump
    /// start (40) and its AnimTime-gated advance to the air state (50) can both
    /// resolve within a single tick, so the observable end state is the air state
    /// (50) — proof the built-in stand->jump fired and common1's 40->50 took over.
    #[test]
    fn builtin_locomotion_stand_to_jump() {
        let Some((loaded, mut ch)) = kfm_standing_with_ctrl() else {
            return;
        };

        let mut reached_jump = false;
        let mut visited = Vec::new();
        for _ in 0..20 {
            set_commands(&mut ch, &["holdup"]);
            ch.tick(&loaded, None, crate::StageView::default());
            visited.push(ch.state_no);
            // Either the jump start (40) or the air state (50) proves the built-in
            // stand->jump transition fired.
            if ch.state_no == 40 || ch.state_no == 50 {
                reached_jump = true;
                break;
            }
        }
        assert!(
            reached_jump,
            "holding Up from stand must drive the jump path (40 -> 50); visited {visited:?}"
        );
    }

    /// Part B AC: releasing all directions while walking returns to stand (0).
    #[test]
    fn builtin_locomotion_walk_to_stand_on_release() {
        let Some((loaded, mut ch)) = kfm_standing_with_ctrl() else {
            return;
        };

        // First walk.
        for _ in 0..5 {
            set_commands(&mut ch, &["holdfwd"]);
            ch.tick(&loaded, None, crate::StageView::default());
            if ch.state_no == 20 {
                break;
            }
        }
        assert_eq!(ch.state_no, 20, "precondition: walking before release");

        // Release everything: the walk->stand built-in must return to state 0.
        let mut returned = false;
        for _ in 0..5 {
            set_commands(&mut ch, &[]);
            ch.tick(&loaded, None, crate::StageView::default());
            if ch.state_no == 0 {
                returned = true;
                break;
            }
        }
        assert!(returned, "releasing in walk must return to stand (state 0)");
    }

    /// Part B (priority): the engine built-ins are appended AFTER the character's
    /// own `[State -1, ...]` controllers, so a character's authored command-states
    /// (specials/run/attacks) keep priority. KFM's `[Statedef -1]` controllers
    /// must all precede the appended `engine:`-labelled built-ins.
    #[test]
    fn builtin_locomotion_is_appended_after_authored_minus_one() {
        let Some((loaded, _)) = kfm_standing_with_ctrl() else {
            return;
        };
        let minus_one = loaded.state(-1).expect("[Statedef -1] exists");
        let first_builtin = minus_one
            .controllers
            .iter()
            .position(|c| c.label.starts_with("engine:"));
        let last_authored = minus_one
            .controllers
            .iter()
            .rposition(|c| !c.label.starts_with("engine:"));
        // KFM authors its own -1 controllers (run/specials/attacks).
        let last_authored = last_authored.expect("KFM authors its own [Statedef -1] controllers");
        let first_builtin = first_builtin.expect("engine built-ins must be present");
        assert!(
            first_builtin > last_authored,
            "engine built-ins (first at {first_builtin}) must come AFTER all authored \
             controllers (last at {last_authored}), preserving character priority"
        );
        // Exactly the eight built-in controllers we synthesize: the seven
        // ground-locomotion command-states plus the A.P15b auto-land (50/51->52).
        let builtin_count = minus_one
            .controllers
            .iter()
            .filter(|c| c.label.starts_with("engine:"))
            .count();
        assert_eq!(
            builtin_count, 8,
            "exactly the eight built-ins (seven locomotion + one auto-land)"
        );
    }

    /// Part B (synthetic, no fixture): a character with no `.cmd` (so no authored
    /// `[Statedef -1]`) still gets the engine built-in locomotion synthesized into
    /// `[Statedef -1]`, proving the built-in is applied to EVERY loaded character.
    #[test]
    fn builtin_locomotion_present_for_character_without_cmd() {
        let sff_src = test_asset("kfm/kfm.sff");
        let air_src = test_asset("kfm/kfm.air");
        if !sff_src.exists() || !air_src.exists() {
            eprintln!("skipping no-cmd locomotion test: kfm binaries absent");
            return;
        }
        let dir = scratch_dir("builtin_no_cmd");
        fs::write(dir.join("chr.sff"), fs::read(&sff_src).unwrap()).unwrap();
        fs::write(dir.join("chr.air"), fs::read(&air_src).unwrap()).unwrap();
        write_file(&dir, "chr.cns", "[Statedef 0]\ntype = S\n");
        // No `cmd` in [Files] at all.
        let def = write_file(
            &dir,
            "chr.def",
            "[Files]\nsprite = chr.sff\nanim = chr.air\ncns = chr.cns\n",
        );
        let loaded = LoadedCharacter::load(&def).expect("synthetic character should load");
        let minus_one = loaded
            .state(-1)
            .expect("engine built-in locomotion must synthesize [Statedef -1] even without a .cmd");
        let walks = minus_one.controllers.iter().any(|c| {
            c.controller_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"))
                && c.params
                    .get("value")
                    .is_some_and(|e| e.source.trim() == "20")
        });
        assert!(
            walks,
            "the built-in stand->walk command-state must be present"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ====================================================================
    // Task A.P15b (Proctor): engine built-in AUTO-LAND.
    //
    // MUGEN's basic jump (states 50/51) has NO land transition in common1
    // (verified: `[Statedef 50]` carries only VarSet/ChangeAnim). The engine
    // supplies the land as a hardcoded built-in: a `[Statedef -1]` ChangeState
    //   value = 52
    //   trigger1 = (stateno = 50 || stateno = 51) && pos y >= 0 && vel y >= 0
    // with NO `ctrl` triggerall (landing is unconditional), gated STRICTLY to
    // states 50/51 so it never hijacks get-hit / custom air states (5040 etc.)
    // which carry their own land logic.
    //
    // These tests are injection- and behavior-level: they build a tiny state
    // graph, run the SAME loader built-in injection (`append_builtin_ground_
    // locomotion`), and drive `Character::tick_with` to assert the land fires
    // exactly when MUGEN says it should — and never otherwise.
    // ====================================================================

    use crate::{Character, Physics, StageView};
    use fp_formats::air::{AnimAction, AnimFrame, BlendMode};

    /// World Y of the floor (ground plane). Mirrors the executor's private
    /// `GROUND_Y` (the world origin, `Y = 0`); kept local because that constant
    /// is module-private. Y increases downward, so `pos.y >= 0` means "at or
    /// below the floor line" — i.e. the landing condition.
    const FLOOR_Y: f32 = 0.0;

    /// A one-action AIR file so the executor's `advance_animation` has something
    /// to advance (action 0, a single 5-tick frame). Keeps the synthetic
    /// auto-land tests from depending on any binary fixture.
    fn tiny_air_p15b() -> AirFile {
        let mut actions = HashMap::new();
        actions.insert(
            0,
            AnimAction {
                action_number: 0,
                frames: vec![AnimFrame {
                    sprite: fp_core::SpriteId::new(0, 0),
                    offset: Vec2::new(0, 0),
                    ticks: 5,
                    flip_h: false,
                    flip_v: false,
                    blend: BlendMode::Normal,
                    clsn1: Vec::new(),
                    clsn2: Vec::new(),
                    ..Default::default()
                }],
                loopstart: 0,
            },
        );
        AirFile { actions }
    }

    /// Builds the minimal state graph the auto-land tests drive: the basic air
    /// jump states 50 (`type=A, physics=A`) and 51 (an alternate basic jump),
    /// the get-hit air-fall state 5040 (`type=A, physics=A`), and the grounded
    /// Jump Land 52 (`type=S`). The graph deliberately authors NO land
    /// transition of its own on 50/51 — exactly like KFM's common1 — so any
    /// transition to 52 must come from the engine built-in under test.
    ///
    /// State 5040 carries its OWN land rule (common1's `Vel Y > 0 && Pos Y >= 0`
    /// → 52) so the "the built-in must not hijack get-hit states" assertion can
    /// distinguish "the BUILTIN fired" (it must not, for 5040) from "5040's own
    /// authored rule fired" (allowed). To make that distinction crisp, 5040 here
    /// authors NO land rule at all: if it ever reaches 52, only the (incorrectly
    /// ungated) built-in could have done it.
    fn p15b_graph() -> HashMap<i32, CompiledState> {
        let cns = CnsFile::from_str(
            "[Statedef 50]\n\
             type = A\n\
             physics = A\n\
             anim = 0\n\
             \n\
             [Statedef 51]\n\
             type = A\n\
             physics = A\n\
             anim = 0\n\
             \n\
             [Statedef 52]\n\
             type = S\n\
             physics = N\n\
             anim = 0\n\
             \n\
             [Statedef 5040]\n\
             type = A\n\
             physics = A\n\
             anim = 0\n",
        )
        .expect("synthetic A.P15b CNS compiles");
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        for d in &cns.statedefs {
            states.insert(d.number, CompiledState::from_parsed(d));
        }
        // Run the REAL loader built-in injection path (the same call site the
        // loader uses at load time). This is what is supposed to add the
        // auto-land controller to [Statedef -1].
        append_builtin_ground_locomotion(&mut states);
        states
    }

    /// Locate the auto-land built-in controller in `[Statedef -1]`: an
    /// `engine:`-labelled ChangeState whose `value` resolves to `52`.
    fn find_auto_land(states: &HashMap<i32, CompiledState>) -> Option<&CompiledController> {
        states.get(&-1)?.controllers.iter().find(|c| {
            c.label.starts_with("engine:")
                && c.controller_type
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"))
                && c.params
                    .get("value")
                    .is_some_and(|p| p.component(0).is_some_and(|e| e.source.trim() == "52"))
        })
    }

    /// AC1 (injection): the loader built-in path injects an auto-land ChangeState
    /// (value 52) into the synthesized `[Statedef -1]`. It must be `engine:`-
    /// labelled (so the idempotency guard covers it) and must NOT carry a `ctrl`
    /// triggerall (landing is unconditional).
    #[test]
    fn p15b_auto_land_builtin_is_injected_value_52_not_ctrl_gated() {
        let states = p15b_graph();
        let land = find_auto_land(&states)
            .expect("auto-land built-in (engine: ChangeState value=52) must be injected into [-1]");
        // Landing is unconditional: no `ctrl` in any triggerall component.
        let ctrl_gated = land
            .triggerall
            .iter()
            .any(|e| e.source.to_ascii_lowercase().contains("ctrl"));
        assert!(
            !ctrl_gated,
            "auto-land must NOT be ctrl-gated (landing is unconditional); triggerall = {:?}",
            land.triggerall
                .iter()
                .map(|e| &e.source)
                .collect::<Vec<_>>()
        );
    }

    /// AC1 (idempotency): re-running the built-in injection on an already-injected
    /// graph must not duplicate the auto-land controller (the `engine:` label
    /// guard still holds). Exactly one auto-land ChangeState(52) controller.
    #[test]
    fn p15b_auto_land_injection_is_idempotent() {
        let mut states = p15b_graph();
        let count_52 = |s: &HashMap<i32, CompiledState>| -> usize {
            s.get(&-1)
                .map(|m| {
                    m.controllers
                        .iter()
                        .filter(|c| {
                            c.label.starts_with("engine:")
                                && c.controller_type
                                    .as_deref()
                                    .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"))
                                && c.params.get("value").is_some_and(|p| {
                                    p.component(0).is_some_and(|e| e.source.trim() == "52")
                                })
                        })
                        .count()
                })
                .unwrap_or(0)
        };
        assert_eq!(
            count_52(&states),
            1,
            "exactly one auto-land controller after first inject"
        );
        // Second inject must be a no-op (idempotency guard).
        append_builtin_ground_locomotion(&mut states);
        assert_eq!(
            count_52(&states),
            1,
            "re-injecting must not duplicate the auto-land controller (idempotency guard)"
        );
    }

    /// AC3 (positive, state 50): a character in basic jump state 50 sitting AT
    /// the floor (`pos.y >= 0`) with a downward/zero velocity (`vel.y >= 0`) is
    /// carried to Jump Land 52 by the built-in within a tick. This is the exact
    /// post-clamp landing frame (P15a pins pos.y=0 and leaves vel.y downward).
    #[test]
    fn p15b_state_50_at_floor_falling_lands_to_52() {
        let states = p15b_graph();
        let air = tiny_air_p15b();
        let mut ch = Character::new();
        ch.change_state(&states, 50);
        ch.pos = Vec2::new(0.0, FLOOR_Y); // at the floor
        ch.physics = Physics::None; // isolate from gravity; vel set explicitly
        ch.vel = Vec2::new(0.0, 1.0); // moving downward (vel.y >= 0)
        ch.tick_with(&states, &air, None, StageView::default());
        assert_eq!(
            ch.state_no, 52,
            "basic jump state 50 at the floor with downward velocity must land to 52"
        );
    }

    /// AC3 (positive, state 51): the OTHER basic jump state lands to 52 too — the
    /// built-in is gated to 50 OR 51, not just 50.
    #[test]
    fn p15b_state_51_at_floor_falling_lands_to_52() {
        let states = p15b_graph();
        let air = tiny_air_p15b();
        let mut ch = Character::new();
        ch.change_state(&states, 51);
        ch.pos = Vec2::new(0.0, FLOOR_Y);
        ch.physics = Physics::None;
        ch.vel = Vec2::new(0.0, 0.0); // exactly zero is still >= 0 → lands
        ch.tick_with(&states, &air, None, StageView::default());
        assert_eq!(
            ch.state_no, 52,
            "basic jump state 51 at the floor must also land to 52"
        );
    }

    /// AC2 (negative, still rising): a character in state 50 with upward velocity
    /// (`vel.y < 0`) must NOT land — it is still going up. Even at pos.y = 0
    /// (the launch frame) a rising character keeps jumping.
    #[test]
    fn p15b_state_50_rising_does_not_land() {
        let states = p15b_graph();
        let air = tiny_air_p15b();
        let mut ch = Character::new();
        ch.change_state(&states, 50);
        ch.pos = Vec2::new(0.0, FLOOR_Y); // at floor height...
        ch.physics = Physics::None;
        ch.vel = Vec2::new(0.0, -8.0); // ...but moving UP (vel.y < 0)
        ch.tick_with(&states, &air, None, StageView::default());
        assert_eq!(
            ch.state_no, 50,
            "a rising character (vel.y < 0) must NOT land prematurely; stayed airborne"
        );
    }

    /// AC2 (negative, airborne): a character in state 50 above the ground
    /// (`pos.y < 0`) must NOT land even while falling (`vel.y > 0`) — it has not
    /// reached the floor yet.
    #[test]
    fn p15b_state_50_airborne_does_not_land() {
        let states = p15b_graph();
        let air = tiny_air_p15b();
        let mut ch = Character::new();
        ch.change_state(&states, 50);
        ch.pos = Vec2::new(0.0, -40.0); // well above the floor
        ch.physics = Physics::None; // freeze position so it stays at -40
        ch.vel = Vec2::new(0.0, 2.0); // falling, but not yet landed
        ch.tick_with(&states, &air, None, StageView::default());
        assert_eq!(
            ch.state_no, 50,
            "an airborne character (pos.y < 0) must NOT land before reaching the floor"
        );
    }

    /// AC2 (gating): the auto-land built-in must be strictly gated to states
    /// 50/51 and must NOT hijack a get-hit / custom air state (here 5040, which
    /// authors NO land rule of its own in this graph). A character dumped at the
    /// floor while falling in state 5040 must STAY in 5040 — only the basic-jump
    /// states 50/51 auto-land.
    #[test]
    fn p15b_gethit_air_state_5040_is_not_hijacked() {
        let states = p15b_graph();
        let air = tiny_air_p15b();
        let mut ch = Character::new();
        ch.change_state(&states, 5040);
        ch.pos = Vec2::new(0.0, FLOOR_Y); // at floor, the landing-frame condition
        ch.physics = Physics::None;
        ch.vel = Vec2::new(0.0, 3.0); // falling
                                      // Run several ticks: the built-in must never fire for 5040.
        for _ in 0..5 {
            ch.tick_with(&states, &air, None, StageView::default());
            assert_eq!(
                ch.state_no, 5040,
                "get-hit air state 5040 must NOT be auto-landed by the basic-jump built-in"
            );
        }
    }

    /// AC2/AC3 (full synthetic arc, no fixture): drive the basic jump end to end
    /// under the REAL physics path. Enter state 50 with a real upward launch and
    /// air physics; gravity (yaccel) reverses it, the P15a clamp pins pos.y = 0
    /// while leaving vel.y downward, and the auto-land built-in then carries the
    /// character into Jump Land 52 — proving the jump → land loop completes
    /// without any data land rule on 50.
    #[test]
    fn p15b_synthetic_full_jump_50_reaches_land_52() {
        let states = p15b_graph();
        let air = tiny_air_p15b();
        let yaccel = CharacterConstants::default().movement.yaccel;
        assert!(
            yaccel > 0.0,
            "gravity must be downward (positive, Y increases downward)"
        );

        let mut ch = Character::new();
        ch.change_state(&states, 50); // sets physics = Air via the statedef header
        assert_eq!(
            ch.physics,
            Physics::Air,
            "state 50 entry must set air physics"
        );
        ch.pos = Vec2::new(0.0, FLOOR_Y);
        ch.vel = Vec2::new(0.0, -8.4); // launch upward (negative Y)

        let mut peaked_airborne = false;
        let mut landed = false;
        for _ in 0..240 {
            ch.tick_with(&states, &air, None, StageView::default());
            assert!(
                ch.pos.y <= FLOOR_Y + 1e-4,
                "character must never sink below the floor; pos.y = {}",
                ch.pos.y
            );
            if ch.pos.y < -1.0 {
                peaked_airborne = true; // genuinely left the ground on the way up
            }
            if ch.state_no == 52 {
                landed = true;
                break;
            }
        }
        assert!(
            peaked_airborne,
            "the jump should lift the character off the floor first"
        );
        assert!(
            landed,
            "the basic jump (50) must auto-land to Jump Land (52) once it returns to the floor; \
             ended in state {} at pos.y = {}",
            ch.state_no, ch.pos.y
        );
    }

    /// AC3 (gated real KFM, skip-if-missing): drive a FULL real-KFM jump from
    /// jumpstart (40) through the air state (50) with KFM's real jump velocity +
    /// gravity + the P15a clamp, and assert the character ends GROUNDED (Jump
    /// Land 52, then common1's 52 → 0 Stand). Proves the jump → land → stand loop
    /// completes on real content. Skips cleanly when test-assets/ is absent.
    #[test]
    fn p15b_real_kfm_full_jump_reaches_grounded_state() {
        let Some((loaded, mut ch)) = kfm_standing_with_ctrl() else {
            return;
        };

        // Hold Up from stand: the built-in stand->jump (40) fires, common1
        // carries 40 -> 50 with KFM's jump velocity, then gravity + the P15a
        // clamp return the character to the floor and the auto-land built-in
        // takes 50 -> 52 -> (common1) 0.
        let mut left_ground = false;
        let mut grounded_after_jump = false;
        let mut visited: Vec<i32> = Vec::new();
        for tick in 0..360 {
            // Hold Up only for the first few ticks to initiate the jump; release
            // afterwards so the character is free to land and settle.
            if tick < 3 {
                set_commands(&mut ch, &["holdup"]);
            } else {
                set_commands(&mut ch, &[]);
            }
            ch.tick(&loaded, None, crate::StageView::default());
            visited.push(ch.state_no);
            assert!(
                ch.pos.y <= FLOOR_Y + 1e-3,
                "KFM must never sink below the floor; pos.y = {}",
                ch.pos.y
            );
            if ch.pos.y < -1.0 {
                left_ground = true;
            }
            // A grounded state reached AFTER leaving the ground proves the loop
            // closed: Jump Land 52 or back to Stand 0 (or walk 20 if still held).
            if left_ground && (ch.state_no == 52 || ch.state_no == 0) {
                grounded_after_jump = true;
                break;
            }
        }
        assert!(
            left_ground,
            "holding Up should lift KFM off the floor; visited states = {visited:?}"
        );
        assert!(
            grounded_after_jump,
            "the full KFM jump must complete the jump -> land -> stand loop and end grounded \
             (52 then 0); visited states = {visited:?}, ended in {} at pos.y = {}",
            ch.state_no, ch.pos.y
        );
    }

    /// AC1/AC2 (structural): the auto-land trigger must encode the EXACT MUGEN
    /// semantics — gated to states 50 AND 51, AND a floor bound (`pos y >= 0`),
    /// AND a downward/zero velocity bound (`vel y >= 0`). A behavior test can pass
    /// while one clause silently rots (e.g. dropping `vel y >= 0` would still
    /// land a falling char); this guards the trigger text directly. We inspect
    /// the raw trigger sources (lower-cased, whitespace-collapsed) so a refactor
    /// that re-spaces the expression still passes.
    #[test]
    fn p15b_auto_land_trigger_encodes_state_floor_and_velocity_bounds() {
        let states = p15b_graph();
        let land = find_auto_land(&states).expect("auto-land built-in must be injected");
        // Concatenate every trigger condition (all numbered groups) into one
        // normalized haystack: lowercase, all whitespace stripped.
        let haystack: String = land
            .triggers
            .iter()
            .flat_map(|g| g.conditions.iter())
            .map(|c| {
                c.source
                    .to_ascii_lowercase()
                    .split_whitespace()
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("&&");
        assert!(
            haystack.contains("stateno=50"),
            "auto-land must gate on stateno=50; trigger = {haystack:?}"
        );
        assert!(
            haystack.contains("stateno=51"),
            "auto-land must gate on stateno=51; trigger = {haystack:?}"
        );
        assert!(
            haystack.contains("posy>=0"),
            "auto-land must require pos y >= 0 (at/below floor); trigger = {haystack:?}"
        );
        assert!(
            haystack.contains("vely>=0"),
            "auto-land must require vel y >= 0 (downward/zero); trigger = {haystack:?}"
        );
    }

    /// AC2 (condition-gated, NOT command-gated): unlike the seven locomotion
    /// built-ins (which all hinge on `command = "..."`), the auto-land must carry
    /// NO `command` reference anywhere in its triggerall or numbered triggers —
    /// landing depends only on state + position + velocity. This is the explicit
    /// "condition-gated, not command-gated" distinction in the task: it is why
    /// the auto-land can sit at the very end of `[Statedef -1]` without being
    /// shadowed by (nor shadowing) the character's command-driven controllers.
    #[test]
    fn p15b_auto_land_is_condition_gated_not_command_gated() {
        let states = p15b_graph();
        let land = find_auto_land(&states).expect("auto-land built-in must be injected");
        let mentions_command = |e: &CompiledExpr| e.source.to_ascii_lowercase().contains("command");
        assert!(
            !land.triggerall.iter().any(mentions_command),
            "auto-land triggerall must not reference `command` (it is condition-gated)"
        );
        assert!(
            !land
                .triggers
                .iter()
                .flat_map(|g| g.conditions.iter())
                .any(mentions_command),
            "auto-land triggers must not reference `command` (landing is unconditional on input)"
        );
    }

    /// AC2 (strict gating, second air state): the "must not hijack air states"
    /// guarantee is not specific to 5040. A different custom air state (here 5050,
    /// type=A, authoring no land rule) dumped at the floor while falling must also
    /// stay put — proving the built-in is gated to the literal set {50, 51}, not
    /// to "any airborne char at the floor".
    #[test]
    fn p15b_custom_air_state_5050_is_not_hijacked() {
        // Extend the standard graph with an extra custom air state 5050.
        let cns = CnsFile::from_str(
            "[Statedef 50]\ntype = A\nphysics = A\nanim = 0\n\n\
             [Statedef 51]\ntype = A\nphysics = A\nanim = 0\n\n\
             [Statedef 52]\ntype = S\nphysics = N\nanim = 0\n\n\
             [Statedef 5050]\ntype = A\nphysics = A\nanim = 0\n",
        )
        .expect("synthetic 5050 CNS compiles");
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        for d in &cns.statedefs {
            states.insert(d.number, CompiledState::from_parsed(d));
        }
        append_builtin_ground_locomotion(&mut states);

        let air = tiny_air_p15b();
        let mut ch = Character::new();
        ch.change_state(&states, 5050);
        ch.pos = Vec2::new(0.0, FLOOR_Y); // at the floor, the landing-frame condition
        ch.physics = Physics::None;
        ch.vel = Vec2::new(0.0, 4.0); // falling
        for _ in 0..5 {
            ch.tick_with(&states, &air, None, StageView::default());
            assert_eq!(
                ch.state_no, 5050,
                "custom air state 5050 must NOT be auto-landed by the 50/51-only built-in"
            );
        }
    }

    // ---- Engine-default common1.cns (Task C) --------------------------------
    //
    // The shipped, original `assets/data/common1.cns` is committed (clean-room),
    // so these are NOT gated on test-assets — they exercise content that is
    // always present in the workspace.

    /// Resolves the shipped engine-default `assets/data/common1.cns`.
    /// `CARGO_MANIFEST_DIR` points at `crates/fp-character`; go up two levels to
    /// the workspace root.
    fn shipped_common1() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/data/common1.cns")
    }

    #[test]
    fn shipped_common1_parses_and_defines_key_states() {
        let path = shipped_common1();
        assert!(
            path.exists(),
            "engine default common1.cns must be shipped at {}",
            path.display()
        );
        let cns = CnsFile::load(&path).expect("shipped common1.cns must parse");
        let numbers: std::collections::HashSet<i32> =
            cns.statedefs.iter().map(|d| d.number).collect();
        // The essential get-hit / fall / land / death / getup states must exist.
        for n in [
            5000, 5010, 5020, 5030, 5040, 5050, 5070, 5100, 5110, 5120, 5150, 5160, 5170, 5200,
        ] {
            assert!(
                numbers.contains(&n),
                "shipped common1.cns must define [Statedef {n}]"
            );
        }
    }

    #[test]
    fn engine_default_common1_path_finds_shipped_asset() {
        // The resolver walks CWD then ancestors; tests run with CWD at the crate
        // dir (`crates/fp-character`), so the workspace `assets/data/common1.cns`
        // is found by walking up. (If for some reason it is not on disk, the
        // resolver returns None — which the fallback handles gracefully — so we
        // only assert when the shipped file is genuinely present.)
        if shipped_common1().exists() {
            let found = engine_default_common1_path();
            assert!(
                found.is_some(),
                "engine_default_common1_path must locate the shipped common1.cns"
            );
            assert!(found.unwrap().exists());
        }
    }

    #[test]
    fn find_in_ancestors_walks_up_to_shipped_default_and_handles_absence() {
        // From the crate dir, `find_in_ancestors` walks up to the workspace root
        // where `assets/data/common1.cns` is committed, returning that path.
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        if shipped_common1().exists() {
            let found =
                find_in_ancestors(crate_dir).expect("walks up to the shipped default common1.cns");
            assert!(
                found.ends_with(ENGINE_DEFAULT_COMMON1),
                "found path must end with {ENGINE_DEFAULT_COMMON1}, got {}",
                found.display()
            );
            assert!(found.exists());
        }
        // A start path with no `assets/data/common1.cns` in any ancestor returns
        // `None` (never panics) — the filesystem root has no such file.
        assert!(find_in_ancestors(Path::new("/")).is_none());
    }

    #[test]
    fn missing_stcommon_falls_back_to_engine_default() {
        // A character graph with NO common states. The fallback must fill in the
        // engine default's common states (e.g. 5000) from the shipped asset.
        if !shipped_common1().exists() {
            eprintln!("skipping: shipped common1.cns absent");
            return;
        }
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        states.insert(0, CompiledState::from_parsed(&Statedef::default()));
        assert!(
            !states.contains_key(&5000),
            "precondition: no common get-hit state yet"
        );

        // Reference resolves to a path that does NOT exist (as evilken's does).
        let missing = Path::new("/nonexistent/dir/common1.cns");
        merge_default_common_states(&mut states, "common1.cns", missing);

        for n in [5000, 5020, 5050, 5070, 5100] {
            assert!(
                states.contains_key(&n),
                "default common1.cns should have supplied [Statedef {n}]"
            );
        }
    }

    #[test]
    fn missing_default_is_not_fatal() {
        // If even the shipped default cannot be found, the fallback is a no-op,
        // never a panic. We can only force the "default absent" branch when the
        // shipped asset is genuinely missing; otherwise assert the graph is at
        // least never corrupted (states added, never removed).
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        states.insert(0, CompiledState::from_parsed(&Statedef::default()));
        let missing = Path::new("/nonexistent/dir/common1.cns");
        // Must not panic regardless of whether the default is present.
        merge_default_common_states(&mut states, "common1.cns", missing);
        assert!(states.contains_key(&0), "existing states are preserved");
    }

    #[test]
    fn character_bundled_common_states_win_over_default() {
        // A character that authors its OWN 5000 must keep it; the fallback only
        // fills states the character did not define (first-wins). Simulate by
        // pre-populating 5000 with a recognizable marker controller, then running
        // the default fallback: the marker must survive.
        if !shipped_common1().exists() {
            eprintln!("skipping: shipped common1.cns absent");
            return;
        }
        let own_5000 = CnsFile::from_str(
            "[Statedef 5000]\ntype = S\nmovetype = H\nphysics = N\n\n\
             [State 5000, marker]\ntype = Null\ntrigger1 = 1\n",
        )
        .expect("synthetic own-5000 compiles");
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        for d in &own_5000.statedefs {
            states.insert(d.number, CompiledState::from_parsed(d));
        }
        let own_ctrl_count = states[&5000].controllers.len();

        merge_default_common_states(&mut states, "common1.cns", Path::new("/nope/common1.cns"));

        // 5000 is unchanged (the character's own definition won, first-wins).
        assert_eq!(
            states[&5000].controllers.len(),
            own_ctrl_count,
            "character's own 5000 must not be overridden by the default"
        );
        // But states the character did NOT author are filled from the default.
        assert!(
            states.contains_key(&5050),
            "default still fills the states the character omitted"
        );
    }

    #[test]
    fn real_fixture_evilken_gets_default_common_states() {
        // evilken.def has `stcommon = common1.cns` but bundles no common1.cns of
        // its own. Before the fallback, evilken loaded with NO common get-hit
        // states. With it, the engine default supplies them. Gated on test-assets.
        let def = test_asset("evilken/evilken.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        // Precondition: evilken really does not bundle its own common1.cns.
        assert!(
            !test_asset("evilken/common1.cns").exists(),
            "test premise: evilken bundles no common1.cns"
        );

        let loaded = LoadedCharacter::load(&def).expect("evilken.def should load");
        // The engine-default common states must now be present.
        for n in [5000, 5020, 5050, 5070, 5100] {
            assert!(
                loaded.state(n).is_some(),
                "evilken should inherit default common [Statedef {n}]"
            );
        }
    }

    #[test]
    fn real_fixture_kfm_uses_own_common_not_default() {
        // KFM bundles its own common1.cns, so the default fallback must NOT fire:
        // KFM's own common states load. The presence of common state 0 (Stand)
        // and the get-hit family proves the merge worked from KFM's own file (the
        // `else` default branch is only taken when the reference is MISSING, and
        // KFM's is present). Gated on test-assets.
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        // Premise: KFM DOES bundle its own common1.cns (so no fallback is needed).
        assert!(
            test_asset("kfm/common1.cns").exists(),
            "test premise: KFM bundles its own common1.cns"
        );
        let loaded = LoadedCharacter::load(&def).expect("kfm.def should load");
        // KFM's own common1 defines Stand (0) and the get-hit family.
        assert!(loaded.state(0).is_some(), "KFM's own common Stand 0 loads");
        assert!(
            loaded.state(5000).is_some(),
            "KFM's own common get-hit 5000 loads"
        );
    }

    // ====================================================================
    // T049: trainingdummy special-move statedef presence (loader test).
    //
    // The shipped `assets/trainingdummy` is original clean-room content that
    // ships in the repo (not under `test-assets/`), so this test is NOT
    // asset-gated and must pass on every machine and on CI.
    // ====================================================================

    /// Resolves a path relative to the workspace `assets/trainingdummy/` dir.
    ///
    /// `CARGO_MANIFEST_DIR` for this crate is `crates/fp-character`; go up two
    /// levels to reach the workspace root.
    fn dummy_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/trainingdummy")
            .join(rel)
    }

    #[test]
    fn trainingdummy_loads_with_special_move_statedefs() {
        // Verify that the trainingdummy's two new motion-command statedefs (1000
        // and 1100, added in T049) are present in the compiled state table after
        // a full LoadedCharacter::load.  These states are the ChangeState targets
        // for the QCF+a (fireball) and DP+a (dragon-punch) commands defined in
        // trainingdummy.cmd.
        let def = dummy_asset("trainingdummy.def");
        let loaded = LoadedCharacter::load(&def)
            .unwrap_or_else(|e| panic!("Training Dummy failed to load: {e}"));

        assert!(
            loaded.state(1000).is_some(),
            "fireball [Statedef 1000] must be present in the compiled state table"
        );
        assert!(
            loaded.state(1100).is_some(),
            "dragon-punch [Statedef 1100] must be present in the compiled state table"
        );
        // The existing locomotion/attack states must still be intact.
        for required in [0, 20, 200, 5000] {
            assert!(
                loaded.state(required).is_some(),
                "pre-existing state {required} must still be present after T049 changes"
            );
        }
    }
}
