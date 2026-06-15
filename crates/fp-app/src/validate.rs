//! Content validator: loads a `.def`, detects what kind of content it is
//! (character / stage / scene), and produces an actionable lint report.
//!
//! This is the engine behind `fp-app validate <file.def>`. The CLI entry point
//! is [`validate_path`], which sniffs the `.def`'s sections (see [`ContentKind`])
//! and routes to the matching analyzer:
//!
//! - **Characters** → [`validate`] (loads through
//!   [`fp_character::LoadedCharacter::load`] — the exact path the live match
//!   uses — then statically inspects the compiled state graph and assets).
//! - **Stages** → [`validate_stage`] (parses through [`fp_stage::Stage`] and
//!   lints the `[Camera]`/`[PlayerInfo]`/`[StageInfo]`/`[BGdef]`/`[BG]` sections
//!   for missing/invalid fields and a missing/absent sprite container).
//! - **Scenes** → [`validate_scene`] (storyboard `.def` via
//!   [`fp_storyboard::Storyboard`] and/or `fight.def` screenpack via
//!   [`fp_ui::ScreenpackLayout`]; lints scene/HUD asset references).
//!
//! ## Character validation
//!
//! The character analyzer statically inspects the compiled result for the kinds
//! of authoring mistakes that otherwise fail *silently* at runtime (MUGEN content
//! is famously forgiving — a bad reference just renders nothing or never fires):
//!
//! - **Missing sprites** — an AIR frame references a `(group, image)` the SFF
//!   does not contain (the sprite would draw as invisible).
//! - **Unresolved state references** — a `ChangeState` / `SelfState` /
//!   `TargetState` jumps to a `stateno` that no statedef defines (the transition
//!   would be a dead-end).
//! - **Unresolved anim references** — a `ChangeAnim` switches to an `animno`
//!   that the AIR file has no action for (the anim would not change).
//! - **Failed expression compiles** — the loader silently substitutes a const-`0`
//!   fallback (and `tracing::warn!`s) for any expression that fails to parse; the
//!   report surfaces every such fallback (which trigger / parameter, and the raw
//!   source) so a typo in a trigger is not lost in the log.
//! - **Unsupported controllers** — controller types the executor does not yet
//!   handle (they fall through to a debug-logged safe no-op at runtime); the
//!   report lists each distinct unsupported type with a count.
//!
//! The analysis is **purely static** over the compiled state graph and assets;
//! it never ticks the character and never needs a window, GPU, or audio device,
//! so it is fully unit-testable. Findings are collected into a [`ValidationReport`]
//! that knows how to render itself to a human-readable string ([`render_report`]).
//!
//! Constant references only: a `value = 200` (an integer literal, possibly
//! negated) is resolved statically; a `value = var(1) + 100` (a runtime
//! expression) cannot be resolved without ticking, so it is **not** flagged as
//! unresolved (avoiding false positives). This limitation is documented in the
//! rendered report.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use fp_character::loader::{CompiledController, CompiledExpr, CompiledState};
use fp_character::LoadedCharacter;
use fp_formats::def::DefFile;
use fp_stage::{BgType, Stage};
use fp_storyboard::Storyboard;
use fp_ui::ScreenpackLayout;
use fp_vm::Expr;

/// The kind of MUGEN content a `.def` describes, inferred from its sections.
///
/// `fp-app validate` accepts any `.def` and routes it to the right analyzer; the
/// detector is heuristic (MUGEN files carry no explicit type tag) but the section
/// sets are disjoint enough in practice to classify reliably. See
/// [`detect_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// A playable character: a `[Files]` section referencing a `sprite`/`anim`
    /// (`.sff`/`.air`). Routed to [`validate`].
    Character,
    /// A stage: a `[BGdef]`/`[StageInfo]`/`[Camera]`/`[PlayerInfo]` section set.
    /// Routed to [`validate_stage`].
    Stage,
    /// A storyboard scene file: a `[SceneDef]` or `[Scene N]` section. Routed to
    /// [`validate_scene`].
    Storyboard,
    /// A screenpack / `fight.def`: a `[Lifebar]`/`[Powerbar]`/`[Round]` section
    /// set. Routed to [`validate_scene`].
    Screenpack,
}

impl ContentKind {
    /// A short human label for the report header.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ContentKind::Character => "character",
            ContentKind::Stage => "stage",
            ContentKind::Storyboard => "storyboard / scene",
            ContentKind::Screenpack => "screenpack (fight.def)",
        }
    }
}

/// Detects the [`ContentKind`] of an already-parsed `.def`.
///
/// Heuristic, in priority order (a real `.def` rarely satisfies more than one):
///
/// 1. **Character** — has `[Files]` with a `sprite` *or* `anim` key (the
///    `.sff`/`.air` a character cannot do without). This is checked first because
///    a character `.def` also carries an `[Info]` block that other kinds share.
/// 2. **Stage** — has a `[BGdef]`, `[StageInfo]`, `[PlayerInfo]`, or `[Camera]`
///    section (and is not a character).
/// 3. **Storyboard** — has a `[SceneDef]` section or any `[Scene <n>]` section.
/// 4. **Screenpack** — has a `[Lifebar]`, `[Powerbar]`, `[Round]`, or `[Time]`
///    section.
///
/// Returns `None` when nothing matches (an empty or unrecognizable `.def`); the
/// caller treats that as "cannot classify" rather than guessing.
#[must_use]
pub fn detect_kind(def: &DefFile) -> Option<ContentKind> {
    let has_section = |name: &str| def.sections.contains_key(name);
    let files_has = |key: &str| def.get("Files", key).is_some_and(|v| !v.trim().is_empty());

    // 1. Character: a [Files] sprite/anim reference.
    if has_section("files") && (files_has("sprite") || files_has("anim")) {
        return Some(ContentKind::Character);
    }

    // 2. Stage.
    if has_section("bgdef")
        || has_section("stageinfo")
        || has_section("playerinfo")
        || has_section("camera")
    {
        return Some(ContentKind::Stage);
    }

    // 3. Storyboard: a [SceneDef] or any [Scene N] section.
    if has_section("scenedef")
        || def
            .sections
            .keys()
            .any(|s| s == "scene" || s.starts_with("scene "))
    {
        return Some(ContentKind::Storyboard);
    }

    // 4. Screenpack / fight.def.
    if has_section("lifebar")
        || has_section("powerbar")
        || has_section("round")
        || has_section("time")
    {
        return Some(ContentKind::Screenpack);
    }

    None
}

/// The set of controller types the `fp-character` executor currently dispatches.
///
/// Kept in sync with the executor's dispatch chain (`executor.rs`): any type not
/// in this set falls through to the executor's safe no-op branch at runtime, and
/// the validator reports it as unsupported. Compared case-insensitively (MUGEN
/// controller names are case-insensitive).
const SUPPORTED_CONTROLLERS: &[&str] = &[
    "ChangeState",
    "SelfState",
    "VelSet",
    "VelAdd",
    "VelMul",
    "CtrlSet",
    "PosSet",
    "PosAdd",
    "ChangeAnim",
    "ChangeAnim2",
    "VarSet",
    "VarAdd",
    "VarRangeSet",
    "PowerAdd",
    "PowerSet",
    "AttackMulSet",
    "DefenceMulSet",
    "StateTypeSet",
    "Turn",
    "PlaySnd",
    "HitDef",
    "NotHitBy",
    "HitBy",
    "TargetState",
    "TargetBind",
    "TargetLifeAdd",
    "TargetFacing",
    "TargetVelSet",
    "TargetVelAdd",
    "TargetPowerAdd",
    "AssertSpecial",
    "Width",
    "HitVelSet",
    "HitFallSet",
    "HitFallVel",
    "HitFallDamage",
    "HitOverride",
    "SprPriority",
    "Null",
];

/// A single AIR frame that references a sprite missing from the SFF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSprite {
    /// The AIR action (animation) number the frame belongs to.
    pub action: i32,
    /// The zero-based frame index within that action.
    pub frame: usize,
    /// The referenced sprite group.
    pub group: u16,
    /// The referenced sprite image.
    pub image: u16,
}

/// A controller transition that targets a state number no statedef defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedState {
    /// The state the offending controller lives in.
    pub from_state: i32,
    /// The controller type (`ChangeState`, `SelfState`, `TargetState`).
    pub controller: String,
    /// The controller's free-form label, for locating it in the source.
    pub label: String,
    /// The target state number that does not exist.
    pub target: i32,
}

/// A `ChangeAnim` that targets an animation number the AIR file lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedAnim {
    /// The state the offending controller lives in.
    pub from_state: i32,
    /// The controller's free-form label.
    pub label: String,
    /// The target animation number that does not exist.
    pub target: i32,
}

/// An expression that failed to compile and was replaced with a const-`0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedExpr {
    /// The state the expression belongs to.
    pub from_state: i32,
    /// A human-readable location within the state (e.g. `trigger1` or a
    /// parameter name like `value`).
    pub site: String,
    /// The raw, un-parseable source text.
    pub source: String,
}

/// The full result of validating a character: counts + actionable findings.
///
/// Every list is empty for a clean character. [`render_report`] turns this into
/// the user-facing text the `validate` subcommand prints.
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    /// `[Info] name` of the character (empty if unset).
    pub name: String,
    /// Number of compiled states in the merged graph.
    pub state_count: usize,
    /// Number of sprites in the SFF.
    pub sprite_count: usize,
    /// Number of AIR animation actions.
    pub anim_count: usize,
    /// Whether the character referenced and loaded a `.snd` file.
    pub has_sound: bool,
    /// AIR frames referencing sprites absent from the SFF.
    pub missing_sprites: Vec<MissingSprite>,
    /// Controllers jumping to a non-existent state number.
    pub unresolved_states: Vec<UnresolvedState>,
    /// `ChangeAnim`s switching to a non-existent animation number.
    pub unresolved_anims: Vec<UnresolvedAnim>,
    /// Expressions that failed to compile (silent const-`0` fallbacks).
    pub failed_exprs: Vec<FailedExpr>,
    /// Distinct unsupported controller types -> how many times each appears.
    pub unsupported_controllers: BTreeMap<String, usize>,
}

impl ValidationReport {
    /// `true` when no actionable problems were found (the character is clean).
    /// Unsupported controllers are an *advisory*, not a failure, so they do not
    /// affect this predicate.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.missing_sprites.is_empty()
            && self.unresolved_states.is_empty()
            && self.unresolved_anims.is_empty()
            && self.failed_exprs.is_empty()
    }

    /// Total number of actionable problems (sum of every finding list except the
    /// advisory unsupported-controller tally).
    #[must_use]
    pub fn problem_count(&self) -> usize {
        self.missing_sprites.len()
            + self.unresolved_states.len()
            + self.unresolved_anims.len()
            + self.failed_exprs.len()
    }
}

/// Loads the character at `def_path` and produces a [`ValidationReport`].
///
/// # Errors
///
/// Returns the loader's [`fp_core::FpError`] only when the character cannot be
/// loaded at all (e.g. a missing required SFF/AIR, or no states) — the same
/// fatal conditions [`fp_character::LoadedCharacter::load`] reports. A character
/// that loads but has authoring problems returns `Ok` with those problems
/// recorded in the report (never an error, never a panic).
pub fn validate(def_path: &Path) -> fp_core::FpResult<ValidationReport> {
    let loaded = LoadedCharacter::load(def_path)?;
    Ok(analyze(&loaded))
}

/// Builds a [`ValidationReport`] from an already-loaded character.
///
/// Split out from [`validate`] so the analysis can be unit-tested against a
/// [`LoadedCharacter`] built in-memory without touching the filesystem twice.
#[must_use]
pub fn analyze(loaded: &LoadedCharacter) -> ValidationReport {
    let mut report = ValidationReport {
        name: loaded.name.clone(),
        state_count: loaded.state_count(),
        sprite_count: loaded.sff.sprites.len(),
        anim_count: loaded.air.actions.len(),
        has_sound: loaded.snd.is_some(),
        ..Default::default()
    };

    check_missing_sprites(loaded, &mut report);
    check_states_and_anims(loaded, &mut report);

    report
}

/// Records every AIR frame whose sprite is not present in the SFF.
fn check_missing_sprites(loaded: &LoadedCharacter, report: &mut ValidationReport) {
    // The set of sprite (group, image) pairs the SFF actually contains.
    let present: BTreeSet<(u16, u16)> = loaded
        .sff
        .sprites
        .iter()
        .map(|s| (s.group, s.image))
        .collect();

    // Iterate actions in a stable (ascending) order so the report is
    // deterministic regardless of the AIR HashMap's iteration order.
    let mut action_numbers: Vec<i32> = loaded.air.actions.keys().copied().collect();
    action_numbers.sort_unstable();

    for action_no in action_numbers {
        let Some(action) = loaded.air.actions.get(&action_no) else {
            continue;
        };
        for (frame_idx, frame) in action.frames.iter().enumerate() {
            let g = frame.sprite.group();
            let i = frame.sprite.image();
            if !present.contains(&(g, i)) {
                report.missing_sprites.push(MissingSprite {
                    action: action_no,
                    frame: frame_idx,
                    group: g,
                    image: i,
                });
            }
        }
    }
}

/// Walks every compiled controller, recording unresolved state/anim targets,
/// failed-compile expressions, and unsupported controller types.
fn check_states_and_anims(loaded: &LoadedCharacter, report: &mut ValidationReport) {
    // Iterate states in a stable order for a deterministic report.
    let mut state_numbers: Vec<i32> = loaded.states.keys().copied().collect();
    state_numbers.sort_unstable();

    for state_no in state_numbers {
        let Some(state) = loaded.states.get(&state_no) else {
            continue;
        };
        check_state_header_exprs(state, report);
        for ctrl in &state.controllers {
            check_controller(loaded, state_no, ctrl, report);
        }
    }
}

/// Records failed-compile fallbacks in a statedef *header* expression
/// (`anim`, `ctrl`, `poweradd`, `sprpriority`, `juggle`, `facep2`,
/// `hitdefpersist`, `movehitpersist`).
fn check_state_header_exprs(state: &CompiledState, report: &mut ValidationReport) {
    let headers: [(&str, &Option<CompiledExpr>); 8] = [
        ("statedef anim", &state.anim),
        ("statedef ctrl", &state.ctrl),
        ("statedef poweradd", &state.poweradd),
        ("statedef sprpriority", &state.sprpriority),
        ("statedef juggle", &state.juggle),
        ("statedef facep2", &state.facep2),
        ("statedef hitdefpersist", &state.hitdefpersist),
        ("statedef movehitpersist", &state.movehitpersist),
    ];
    for (site, maybe_expr) in headers {
        if let Some(expr) = maybe_expr {
            record_if_fallback(state.number, site, expr, report);
        }
    }
}

/// Analyzes one controller: unsupported type, unresolved state/anim targets,
/// and every failed-compile expression (triggers + parameters + universal
/// params).
fn check_controller(
    loaded: &LoadedCharacter,
    state_no: i32,
    ctrl: &CompiledController,
    report: &mut ValidationReport,
) {
    let kind = ctrl.controller_type.as_deref().unwrap_or("");

    // --- Unsupported controller type (advisory) ---
    if !kind.is_empty() && !is_supported(kind) {
        *report
            .unsupported_controllers
            .entry(kind.to_string())
            .or_insert(0) += 1;
    }

    // --- Unresolved state target (ChangeState / SelfState / TargetState) ---
    if kind.eq_ignore_ascii_case("ChangeState")
        || kind.eq_ignore_ascii_case("SelfState")
        || kind.eq_ignore_ascii_case("TargetState")
    {
        if let Some(target) = controller_const_value(ctrl) {
            if !loaded.states.contains_key(&target) {
                report.unresolved_states.push(UnresolvedState {
                    from_state: state_no,
                    controller: kind.to_string(),
                    label: ctrl.label.clone(),
                    target,
                });
            }
        }
    }

    // --- Unresolved anim target (ChangeAnim / ChangeAnim2) ---
    if kind.eq_ignore_ascii_case("ChangeAnim") || kind.eq_ignore_ascii_case("ChangeAnim2") {
        if let Some(target) = controller_const_value(ctrl) {
            if loaded.air.action(target).is_none() {
                report.unresolved_anims.push(UnresolvedAnim {
                    from_state: state_no,
                    label: ctrl.label.clone(),
                    target,
                });
            }
        }
    }

    // --- Failed-compile expressions in this controller ---
    for (i, expr) in ctrl.triggerall.iter().enumerate() {
        record_if_fallback(state_no, &format!("triggerall[{i}]"), expr, report);
    }
    for group in &ctrl.triggers {
        for expr in &group.conditions {
            record_if_fallback(state_no, &format!("trigger{}", group.number), expr, report);
        }
    }
    if let Some(p) = &ctrl.persistent {
        record_if_fallback(state_no, "persistent", p, report);
    }
    if let Some(p) = &ctrl.ignorehitpause {
        record_if_fallback(state_no, "ignorehitpause", p, report);
    }
    // Parameters: iterate the map in a stable (sorted) key order.
    let mut param_names: Vec<&String> = ctrl.params.keys().collect();
    param_names.sort();
    for name in param_names {
        if let Some(param) = ctrl.params.get(name) {
            for (i, comp) in param.components.iter().enumerate() {
                let site = if param.components.len() > 1 {
                    format!("param {name}[{i}]")
                } else {
                    format!("param {name}")
                };
                record_if_fallback(state_no, &site, comp, report);
            }
        }
    }
}

/// Pushes a [`FailedExpr`] when `expr` is the const-`0` fallback for a failed
/// parse. A clean (non-fallback) expression records nothing.
fn record_if_fallback(
    state_no: i32,
    site: &str,
    expr: &CompiledExpr,
    report: &mut ValidationReport,
) {
    if expr.is_fallback {
        report.failed_exprs.push(FailedExpr {
            from_state: state_no,
            site: site.to_string(),
            source: expr.source.clone(),
        });
    }
}

/// Returns the controller's `value` parameter as a constant `i32` when it is a
/// plain integer literal (optionally negated), else `None`.
///
/// MUGEN authors the overwhelming majority of `ChangeState value = N` /
/// `ChangeAnim value = N` as literal numbers, so resolving these statically
/// catches the common typo (a jump to a state that does not exist). A `value`
/// that is a *runtime expression* (`var(1)`, `stateno + 1`, …) cannot be
/// resolved without ticking the character, so it returns `None` and is left
/// un-flagged rather than producing a false positive.
fn controller_const_value(ctrl: &CompiledController) -> Option<i32> {
    let param = ctrl.params.get("value")?;
    let comp = param.component(0)?;
    if comp.is_fallback {
        // A failed-compile value is already reported as a FailedExpr; don't
        // also (mis)report it as an unresolved-state reference.
        return None;
    }
    const_int(&comp.expr)
}

/// Resolves an [`Expr`] to a constant `i32` when it is an integer literal or the
/// unary negation of one. Anything else (a float, an identifier, a call, an
/// arithmetic expression) returns `None`.
fn const_int(expr: &Expr) -> Option<i32> {
    match expr {
        Expr::Int(n) => i32::try_from(*n).ok(),
        Expr::Unary {
            op: fp_vm::UnaryOp::Neg,
            operand,
        } => const_int(operand).map(|v| -v),
        _ => None,
    }
}

/// `true` if `kind` is a controller type the executor dispatches (matched
/// case-insensitively against [`SUPPORTED_CONTROLLERS`]).
fn is_supported(kind: &str) -> bool {
    SUPPORTED_CONTROLLERS
        .iter()
        .any(|c| c.eq_ignore_ascii_case(kind))
}

/// The clean-room license/usage reminder printed at the foot of every report.
pub(crate) const LICENSE_REMINDER: &str = "\
Reminder: Fighters Paradise is a clean-room MUGEN-compatible engine (MIT). Bring \
your own characters — ship ONLY content you have the right to distribute. MUGEN \
is a trademark of Elecbyte; this tool ships no Elecbyte assets.";

/// Renders a [`ValidationReport`] into the multi-line, human-readable text the
/// `validate` subcommand prints to stdout.
///
/// The output leads with a one-line summary, then a section per finding kind
/// (omitted when empty), the unsupported-controller advisory, the static-analysis
/// limitation note, and the clean-room license reminder. Deterministic for a
/// given report (every finding list is produced in a stable order by [`analyze`]).
#[must_use]
pub fn render_report(report: &ValidationReport) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "Character validation report: {}\n",
        if report.name.is_empty() {
            "<unnamed>"
        } else {
            &report.name
        }
    ));
    out.push_str(&format!(
        "  states: {}   sprites: {}   animations: {}   sound: {}\n",
        report.state_count,
        report.sprite_count,
        report.anim_count,
        if report.has_sound { "yes" } else { "no" }
    ));

    if report.is_clean() {
        out.push_str("\nResult: PASS — no authoring problems found.\n");
    } else {
        out.push_str(&format!(
            "\nResult: {} problem(s) found.\n",
            report.problem_count()
        ));
    }

    if !report.missing_sprites.is_empty() {
        out.push_str(&format!(
            "\nMissing sprites ({}): AIR frames reference sprites absent from the SFF\n",
            report.missing_sprites.len()
        ));
        for m in &report.missing_sprites {
            out.push_str(&format!(
                "  - action {} frame {} -> sprite ({}, {}) not in SFF\n",
                m.action, m.frame, m.group, m.image
            ));
        }
    }

    if !report.unresolved_states.is_empty() {
        out.push_str(&format!(
            "\nUnresolved state references ({}): jumps to a non-existent stateno\n",
            report.unresolved_states.len()
        ));
        for u in &report.unresolved_states {
            out.push_str(&format!(
                "  - state {} {} [{}] -> state {} does not exist\n",
                u.from_state, u.controller, u.label, u.target
            ));
        }
    }

    if !report.unresolved_anims.is_empty() {
        out.push_str(&format!(
            "\nUnresolved anim references ({}): ChangeAnim to a non-existent animno\n",
            report.unresolved_anims.len()
        ));
        for u in &report.unresolved_anims {
            out.push_str(&format!(
                "  - state {} ChangeAnim [{}] -> anim {} does not exist\n",
                u.from_state, u.label, u.target
            ));
        }
    }

    if !report.failed_exprs.is_empty() {
        out.push_str(&format!(
            "\nFailed expression compiles ({}): replaced with const-0 (would never \
             fire / read as 0)\n",
            report.failed_exprs.len()
        ));
        for f in &report.failed_exprs {
            out.push_str(&format!(
                "  - state {} {}: {:?}\n",
                f.from_state, f.site, f.source
            ));
        }
    }

    if !report.unsupported_controllers.is_empty() {
        let total: usize = report.unsupported_controllers.values().sum();
        out.push_str(&format!(
            "\nUnsupported controllers ({total} use(s), advisory): these run as a \
             safe no-op for now\n"
        ));
        for (kind, count) in &report.unsupported_controllers {
            out.push_str(&format!("  - {kind} ({count})\n"));
        }
    }

    out.push_str(
        "\nNote: unresolved-reference checks resolve only constant (literal) \
         state/anim targets;\n      a target given as a runtime expression \
         (e.g. var(1), stateno+1) is not checked.\n",
    );

    out.push('\n');
    out.push_str(LICENSE_REMINDER);
    out.push('\n');

    out
}

// ===========================================================================
// Top-level dispatch: detect the content kind and route to its analyzer
// ===========================================================================

/// The outcome of validating a `.def`, tagged by the kind of content found.
///
/// Produced by [`validate_path`]; rendered by [`render_any`]. A
/// [`ContentKind::Character`] yields the existing [`ValidationReport`]; a stage a
/// [`StageReport`]; a storyboard/screenpack a [`SceneReport`].
#[derive(Debug, Clone)]
pub enum AnyReport {
    /// A character validation report.
    Character(ValidationReport),
    /// A stage validation report.
    Stage(StageReport),
    /// A scene (storyboard or screenpack) validation report.
    Scene(SceneReport),
}

impl AnyReport {
    /// `true` when no actionable problems were found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        match self {
            AnyReport::Character(r) => r.is_clean(),
            AnyReport::Stage(r) => r.is_clean(),
            AnyReport::Scene(r) => r.is_clean(),
        }
    }
}

/// Loads the `.def` at `def_path`, detects its [`ContentKind`], and produces the
/// matching report.
///
/// This is the CLI entry point behind `fp-app validate <file.def>` for *any*
/// content type. It parses the `.def` once (to classify it), then routes:
/// characters through [`validate`], stages through [`validate_stage`], and
/// storyboards/screenpacks through [`validate_scene`].
///
/// # Errors
///
/// Returns [`fp_core::FpError`] when the `.def` cannot be read/parsed at all, when
/// its kind cannot be classified (an empty or unrecognizable `.def`), or — for a
/// character — when the underlying character cannot be loaded (a missing required
/// SFF/AIR). A file that parses and classifies but has authoring problems returns
/// `Ok` with those problems recorded in the report (never an error, never a panic).
pub fn validate_path(def_path: &Path) -> fp_core::FpResult<AnyReport> {
    let def = DefFile::load(def_path)?;
    let kind = detect_kind(&def).ok_or_else(|| fp_core::FpError::Parse {
        format: "DEF",
        message: format!(
            "cannot classify {} as a character, stage, or scene .def \
             (no recognizable sections)",
            def_path.display()
        ),
    })?;

    tracing::info!(
        "validate: detected {} ({})",
        kind.label(),
        def_path.display()
    );

    match kind {
        ContentKind::Character => Ok(AnyReport::Character(validate(def_path)?)),
        ContentKind::Stage => Ok(AnyReport::Stage(validate_stage(def_path)?)),
        ContentKind::Storyboard => {
            Ok(AnyReport::Scene(validate_scene(def_path, ContentKind::Storyboard)?))
        }
        ContentKind::Screenpack => {
            Ok(AnyReport::Scene(validate_scene(def_path, ContentKind::Screenpack)?))
        }
    }
}

/// Renders any [`AnyReport`] to the human-readable text the CLI prints.
#[must_use]
pub fn render_any(report: &AnyReport) -> String {
    match report {
        AnyReport::Character(r) => render_report(r),
        AnyReport::Stage(r) => render_stage_report(r),
        AnyReport::Scene(r) => render_scene_report(r),
    }
}

// ===========================================================================
// Stage validation
// ===========================================================================

/// A single missing/invalid field finding in a stage `.def`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageIssue {
    /// The `[Section]` the issue was found in (e.g. `Camera`, `BG Sky`).
    pub section: String,
    /// A human-readable description of the problem.
    pub detail: String,
}

/// The result of validating a stage `.def`: counts + actionable findings.
///
/// Every list is empty for a clean stage. [`render_stage_report`] turns this into
/// the user-facing text.
#[derive(Debug, Clone, Default)]
pub struct StageReport {
    /// `[Info] name` of the stage (empty if unset).
    pub name: String,
    /// Number of `[BG ...]` background elements parsed.
    pub bg_count: usize,
    /// The `[BGdef] spr` sprite-container path, if any.
    pub sprite_path: Option<String>,
    /// `true` if the `[BGdef] spr` file exists on disk and parses as an SFF.
    pub sprite_loaded: bool,
    /// Actionable problems (missing/invalid fields, bad asset references).
    pub issues: Vec<StageIssue>,
}

impl StageReport {
    /// `true` when no actionable problems were found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }

    /// Total number of actionable problems found.
    #[must_use]
    pub fn problem_count(&self) -> usize {
        self.issues.len()
    }

    /// Pushes a finding for `section` with the given `detail`.
    fn push(&mut self, section: impl Into<String>, detail: impl Into<String>) {
        self.issues.push(StageIssue {
            section: section.into(),
            detail: detail.into(),
        });
    }
}

/// Loads the stage at `def_path` and produces a [`StageReport`].
///
/// # Errors
///
/// Returns [`fp_core::FpError`] only when the `.def` itself cannot be read; a
/// stage that parses with authoring problems returns `Ok` with those recorded.
pub fn validate_stage(def_path: &Path) -> fp_core::FpResult<StageReport> {
    let stage = Stage::load(def_path)?;
    Ok(analyze_stage(&stage))
}

/// Builds a [`StageReport`] from an already-parsed [`Stage`].
///
/// Split out from [`validate_stage`] so the analysis is unit-testable against an
/// in-memory [`Stage`] (parsed from synthetic text) without touching the disk.
/// The `[BGdef] spr` file existence/parse check only runs when the stage carries
/// a resolved path (it is `Stage::load` that supplies an absolute one).
#[must_use]
pub fn analyze_stage(stage: &Stage) -> StageReport {
    let mut report = StageReport {
        name: stage.info.name.clone(),
        bg_count: stage.backgrounds.len(),
        sprite_path: stage
            .bgdef
            .sprite_path
            .as_ref()
            .map(|p| p.display().to_string()),
        ..Default::default()
    };

    // --- [BGdef] sprite container --------------------------------------
    match &stage.bgdef.sprite_path {
        None => {
            // A stage with no [BGdef] spr can only draw a flat clear color — flag
            // it as long as it actually declares background elements to draw.
            if !stage.backgrounds.is_empty() {
                report.push(
                    "BGdef",
                    format!(
                        "{} [BG] element(s) declared but no `spr` sprite container — \
                         backgrounds cannot draw",
                        stage.backgrounds.len()
                    ),
                );
            } else {
                report.push("BGdef", "no `spr` sprite container specified");
            }
        }
        Some(path) => {
            // Only check existence for an absolute/loadable path (Stage::load
            // resolves relative to the .def dir). A bare relative path from an
            // in-memory parse is left unchecked.
            if path.is_absolute() || path.exists() {
                if !path.exists() {
                    report.push(
                        "BGdef",
                        format!("`spr` file not found: {}", path.display()),
                    );
                } else if let Err(e) = fp_formats::sff::SffFile::load(path) {
                    report.push(
                        "BGdef",
                        format!("`spr` file {} failed to parse as SFF: {e}", path.display()),
                    );
                } else {
                    report.sprite_loaded = true;
                }
            }
        }
    }

    // --- [Camera] bounds sanity ----------------------------------------
    if stage.camera.bound_left > stage.camera.bound_right {
        report.push(
            "Camera",
            format!(
                "boundleft ({}) > boundright ({}) — the camera range is inverted",
                stage.camera.bound_left, stage.camera.bound_right
            ),
        );
    }
    if !(0.0..=1.0).contains(&stage.camera.vertical_follow) {
        report.push(
            "Camera",
            format!(
                "verticalfollow ({}) is outside the expected [0, 1] range",
                stage.camera.vertical_follow
            ),
        );
    }

    // --- [PlayerInfo] bounds sanity ------------------------------------
    if stage.player_info.left_bound > stage.player_info.right_bound {
        report.push(
            "PlayerInfo",
            format!(
                "leftbound ({}) > rightbound ({}) — fighters would be clamped to \
                 an empty range",
                stage.player_info.left_bound, stage.player_info.right_bound
            ),
        );
    }

    // --- [StageInfo] geometry sanity -----------------------------------
    if stage.stage_info.local_coord.x <= 0.0 || stage.stage_info.local_coord.y <= 0.0 {
        report.push(
            "StageInfo",
            format!(
                "localcoord ({}, {}) must be positive",
                stage.stage_info.local_coord.x, stage.stage_info.local_coord.y
            ),
        );
    }
    if stage.stage_info.x_scale <= 0.0 || stage.stage_info.y_scale <= 0.0 {
        report.push(
            "StageInfo",
            format!(
                "xscale/yscale ({}, {}) must be positive",
                stage.stage_info.x_scale, stage.stage_info.y_scale
            ),
        );
    }

    // --- Per-[BG] element sanity ---------------------------------------
    for (i, bg) in stage.backgrounds.iter().enumerate() {
        let section = if bg.name.is_empty() {
            format!("BG #{i}")
        } else {
            format!("BG {}", bg.name)
        };
        // An `anim` element with no actionno cannot animate.
        if bg.kind == BgType::Anim && bg.action_no.is_none() {
            report.push(
                section.clone(),
                "type = anim but no `actionno` given — the element cannot animate",
            );
        }
        // A negative sprite index is never valid.
        if bg.sprite.x < 0 || bg.sprite.y < 0 {
            report.push(
                section,
                format!(
                    "spriteno ({}, {}) has a negative component",
                    bg.sprite.x, bg.sprite.y
                ),
            );
        }
    }

    report
}

/// Renders a [`StageReport`] into the multi-line, human-readable text the
/// `validate` subcommand prints for a stage `.def`.
#[must_use]
pub fn render_stage_report(report: &StageReport) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "Stage validation report: {}\n",
        if report.name.is_empty() {
            "<unnamed>"
        } else {
            &report.name
        }
    ));
    out.push_str(&format!(
        "  background elements: {}   sprite container: {}\n",
        report.bg_count,
        match &report.sprite_path {
            Some(p) if report.sprite_loaded => format!("{p} (loaded)"),
            Some(p) => p.clone(),
            None => "<none>".to_string(),
        }
    ));

    if report.is_clean() {
        out.push_str("\nResult: PASS — no stage authoring problems found.\n");
    } else {
        out.push_str(&format!(
            "\nResult: {} problem(s) found.\n",
            report.problem_count()
        ));
        for issue in &report.issues {
            out.push_str(&format!("  - [{}] {}\n", issue.section, issue.detail));
        }
    }

    out.push('\n');
    out.push_str(LICENSE_REMINDER);
    out.push('\n');

    out
}

// ===========================================================================
// Scene validation (storyboard + screenpack)
// ===========================================================================

/// A single missing/invalid field finding in a scene (storyboard/screenpack)
/// `.def`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneIssue {
    /// A locator for where the problem is (e.g. `Scene 0 layer 1`, `Files`).
    pub location: String,
    /// A human-readable description of the problem.
    pub detail: String,
}

/// The result of validating a scene `.def` (storyboard or screenpack).
///
/// Every list is empty for a clean scene file. [`render_scene_report`] turns this
/// into the user-facing text.
#[derive(Debug, Clone, Default)]
pub struct SceneReport {
    /// Whether this report is for a screenpack (`true`) or a storyboard
    /// (`false`).
    pub is_screenpack: bool,
    /// Number of scenes (storyboard) or HUD elements counted (screenpack).
    pub scene_count: usize,
    /// The sprite-container path (`[SceneDef] spr` / `[Files] sff`), if any.
    pub sprite_path: Option<String>,
    /// Actionable problems (missing/invalid fields, bad asset references).
    pub issues: Vec<SceneIssue>,
}

impl SceneReport {
    /// `true` when no actionable problems were found.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }

    /// Total number of actionable problems found.
    #[must_use]
    pub fn problem_count(&self) -> usize {
        self.issues.len()
    }

    /// Pushes a finding at `location` with the given `detail`.
    fn push(&mut self, location: impl Into<String>, detail: impl Into<String>) {
        self.issues.push(SceneIssue {
            location: location.into(),
            detail: detail.into(),
        });
    }
}

/// Loads the scene `.def` at `def_path` (a storyboard or a screenpack, per
/// `kind`) and produces a [`SceneReport`].
///
/// # Errors
///
/// Returns [`fp_core::FpError`] only when the `.def` itself cannot be read.
pub fn validate_scene(def_path: &Path, kind: ContentKind) -> fp_core::FpResult<SceneReport> {
    if kind == ContentKind::Screenpack {
        let def = DefFile::load(def_path)?;
        let layout = ScreenpackLayout::parse(&def);
        Ok(analyze_screenpack(&layout))
    } else {
        let sb = Storyboard::load(def_path)?;
        Ok(analyze_storyboard(&sb))
    }
}

/// Builds a [`SceneReport`] from an already-parsed storyboard.
///
/// Lints scene assets statically: a missing sprite container, scenes that
/// reference a `bg.name` no `[<name>Def]` group defines, and layers whose `anim`
/// references an action the storyboard has no `[Begin Action N]` block for.
#[must_use]
pub fn analyze_storyboard(sb: &Storyboard) -> SceneReport {
    let mut report = SceneReport {
        is_screenpack: false,
        scene_count: sb.scenes.len(),
        sprite_path: if sb.sprite_path.is_empty() {
            None
        } else {
            Some(sb.sprite_path.clone())
        },
        ..Default::default()
    };

    if sb.sprite_path.is_empty() {
        report.push("SceneDef", "no `spr` sprite container specified");
    }
    if sb.scenes.is_empty() {
        report.push("SceneDef", "no [Scene N] sections found — nothing to play");
    }

    // The set of background-group names the storyboard defines.
    let group_names: BTreeSet<String> = sb
        .bg_groups
        .iter()
        .map(|g| g.name.to_ascii_lowercase())
        .collect();

    for (i, scene) in sb.scenes.iter().enumerate() {
        // A scene referencing a bg.name with no matching [<name>Def] group.
        if let Some(bg) = &scene.bg_name {
            if !bg.is_empty() && !group_names.contains(&bg.to_ascii_lowercase()) {
                report.push(
                    format!("Scene {i}"),
                    format!("bg.name = {bg:?} but no matching background group is defined"),
                );
            }
        }
        // A negative end.time is nonsensical.
        if scene.end_time < 0 {
            report.push(
                format!("Scene {i}"),
                format!("end.time ({}) is negative", scene.end_time),
            );
        }
        // Each layer's anim must resolve to an embedded action.
        for layer in &scene.layers {
            if let Some(anim) = layer.anim {
                if !sb.animations.contains_key(&anim) {
                    report.push(
                        format!("Scene {i} layer {}", layer.index),
                        format!("anim = {anim} but no [Begin Action {anim}] is defined"),
                    );
                }
            }
            // A layer that draws neither an anim nor a sprite shows nothing.
            if layer.anim.is_none() && layer.spriteno.is_none() {
                report.push(
                    format!("Scene {i} layer {}", layer.index),
                    "layer has neither `anim` nor `spriteno` — it draws nothing",
                );
            }
        }
    }

    report
}

/// Builds a [`SceneReport`] from an already-parsed screenpack (`fight.def`)
/// layout.
///
/// Lints the HUD layout statically: a missing `[Files] sff`, no fonts declared,
/// and text elements that reference a font slot beyond the declared font list.
#[must_use]
pub fn analyze_screenpack(layout: &ScreenpackLayout) -> SceneReport {
    let mut report = SceneReport {
        is_screenpack: true,
        scene_count: layout.fonts.len(),
        sprite_path: if layout.sff.is_empty() {
            None
        } else {
            Some(layout.sff.clone())
        },
        ..Default::default()
    };

    if layout.sff.is_empty() {
        report.push("Files", "no `sff` sprite container specified");
    }
    if layout.fonts.is_empty() {
        report.push(
            "Files",
            "no fonts declared (font0..fontN) — HUD text cannot render",
        );
    }

    // Every text element references a font slot; flag any that points past the
    // declared font list (an out-of-range slot renders nothing).
    let font_count = layout.fonts.len();
    let mut check_font = |location: &str, slot: usize| {
        if font_count == 0 {
            // Already reported as "no fonts declared"; don't double-report.
            return;
        }
        if slot >= font_count {
            report.push(
                location.to_string(),
                format!("font slot {slot} is out of range (only {font_count} font(s) declared)"),
            );
        }
    };
    check_font("Round", layout.round.font);
    check_font("Time", layout.time.font);
    check_font("Combo", layout.combo.font);
    check_font("Name p1", layout.p1_name.font);
    check_font("Name p2", layout.p2_name.font);

    report
}

/// Renders a [`SceneReport`] into the multi-line, human-readable text the
/// `validate` subcommand prints for a storyboard or screenpack `.def`.
#[must_use]
pub fn render_scene_report(report: &SceneReport) -> String {
    let mut out = String::new();

    let (kind, count_label) = if report.is_screenpack {
        ("Screenpack", "fonts")
    } else {
        ("Storyboard", "scenes")
    };

    out.push_str(&format!("{kind} validation report\n"));
    out.push_str(&format!(
        "  {count_label}: {}   sprite container: {}\n",
        report.scene_count,
        report.sprite_path.as_deref().unwrap_or("<none>")
    ));

    if report.is_clean() {
        out.push_str("\nResult: PASS — no scene authoring problems found.\n");
    } else {
        out.push_str(&format!(
            "\nResult: {} problem(s) found.\n",
            report.problem_count()
        ));
        for issue in &report.issues {
            out.push_str(&format!("  - [{}] {}\n", issue.location, issue.detail));
        }
    }

    out.push('\n');
    out.push_str(LICENSE_REMINDER);
    out.push('\n');

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use fp_character::loader::{CompiledExpr, CompiledParam, CompiledState, CompiledTriggerGroup};
    use fp_character::CharacterConstants;
    use fp_formats::air::{AirFile, AnimAction, AnimFrame};
    use fp_formats::sff::SffFile;
    use fp_core::SpriteId;

    // ---- builders for an in-memory broken/clean character ----------------

    /// Builds a real (parseable) SFF v2 with raw 1x1 sprites at the given
    /// `(group, image)` pairs, through the public `SffFile::from_bytes` path —
    /// the `ldata`/`tdata` fields are private, so an in-memory `SffFile` must be
    /// produced from genuine bytes rather than a struct literal.
    fn make_sff(coords: &[(u16, u16)]) -> SffFile {
        SffFile::from_bytes(&synthetic_sff_bytes(coords)).expect("synthetic SFF parses")
    }

    /// The raw bytes of the synthetic SFF v2 [`make_sff`] builds — split out so a
    /// test that needs a real `.sff` file on disk (the stage `[BGdef] spr`
    /// existence/parse check) can write them without depending on any
    /// copyrighted asset.
    fn synthetic_sff_bytes(coords: &[(u16, u16)]) -> Vec<u8> {
        let n = coords.len();
        let sprite_off = 512usize;
        let palette_off = sprite_off + 28 * n;
        let ldata_off = palette_off + 16;
        // LData: 768-byte palette, then one 1-byte pixel per sprite.
        let ldata_len = 768 + n;
        let total = ldata_off + ldata_len;
        let mut buf = vec![0u8; total];

        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[15] = 2; // major = v2
        buf[36..40].copy_from_slice(&(sprite_off as u32).to_le_bytes());
        buf[40..44].copy_from_slice(&(n as u32).to_le_bytes());
        buf[44..48].copy_from_slice(&(palette_off as u32).to_le_bytes());
        buf[48..52].copy_from_slice(&1u32.to_le_bytes());
        buf[52..56].copy_from_slice(&(ldata_off as u32).to_le_bytes());
        buf[56..60].copy_from_slice(&(ldata_len as u32).to_le_bytes());
        buf[60..64].copy_from_slice(&(total as u32).to_le_bytes()); // tdata off (empty)
        buf[64..68].copy_from_slice(&0u32.to_le_bytes()); // tdata len

        for (i, (g, im)) in coords.iter().enumerate() {
            let o = sprite_off + i * 28;
            buf[o..o + 2].copy_from_slice(&g.to_le_bytes());
            buf[o + 2..o + 4].copy_from_slice(&im.to_le_bytes());
            buf[o + 4..o + 6].copy_from_slice(&1u16.to_le_bytes()); // w
            buf[o + 6..o + 8].copy_from_slice(&1u16.to_le_bytes()); // h
            buf[o + 12..o + 14].copy_from_slice(&(i as u16).to_le_bytes()); // linked=self
            buf[o + 14] = 0; // raw
            buf[o + 15] = 8; // depth
            let px_off = 768 + i; // within LData
            buf[o + 16..o + 20].copy_from_slice(&(px_off as u32).to_le_bytes());
            buf[o + 20..o + 24].copy_from_slice(&1u32.to_le_bytes());
        }

        // Palette sub-header (768 bytes at LData offset 0).
        let p = palette_off;
        buf[p + 4..p + 6].copy_from_slice(&256u16.to_le_bytes());
        buf[p + 12..p + 16].copy_from_slice(&768u32.to_le_bytes());

        buf
    }

    fn frame(group: u16, image: u16) -> AnimFrame {
        AnimFrame {
            sprite: SpriteId::new(group, image),
            ticks: 5,
            ..Default::default()
        }
    }

    fn action(number: i32, frames: Vec<AnimFrame>) -> AnimAction {
        AnimAction {
            action_number: number,
            frames,
            loopstart: 0,
        }
    }

    fn make_air(actions: Vec<AnimAction>) -> AirFile {
        let mut map = HashMap::new();
        for a in actions {
            map.insert(a.action_number, a);
        }
        AirFile { actions: map }
    }

    /// A `ChangeState value = N` controller (N a literal int).
    fn change_state(state: i32, label: &str, target: i32) -> CompiledController {
        let mut params = HashMap::new();
        params.insert("value".to_string(), CompiledParam::compile(&target.to_string()));
        CompiledController {
            state_number: state,
            label: label.to_string(),
            controller_type: Some("ChangeState".to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params,
        }
    }

    fn change_anim(state: i32, label: &str, target: i32) -> CompiledController {
        let mut params = HashMap::new();
        params.insert("value".to_string(), CompiledParam::compile(&target.to_string()));
        CompiledController {
            state_number: state,
            label: label.to_string(),
            controller_type: Some("ChangeAnim".to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params,
        }
    }

    /// A controller of an arbitrary (possibly unsupported) type with one
    /// trigger condition compiled from `trigger_src` (use a bad source to force
    /// a fallback).
    fn typed_ctrl(state: i32, kind: &str, trigger_src: &str) -> CompiledController {
        CompiledController {
            state_number: state,
            label: kind.to_string(),
            controller_type: Some(kind.to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile(trigger_src)],
            }],
            persistent: None,
            ignorehitpause: None,
            params: HashMap::new(),
        }
    }

    fn state(number: i32, controllers: Vec<CompiledController>) -> CompiledState {
        CompiledState {
            number,
            controllers,
            ..Default::default()
        }
    }

    fn loaded(
        sff: SffFile,
        air: AirFile,
        states: Vec<CompiledState>,
        has_snd: bool,
    ) -> LoadedCharacter {
        let mut map = HashMap::new();
        for s in states {
            map.insert(s.number, s);
        }
        LoadedCharacter {
            name: "Test Dummy".to_string(),
            localcoord: (320, 240),
            constants: CharacterConstants::default(),
            states: map,
            sff,
            air,
            cmd: None,
            // A `.snd` is optional; we only flip the flag, never build a SndFile.
            snd: if has_snd { build_min_snd() } else { None },
            palettes: Vec::new(),
        }
    }

    /// Builds a tiny valid in-memory SndFile (one empty sound) for the
    /// `has_sound = yes` path. Uses the public byte parser.
    fn build_min_snd() -> Option<fp_formats::snd::SndFile> {
        // ElecbyteSnd\0 + version(4) + count(0) + first_offset(24)
        let mut buf = Vec::new();
        buf.extend_from_slice(b"ElecbyteSnd\0");
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&24u32.to_le_bytes());
        fp_formats::snd::SndFile::from_bytes(&buf).ok()
    }

    #[test]
    fn clean_character_has_no_problems() {
        let sff = make_sff(&[(0, 0), (20, 0)]);
        let air = make_air(vec![
            action(0, vec![frame(0, 0)]),
            action(20, vec![frame(20, 0)]),
        ]);
        let states = vec![
            state(0, vec![change_anim(0, "anim", 0), change_state(0, "go", 20)]),
            state(20, vec![change_anim(20, "anim", 20), change_state(20, "back", 0)]),
        ];
        let c = loaded(sff, air, states, true);
        let report = analyze(&c);

        assert!(report.is_clean(), "report: {report:?}");
        assert_eq!(report.problem_count(), 0);
        assert_eq!(report.state_count, 2);
        assert_eq!(report.sprite_count, 2);
        assert_eq!(report.anim_count, 2);
        assert!(report.has_sound);
    }

    #[test]
    fn detects_missing_sprite() {
        // Action 0 frame 1 references (99,9), which is not in the SFF.
        let sff = make_sff(&[(0, 0)]);
        let air = make_air(vec![action(0, vec![frame(0, 0), frame(99, 9)])]);
        let states = vec![state(0, vec![])];
        let c = loaded(sff, air, states, false);
        let report = analyze(&c);

        assert_eq!(report.missing_sprites.len(), 1);
        let m = &report.missing_sprites[0];
        assert_eq!((m.action, m.frame, m.group, m.image), (0, 1, 99, 9));
        assert!(!report.is_clean());
    }

    #[test]
    fn detects_unresolved_state() {
        let sff = make_sff(&[(0, 0)]);
        let air = make_air(vec![action(0, vec![frame(0, 0)])]);
        // state 0 jumps to 999 which does not exist.
        let states = vec![state(0, vec![change_state(0, "broken", 999)])];
        let c = loaded(sff, air, states, false);
        let report = analyze(&c);

        assert_eq!(report.unresolved_states.len(), 1);
        let u = &report.unresolved_states[0];
        assert_eq!(u.from_state, 0);
        assert_eq!(u.target, 999);
        assert_eq!(u.controller, "ChangeState");
    }

    #[test]
    fn negative_state_target_resolves() {
        // ChangeState to -1 (a real, present state) must NOT be flagged.
        let sff = make_sff(&[(0, 0)]);
        let air = make_air(vec![action(0, vec![frame(0, 0)])]);
        let states = vec![
            state(-1, vec![]),
            state(0, vec![change_state(0, "to-1", -1)]),
        ];
        let c = loaded(sff, air, states, false);
        let report = analyze(&c);
        assert!(report.unresolved_states.is_empty(), "{report:?}");
    }

    #[test]
    fn detects_unresolved_anim() {
        let sff = make_sff(&[(0, 0)]);
        let air = make_air(vec![action(0, vec![frame(0, 0)])]);
        // ChangeAnim to 555 which has no AIR action.
        let states = vec![state(0, vec![change_anim(0, "bad-anim", 555)])];
        let c = loaded(sff, air, states, false);
        let report = analyze(&c);

        assert_eq!(report.unresolved_anims.len(), 1);
        assert_eq!(report.unresolved_anims[0].target, 555);
    }

    #[test]
    fn runtime_expression_target_not_flagged() {
        // A `value = stateno + 1` cannot be resolved statically -> not flagged.
        let sff = make_sff(&[(0, 0)]);
        let air = make_air(vec![action(0, vec![frame(0, 0)])]);
        let mut params = HashMap::new();
        params.insert("value".to_string(), CompiledParam::compile("stateno + 1"));
        let ctrl = CompiledController {
            state_number: 0,
            label: "expr".to_string(),
            controller_type: Some("ChangeState".to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params,
        };
        let states = vec![state(0, vec![ctrl])];
        let c = loaded(sff, air, states, false);
        let report = analyze(&c);
        assert!(report.unresolved_states.is_empty(), "{report:?}");
    }

    #[test]
    fn detects_failed_expression() {
        let sff = make_sff(&[(0, 0)]);
        let air = make_air(vec![action(0, vec![frame(0, 0)])]);
        // A genuinely malformed trigger -> const-0 fallback.
        let bad = typed_ctrl(0, "VelSet", "1 +* 2");
        let states = vec![state(0, vec![bad])];
        let c = loaded(sff, air, states, false);
        let report = analyze(&c);

        assert_eq!(report.failed_exprs.len(), 1, "{report:?}");
        assert_eq!(report.failed_exprs[0].from_state, 0);
        assert_eq!(report.failed_exprs[0].source, "1 +* 2");
    }

    #[test]
    fn detects_unsupported_controller() {
        let sff = make_sff(&[(0, 0)]);
        let air = make_air(vec![action(0, vec![frame(0, 0)])]);
        // `Explod` is not in the supported set (valid trigger, so no fallback).
        let unsup = typed_ctrl(0, "Explod", "1");
        let unsup2 = typed_ctrl(0, "Explod", "1");
        let states = vec![state(0, vec![unsup, unsup2])];
        let c = loaded(sff, air, states, false);
        let report = analyze(&c);

        assert_eq!(report.unsupported_controllers.get("Explod"), Some(&2));
        // Unsupported controllers are advisory only -> still "clean".
        assert!(report.is_clean());
        assert!(report.failed_exprs.is_empty());
    }

    #[test]
    fn supported_controller_not_flagged() {
        let sff = make_sff(&[(0, 0)]);
        let air = make_air(vec![action(0, vec![frame(0, 0)])]);
        // HitDef IS supported (case-insensitively).
        let ok = typed_ctrl(0, "hitdef", "1");
        let states = vec![state(0, vec![ok])];
        let c = loaded(sff, air, states, false);
        let report = analyze(&c);
        assert!(report.unsupported_controllers.is_empty(), "{report:?}");
    }

    #[test]
    fn render_includes_license_and_sections() {
        let sff = make_sff(&[(0, 0)]);
        let air = make_air(vec![action(0, vec![frame(0, 0), frame(99, 9)])]);
        let states = vec![state(0, vec![change_state(0, "broken", 999)])];
        let c = loaded(sff, air, states, false);
        let report = analyze(&c);
        let text = render_report(&report);

        assert!(text.contains("Character validation report: Test Dummy"));
        assert!(text.contains("Missing sprites"));
        assert!(text.contains("Unresolved state references"));
        assert!(text.contains("problem(s) found"));
        assert!(text.contains("clean-room"));
        assert!(text.contains("Elecbyte"));
    }

    #[test]
    fn render_clean_says_pass() {
        let sff = make_sff(&[(0, 0)]);
        let air = make_air(vec![action(0, vec![frame(0, 0)])]);
        let states = vec![state(0, vec![change_state(0, "self", 0)])];
        let c = loaded(sff, air, states, false);
        let report = analyze(&c);
        let text = render_report(&report);
        assert!(text.contains("PASS"), "{text}");
    }

    /// Resolves a path inside the workspace `assets/trainingdummy/` directory.
    /// Unit tests run with the crate dir (`crates/fp-app`) as the manifest root.
    fn dummy_asset(rel: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/trainingdummy")
            .join(rel)
    }

    #[test]
    fn shipped_training_dummy_validates_clean() {
        // The Training Dummy is committed under assets/, so this is NOT
        // asset-gated: the validator must load it end-to-end through the real
        // loader and report it clean (no missing sprites / dead refs / failed
        // compiles). This is the conformance guard for the shipped fixture.
        let def = dummy_asset("trainingdummy.def");
        let report = validate(&def)
            .unwrap_or_else(|e| panic!("shipped Training Dummy failed to load: {e}"));
        assert!(
            report.is_clean(),
            "Training Dummy not clean: {} problem(s):\n{}",
            report.problem_count(),
            render_report(&report)
        );
        // It also exercises real breadth (idle/walk/crouch/jump/attack/get-hit).
        assert!(report.state_count >= 8);
        assert!(report.sprite_count >= 6);
        assert!(report.has_sound);
    }

    #[test]
    fn missing_def_is_a_load_error_not_a_panic() {
        // The loader returns Err for a non-existent .def; `validate` propagates
        // it (the CLI maps that to exit 1) rather than panicking.
        let result = validate(Path::new("definitely/does/not/exist.def"));
        assert!(result.is_err());
    }

    // ===================================================================
    // Content-kind detection
    // ===================================================================

    fn def(text: &str) -> DefFile {
        DefFile::from_str(text).expect("synthetic .def parses")
    }

    #[test]
    fn detect_kind_classifies_character() {
        let d = def("[Info]\nname=X\n[Files]\nsprite=x.sff\nanim=x.air\n");
        assert_eq!(detect_kind(&d), Some(ContentKind::Character));
        // Even with only one of sprite/anim it is still a character.
        let only_anim = def("[Files]\nanim=x.air\n");
        assert_eq!(detect_kind(&only_anim), Some(ContentKind::Character));
    }

    #[test]
    fn detect_kind_classifies_stage() {
        let d = def("[Info]\nname=Dojo\n[BGdef]\nspr=dojo.sff\n[BG Sky]\ntype=normal\n");
        assert_eq!(detect_kind(&d), Some(ContentKind::Stage));
        // A [Files] section *without* sprite/anim must not mask the stage.
        let with_empty_files = def("[Files]\n[Camera]\nboundleft=-200\n");
        assert_eq!(detect_kind(&with_empty_files), Some(ContentKind::Stage));
    }

    #[test]
    fn detect_kind_classifies_storyboard_and_screenpack() {
        let sb = def("[SceneDef]\nspr=intro.sff\n[Scene 0]\nend.time=100\n");
        assert_eq!(detect_kind(&sb), Some(ContentKind::Storyboard));

        let sb_scene_only = def("[Scene 0]\nend.time=50\n");
        assert_eq!(detect_kind(&sb_scene_only), Some(ContentKind::Storyboard));

        let sp = def("[Files]\nsff=fight.sff\nfont0=f.fnt\n[Lifebar]\np1.pos=0,0\n");
        // A screenpack has a [Files] sff but NO sprite/anim → not a character.
        assert_eq!(detect_kind(&sp), Some(ContentKind::Screenpack));
    }

    #[test]
    fn detect_kind_none_for_unrecognizable() {
        let d = def("[Whatever]\nkey=value\n");
        assert_eq!(detect_kind(&d), None);
        assert_eq!(detect_kind(&def("")), None);
    }

    #[test]
    fn content_kind_label_is_human_readable() {
        assert_eq!(ContentKind::Character.label(), "character");
        assert_eq!(ContentKind::Stage.label(), "stage");
        assert!(ContentKind::Storyboard.label().contains("scene"));
        assert!(ContentKind::Screenpack.label().contains("fight.def"));
    }

    // ===================================================================
    // Stage validation
    // ===================================================================

    #[test]
    fn clean_stage_passes() {
        // A well-formed stage (no [BGdef] spr to a real file, so use an in-memory
        // parse with a bare relative path that the analyzer leaves unchecked) and
        // with valid camera/player/stageinfo bounds.
        let text = "\
[Info]
name = Test Stage
[Camera]
boundleft = -300
boundright = 300
verticalfollow = 0.2
[PlayerInfo]
leftbound = -320
rightbound = 320
[StageInfo]
localcoord = 320, 240
xscale = 1
yscale = 1
[BGdef]
spr = stage.sff
[BG Sky]
type = normal
spriteno = 0, 0
";
        let stage = Stage::parse(text, None);
        let report = analyze_stage(&stage);
        assert!(report.is_clean(), "stage not clean: {report:?}");
        assert_eq!(report.bg_count, 1);
        assert_eq!(report.sprite_path.as_deref(), Some("stage.sff"));
        // A bare relative path is not checked, so it is not marked loaded.
        assert!(!report.sprite_loaded);
    }

    #[test]
    fn stage_flags_inverted_camera_and_player_bounds() {
        let text = "\
[Camera]
boundleft = 300
boundright = -300
verticalfollow = 5
[PlayerInfo]
leftbound = 100
rightbound = -100
[StageInfo]
localcoord = 320, 240
[BGdef]
spr = s.sff
";
        let stage = Stage::parse(text, None);
        let report = analyze_stage(&stage);
        assert!(!report.is_clean());
        let text = render_stage_report(&report);
        assert!(text.contains("inverted"), "{text}");
        assert!(text.contains("verticalfollow"), "{text}");
        assert!(text.contains("leftbound"), "{text}");
    }

    #[test]
    fn stage_flags_missing_sprite_container() {
        let text = "\
[StageInfo]
localcoord = 320, 240
[BG Sky]
type = normal
";
        let stage = Stage::parse(text, None);
        let report = analyze_stage(&stage);
        // [BG] declared but no [BGdef] spr.
        assert!(report
            .issues
            .iter()
            .any(|i| i.section == "BGdef" && i.detail.contains("no `spr`")));
    }

    #[test]
    fn stage_flags_anim_without_actionno_and_bad_geometry() {
        let text = "\
[StageInfo]
localcoord = 0, 240
xscale = 0
yscale = 1
[BGdef]
spr = s.sff
[BG Clouds]
type = anim
spriteno = -1, 0
";
        let stage = Stage::parse(text, None);
        let report = analyze_stage(&stage);
        let rendered = render_stage_report(&report);
        assert!(rendered.contains("localcoord"), "{rendered}");
        assert!(rendered.contains("xscale/yscale"), "{rendered}");
        assert!(rendered.contains("cannot animate"), "{rendered}");
        assert!(rendered.contains("negative"), "{rendered}");
    }

    #[test]
    fn stage_render_clean_says_pass_and_has_license() {
        let stage = Stage::parse("[StageInfo]\nlocalcoord = 320, 240\n", None);
        let report = analyze_stage(&stage);
        // No backgrounds + no spr → "no spr" issue, so add one to make it clean:
        // an empty stage with no BGs reports the no-spr issue.
        assert!(!report.is_clean());
        // A truly minimal but valid stage: give it a spr (bare path, unchecked).
        let stage2 = Stage::parse("[StageInfo]\nlocalcoord=320,240\n[BGdef]\nspr=s.sff\n", None);
        let report2 = analyze_stage(&stage2);
        let text = render_stage_report(&report2);
        assert!(text.contains("PASS"), "{text}");
        assert!(text.contains("clean-room"));
        assert!(text.contains("Elecbyte"));
    }

    // ===================================================================
    // Storyboard (scene) validation
    // ===================================================================

    #[test]
    fn clean_storyboard_passes() {
        let text = "\
[SceneDef]
spr = intro.sff
startscene = 0
[Scene 0]
end.time = 100
layer0.spriteno = 0, 0
";
        let sb = Storyboard::from_def(text);
        let report = analyze_storyboard(&sb);
        assert!(report.is_clean(), "storyboard not clean: {report:?}");
        assert!(!report.is_screenpack);
        assert_eq!(report.scene_count, 1);
        assert_eq!(report.sprite_path.as_deref(), Some("intro.sff"));
    }

    #[test]
    fn storyboard_flags_missing_spr_and_no_scenes() {
        let sb = Storyboard::from_def("[SceneDef]\nstartscene = 0\n");
        let report = analyze_storyboard(&sb);
        assert!(!report.is_clean());
        let rendered = render_scene_report(&report);
        assert!(rendered.contains("Storyboard"), "{rendered}");
        assert!(rendered.contains("no `spr`"), "{rendered}");
        assert!(rendered.contains("nothing to play"), "{rendered}");
    }

    #[test]
    fn storyboard_flags_layer_with_no_drawable() {
        // A layer that sets neither anim nor spriteno (only an offset) draws
        // nothing.
        let text = "\
[SceneDef]
spr = intro.sff
[Scene 0]
end.time = 50
layer0.offset = 10, 10
";
        let sb = Storyboard::from_def(text);
        let report = analyze_storyboard(&sb);
        assert!(report
            .issues
            .iter()
            .any(|i| i.detail.contains("draws nothing")), "{report:?}");
    }

    #[test]
    fn storyboard_flags_unknown_bg_group() {
        let text = "\
[SceneDef]
spr = intro.sff
[Scene 0]
end.time = 50
bg.name = Nonexistent
layer0.spriteno = 0, 0
";
        let sb = Storyboard::from_def(text);
        let report = analyze_storyboard(&sb);
        assert!(report
            .issues
            .iter()
            .any(|i| i.detail.contains("no matching background group")), "{report:?}");
    }

    // ===================================================================
    // Screenpack (scene) validation
    // ===================================================================

    #[test]
    fn clean_screenpack_passes() {
        let text = "\
[Files]
sff = fight.sff
font0 = font.fnt
[Lifebar]
p1.pos = 0, 0
[Round]
pos = 160, 20
font = 0
[Time]
pos = 160, 20
font = 0
";
        let d = def(text);
        let layout = ScreenpackLayout::parse(&d);
        let report = analyze_screenpack(&layout);
        assert!(report.is_clean(), "screenpack not clean: {report:?}");
        assert!(report.is_screenpack);
        assert_eq!(report.sprite_path.as_deref(), Some("fight.sff"));
    }

    #[test]
    fn screenpack_flags_missing_sff_and_fonts() {
        let d = def("[Lifebar]\np1.pos = 0, 0\n");
        let layout = ScreenpackLayout::parse(&d);
        let report = analyze_screenpack(&layout);
        assert!(!report.is_clean());
        let rendered = render_scene_report(&report);
        assert!(rendered.contains("Screenpack"), "{rendered}");
        assert!(rendered.contains("no `sff`"), "{rendered}");
        assert!(rendered.contains("no fonts"), "{rendered}");
    }

    #[test]
    fn screenpack_flags_out_of_range_font_slot() {
        // Declare one font (slot 0) but point Round at slot 3.
        let text = "\
[Files]
sff = fight.sff
font0 = font.fnt
[Round]
pos = 0, 0
font = 3
";
        let d = def(text);
        let layout = ScreenpackLayout::parse(&d);
        let report = analyze_screenpack(&layout);
        assert!(report
            .issues
            .iter()
            .any(|i| i.location == "Round" && i.detail.contains("out of range")), "{report:?}");
    }

    // ===================================================================
    // Top-level dispatch (validate_path / AnyReport)
    // ===================================================================

    /// Writes `text` to a uniquely-named temp `.def` and returns its path.
    fn write_temp_def(name: &str, text: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "fp_t025_{}_{}_{name}.def",
            std::process::id(),
            // a coarse nonce so concurrent test threads don't collide
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, text).expect("write temp .def");
        path
    }

    #[test]
    fn validate_path_routes_stage_to_stage_report() {
        let path = write_temp_def(
            "stage",
            "[Info]\nname=Dojo\n[StageInfo]\nlocalcoord=320,240\n[BGdef]\nspr=s.sff\n[BG Sky]\ntype=normal\n",
        );
        let report = validate_path(&path).expect("stage validates");
        // It routes to the stage analyzer...
        let AnyReport::Stage(stage_report) = &report else {
            panic!("expected a Stage report, got {report:?}");
        };
        // ...and `Stage::load` resolves `spr` to an absolute path next to the
        // .def, so the (non-existent) sprite container is correctly flagged —
        // exactly the asset-reference lint the validator is for.
        assert!(!report.is_clean());
        assert!(
            stage_report
                .issues
                .iter()
                .any(|i| i.section == "BGdef" && i.detail.contains("file not found")),
            "{}",
            render_any(&report)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_path_stage_passes_when_spr_file_exists_and_parses() {
        // Write a real (synthetic) SFF next to a stage .def so the [BGdef] spr
        // existence + parse check succeeds and the stage validates clean.
        let dir = std::env::temp_dir().join(format!(
            "fp_t025_stageok_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        // A minimal valid SFF (reuse the synthetic v2 builder, serialized back to
        // bytes is not exposed, so author the bytes the same way make_sff does).
        let sff_bytes = synthetic_sff_bytes(&[(0, 0)]);
        std::fs::write(dir.join("s.sff"), &sff_bytes).expect("write sff");
        let def_path = dir.join("stage.def");
        std::fs::write(
            &def_path,
            "[StageInfo]\nlocalcoord=320,240\n[BGdef]\nspr=s.sff\n[BG Sky]\ntype=normal\nspriteno=0,0\n",
        )
        .expect("write def");

        let report = validate_path(&def_path).expect("stage validates");
        match &report {
            AnyReport::Stage(r) => {
                assert!(r.is_clean(), "{}", render_any(&report));
                assert!(r.sprite_loaded, "spr should have loaded: {r:?}");
            }
            other => panic!("expected a Stage report, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_path_routes_storyboard_to_scene_report() {
        let path = write_temp_def(
            "story",
            "[SceneDef]\nspr=intro.sff\n[Scene 0]\nend.time=100\nlayer0.spriteno=0,0\n",
        );
        let report = validate_path(&path).expect("storyboard validates");
        assert!(matches!(report, AnyReport::Scene(_)));
        assert!(report.is_clean(), "{}", render_any(&report));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_path_routes_screenpack_to_scene_report() {
        let path = write_temp_def(
            "fight",
            "[Files]\nsff=fight.sff\nfont0=f.fnt\n[Lifebar]\np1.pos=0,0\n[Round]\npos=0,0\nfont=0\n",
        );
        let report = validate_path(&path).expect("screenpack validates");
        match &report {
            AnyReport::Scene(r) => assert!(r.is_screenpack),
            other => panic!("expected a screenpack Scene report, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_path_errors_on_unclassifiable_def() {
        let path = write_temp_def("junk", "[Whatever]\nkey=value\n");
        let result = validate_path(&path);
        assert!(result.is_err(), "an unclassifiable .def must error, not guess");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_path_routes_shipped_training_dummy_to_character() {
        // The shipped (committed, non-asset-gated) Training Dummy must still
        // classify as a character and validate clean through the dispatcher —
        // the regression guard that character validation kept working.
        let def_path = dummy_asset("trainingdummy.def");
        let report = validate_path(&def_path)
            .unwrap_or_else(|e| panic!("Training Dummy failed via validate_path: {e}"));
        match &report {
            AnyReport::Character(_) => {}
            other => panic!("expected a Character report, got {other:?}"),
        }
        assert!(report.is_clean(), "{}", render_any(&report));
    }
}
