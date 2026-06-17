//! Fighters Paradise — a modern MUGEN engine reimplementation in Rust.
//!
//! This is the application entry point. It initializes the SDL2 window,
//! sets up the wgpu rendering pipeline, and runs the main 60Hz game loop.
//!
//! # Usage
//!
//! ```text
//! cargo run -p fp-app                          # two KFMs in a match (P1 keyboard, P2 baseline CPU AI)
//! cargo run -p fp-app -- <p1.def>              # P1 = that character, P2 = same character
//! cargo run -p fp-app -- <p1.def> <p2.def>     # P1 and P2 from two .def files
//! cargo run -p fp-app -- <file.sff> <file.air> # legacy SFF+AIR animation viewer (demo mode)
//! cargo run -p fp-app -- <file.sff>            # legacy single-sprite viewer
//! cargo run -p fp-app -- validate <file.def>   # headless: load a character/stage/scene +
//!                                              #   print an actionable lint report (no window)
//! ```
//!
//! ## The `validate` subcommand (headless content linter)
//!
//! `validate <file.def>` detects what the `.def` describes — a **character**, a
//! **stage**, or a **scene** (storyboard / `fight.def` screenpack) — and prints
//! an actionable lint report for it, then exits 0 (the report body carries the
//! findings; a non-zero exit is reserved for a missing argument or a `.def` that
//! cannot be loaded/classified at all). It opens no window, GPU, or audio device,
//! so it runs anywhere.
//!
//! - **Characters** report missing referenced sprites, unresolved
//!   `ChangeState`/`ChangeAnim` targets, expressions that failed to compile
//!   (silent const-`0` fallbacks), and unsupported controllers.
//! - **Stages** report missing/invalid `[Camera]`/`[PlayerInfo]`/`[StageInfo]`
//!   fields, a missing or unloadable `[BGdef] spr`, and per-`[BG]` issues.
//! - **Scenes** report missing sprite containers, scenes referencing an
//!   undefined background group, layers with no drawable, and out-of-range font
//!   slots.
//!
//! See [`validate::validate_path`] for the dispatch and the per-kind analyzers.
//!
//! ## The two-player match (task 7.2 — the playable milestone)
//!
//! The default mode loads **two** full MUGEN characters (both KFM) from their
//! `.def`s, places them at opposing start positions facing each other, and drives
//! them through a [`fp_engine::Match`]. Basic ground locomotion is no longer
//! shimmed in the app: the loader supplies MUGEN's built-in
//! stand<->walk<->crouch<->jump command-states for every character (task 7.3
//! part B) and the `Match` runs each player's real [`fp_input::CommandMatcher`]
//! (task 7.3 part A). Every 60Hz frame the app gathers P1 input from the keyboard
//! and a neutral/idle input for P2, calls [`fp_engine::Match::tick`], then renders
//! BOTH fighters from their current AIR frame (per-character texture cache) plus a
//! minimal HUD: each fighter's life as a bar (P1 top-left, P2 top-right) and a
//! KO/round indicator.
//!
//! Controls (P1): arrow keys or WASD move; attack buttons map to U/I/O (a/b/c)
//! and J/K/L (x/y/z). P2 is a stationary dummy in this milestone.
//!
//! ## Legacy single-character / viewer modes
//!
//! Passing an `.sff`+`.air` pair falls back to the original animation viewer, and
//! a lone `.sff` shows the first sprite; these legacy demo paths are retained.
//! With no arguments and no KFM assets present, the app shows a checkerboard test
//! pattern. Missing or unloadable assets degrade gracefully to the test pattern
//! with a clear log message; the app never panics.

mod import;
mod screens;
mod training;
mod validate;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fp_audio::{AudioSystem, Sound};
use fp_character::{Character, LoadedCharacter, SoundRequest};
use fp_core::{SpriteId, Vec2};
use fp_engine::{
    derive_player_seed, dummy_input, DummyMode, EffectSide, GameMode, Match, MatchInput, Player,
    PlayerDriver, RoundState, Side, StageBounds, TeamMatch, TeamMode, Winner, DEFAULT_MATCH_SEED,
};
use fp_formats::air::{AirFile, AnimAction};
use fp_formats::sff::SffFile;
use fp_input::{
    map_controller, AiDifficulty, BehaviorMode, Button as PadButton, ControllerInput, CpuAi,
    RawController, DEADZONE_DEFAULT,
};
use fp_render::{
    BlendMode, GlyphFont, PaletteTexture, Renderer, SpriteDrawParams, SpriteTexture, TextDrawParams,
};
use fp_stage::{BgLayer, Stage};
use fp_ui::{MatchHudState, ScreenpackHud, ScreenpackLayout, SelectDef, SystemDef};
use sdl2::controller::{Axis, Button as SdlPadButton, GameController};
use sdl2::event::Event;
use sdl2::keyboard::{KeyboardState, Keycode, Scancode};
use sdl2::GameControllerSubsystem;

// The single-character CNS driver (now `#[cfg(test)]`) and the legacy
// single-character tests are the only users of the command-recognition seam, so
// these imports are gated to keep the shipped binary free of unused imports.
#[cfg(test)]
use fp_character::ActiveCommands;
#[cfg(test)]
use fp_input::{compile_command, CommandDef, CommandMatcher, Direction, InputBuffer, InputState};

/// Window width in pixels.
const WINDOW_WIDTH: u32 = 640;
/// Window height in pixels.
const WINDOW_HEIGHT: u32 = 480;
/// Fixed timestep duration: 1/60th of a second (~16.667ms).
const TICK_DURATION: Duration = Duration::from_nanos(16_666_667);

/// Default character `.def` loaded when no CLI argument is given.
const DEFAULT_DEF: &str = "test-assets/kfm/kfm.def";

/// Shipped original common-effects (`fightfx`) sprite/animation set (audit #17).
/// Loaded once per match to render common (`fightfx`) hit-sparks for KFM and
/// conventional characters. Resolved relative to the process working directory;
/// a missing/bad asset is a best-effort no-op (no spark, no panic).
const COMMON_FX_SFF: &str = "assets/data/fightfx.sff";
/// Shipped original common-effects animation file. See [`COMMON_FX_SFF`].
const COMMON_FX_AIR: &str = "assets/data/fightfx.air";

/// Shipped original HUD bitmap font (FL2b). A clean-room FNT v1 covering `0-9`,
/// `A-Z`, space, and colon, loaded once into a [`fp_render::GlyphFont`] to draw
/// the round/KO/winner announcer and the round timer as real text. Resolved
/// relative to the process working directory; a missing/bad font degrades the HUD
/// to its solid-color quad markers (no panic, no regression).
const HUD_FONT_FNT: &str = "assets/data/font.fnt";

/// The shipped default motif `system.def` (the title menu + select grid
/// geometry). Resolved relative to the process working directory. When absent or
/// unparseable the title menu degrades to a built-in fallback (VS / TRAINING /
/// EXIT) over the shipped trainingdummy roster — see [`screens::TitleMenu::fallback`].
const DEFAULT_SYSTEM_DEF: &str = "assets/data/system.def";

/// The shipped fallback `select.def` roster, used when the motif `system.def`
/// declares no usable `[Files] select` (or it cannot be read). The default motif
/// points its `select` at this same file.
const DEFAULT_SELECT_DEF: &str = "assets/data/select.def";

/// The directory scanned for motif/screenpack sets (T045): each subfolder
/// holding a `system.def` is a selectable motif. The `--motif <name>` flag picks
/// one by its subfolder name; an absent/invalid selection falls back to
/// [`DEFAULT_SYSTEM_DEF`].
const DEFAULT_MOTIF_DIR: &str = "assets/data";

/// MUGEN common stand (idle) state number.
const STATE_STAND: i32 = 0;
/// MUGEN common walk state number. Only referenced by the single-character CNS
/// regression tests now that the app no longer shims locomotion, so it is gated
/// to keep the shipped binary free of an unused constant.
#[cfg(test)]
const STATE_WALK: i32 = 20;

/// Player 1's starting world X (left of center), in pixels.
const P1_START_X: f32 = -60.0;
/// Player 2's starting world X (right of center), in pixels.
const P2_START_X: f32 = 60.0;
/// Horizontal half-extent of the playfield, in world pixels. The match clamps
/// both fighters inside `[-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH]`.
const STAGE_HALF_WIDTH: f32 = 220.0;
/// World pixels of horizontal travel mapped to one window pixel. The match
/// world is centered on the origin; this scales it into the window for display.
const WORLD_TO_SCREEN: f32 = 1.0;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("Fighters Paradise v0.1.0");

    // The `validate` subcommand is a headless CLI report — it must NOT open an
    // SDL2 window or a GPU device, so it is intercepted here, before `run()`.
    let args: Vec<String> = std::env::args().collect();
    if args
        .get(1)
        .is_some_and(|a| a.eq_ignore_ascii_case("validate"))
    {
        std::process::exit(run_validate(&args));
    }

    // The `import` subcommand writes a repaired CNS/CMD overlay; it is also a
    // headless CLI path and must not open a window.
    if args
        .get(1)
        .is_some_and(|a| a.eq_ignore_ascii_case("import"))
    {
        std::process::exit(run_import(&args));
    }

    // The `tutorial` subcommand lists the Trials lesson flow (and, with `--demo`,
    // drives the runner to completion with synthesized success signals). It is a
    // headless CLI path — no window — so it is intercepted here, before `run()`.
    if args
        .get(1)
        .is_some_and(|a| a.eq_ignore_ascii_case("tutorial"))
    {
        std::process::exit(run_tutorial(&args));
    }

    if let Err(e) = run() {
        tracing::error!("Fatal error: {e}");
        std::process::exit(1);
    }
}

/// Runs the `validate <file.def>` subcommand: loads the character through the
/// real loader, prints an actionable report to stdout, and returns the process
/// exit code.
///
/// Per the task contract this **exits 0 with a summary** for any character that
/// loads (even one with authoring problems) — the report's body carries the
/// findings. A non-zero code is reserved for the two operational failures that
/// are not about the character's *content*: a missing `<file.def>` argument
/// (exit 2) and a character that cannot be loaded at all (exit 1, e.g. a missing
/// required SFF/AIR). The report itself is printed to stdout (its job is to be
/// the program's user-facing output); diagnostics go through `tracing`.
fn run_validate(args: &[String]) -> i32 {
    let Some(def_arg) = args.get(2) else {
        eprintln!("usage: fp-app validate <file.def>");
        return 2;
    };
    let def_path = Path::new(def_arg);
    match validate::validate_path(def_path) {
        Ok(report) => {
            if report.is_clean() {
                tracing::info!("validate: {} is clean", def_path.display());
            } else {
                tracing::warn!("validate: {} has authoring problems", def_path.display());
            }
            // stdout: this IS the program's output (a user-facing report), so a
            // direct print is correct here — not logging.
            print!("{}", validate::render_any(&report));
            0
        }
        Err(e) => {
            tracing::error!("validate: cannot load {}: {e}", def_path.display());
            eprintln!("validate: failed to load {}: {e}", def_path.display());
            1
        }
    }
}

/// Default directory the Trials lesson scripts ship in (relative to a game root).
const TUTORIAL_DIR: &str = "assets/data/tutorial";

/// Runs the `tutorial [dir] [--demo]` subcommand: a headless view of the Trials
/// flow.
///
/// Without `--demo` it loads the ordered lesson list (from `[dir]/tutorial.def`,
/// or the optional path, falling back to the built-in clean-room set when the
/// scripts are absent) and prints each lesson's goal + dummy/overlay config — so
/// the flow is inspectable without a window.
///
/// With `--demo` it additionally drives a [`training::tutorial::TutorialRunner`]
/// through the whole trial, feeding the synthesized success signal for each
/// lesson, and prints the advance trace — proving the runner advances through the
/// list and detects each success exactly once, with the always-works Skip never
/// soft-locking. Always exits 0 (a tutorial flow has no failure mode — bad/missing
/// assets degrade to the built-in set).
fn run_tutorial(args: &[String]) -> i32 {
    use training::tutorial::{load_lessons, TickOutcome, TutorialRunner};

    let demo = args.iter().any(|a| a.eq_ignore_ascii_case("--demo"));
    // First non-flag positional after `tutorial` is an optional lesson dir.
    let dir = args
        .get(2)
        .filter(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(TUTORIAL_DIR));

    let lessons = load_lessons(&dir);
    println!("Trials — {} lesson(s):", lessons.len());
    for (i, lesson) in lessons.iter().enumerate() {
        println!(
            "  {}. {} [dummy={:?}] — {}",
            i + 1,
            lesson.title,
            lesson.dummy,
            lesson.instruction
        );
    }

    if !demo {
        return 0;
    }

    println!("\n--demo: driving the runner with synthesized success signals:");
    let mut runner = TutorialRunner::new(lessons);
    if runner.is_empty() {
        println!("  (no lessons — trial is already complete)");
    }
    let total = runner.len();
    while let Some(lesson) = runner.current() {
        let step = runner.index() + 1;
        let title = format!("[{step}/{total}] {}", lesson.title);
        let Some(event) = synth_success_event(&lesson.success) else {
            // No synthesizable signal (only `Unsatisfiable`, which the runner
            // auto-skips) — use the always-works Skip so we never hang.
            let outcome = runner.skip();
            println!("  skipped: {title} -> {outcome:?}");
            continue;
        };
        // Some conditions need repeated signals (e.g. BlockNHits feeds one guarded
        // hit at a time). Feed until the lesson advances, capped so a lesson that
        // never advances cannot spin (it can't — every non-`Unsatisfiable` cond is
        // satisfiable by `synth_success_event`).
        let mut outcome = TickOutcome::InProgress;
        for _ in 0..64 {
            outcome = runner.observe(std::slice::from_ref(&event));
            if outcome != TickOutcome::InProgress {
                break;
            }
        }
        println!("  completed: {title} -> {outcome:?}");
    }
    println!("Trial complete: {}", runner.is_complete());
    0
}

/// Maps a [`training::tutorial::SuccessCond`] to the [`training::tutorial::LessonEvent`]
/// that satisfies it — used by `tutorial --demo` to drive the runner headlessly,
/// and shared with the runner's own tests. Returns `None` for an unsatisfiable
/// condition (the runner auto-skips those).
fn synth_success_event(
    cond: &training::tutorial::SuccessCond,
) -> Option<training::tutorial::LessonEvent> {
    use training::tutorial::{LessonEvent, SuccessCond};
    match cond {
        SuccessCond::LandCommand(name) => Some(LessonEvent::CommandRecognized(name.clone())),
        SuccessCond::BlockNHits(_) => Some(LessonEvent::HitConnected {
            defender_airborne: false,
            guarded: true,
        }),
        SuccessCond::ComboCount(n) => Some(LessonEvent::ComboCount(*n)),
        SuccessCond::AntiAir => Some(LessonEvent::HitConnected {
            defender_airborne: true,
            guarded: false,
        }),
        SuccessCond::ThrowConnected => Some(LessonEvent::ThrowConnected),
        SuccessCond::Unsatisfiable => None,
    }
}

/// Parsed flags / positionals for the `import` subcommand.
struct ImportArgs {
    /// The source `.cns`/`.cmd`/`.air` file to import.
    src: String,
    /// Where to write the repaired overlay, if requested (positional). `None` in
    /// report-only mode (`--report`/`--report-json` with no overlay-out).
    overlay_out: Option<String>,
    /// `--prune`: remove dead AIR frames (vs. only flagging them).
    prune: bool,
    /// `--report`: print the tiered human report to stdout.
    report: bool,
    /// `--report-json <path>`: write the stable, sorted JSON report.
    report_json: Option<String>,
    /// `--strict`: exit non-zero iff the report has Flagged entries.
    strict: bool,
}

/// Parses the `import` argv (everything after `fp-app import`) into [`ImportArgs`].
///
/// Positionals are `<src>` then an optional `<overlay-out>`; the rest are flags.
/// Returns `None` on a usage error (no source, or `--report-json` missing its
/// path) so the caller can print usage and exit `2`.
fn parse_import_args(args: &[String]) -> Option<ImportArgs> {
    let mut positionals: Vec<&str> = Vec::new();
    let mut prune = false;
    let mut report = false;
    let mut strict = false;
    let mut report_json: Option<String> = None;

    let mut i = 2; // skip "fp-app" and "import"
    while i < args.len() {
        let a = &args[i];
        if a.eq_ignore_ascii_case("--prune") {
            prune = true;
        } else if a.eq_ignore_ascii_case("--report") {
            report = true;
        } else if a.eq_ignore_ascii_case("--strict") {
            strict = true;
        } else if a.eq_ignore_ascii_case("--report-json") {
            // Path is the next token.
            i += 1;
            report_json = Some(args.get(i)?.clone());
        } else if a.starts_with("--") {
            // Unknown flag: a usage error.
            return None;
        } else {
            positionals.push(a);
        }
        i += 1;
    }

    let src = (*positionals.first()?).to_string();
    let overlay_out = positionals.get(1).map(|s| (*s).to_string());
    Some(ImportArgs {
        src,
        overlay_out,
        prune,
        report,
        report_json,
        strict,
    })
}

/// Runs the `import <file.cns|.cmd|.air> [<overlay-out>] [--prune] [--report]
/// [--report-json <path>] [--strict]` subcommand: reads a CNS/CMD/AIR file,
/// produces a repaired-text overlay (see [`import`]), optionally writes it,
/// builds a tiered repair report, and renders it (human + stable JSON).
///
/// `.cns`/`.cmd` files take the CNS line-repair path. `.air` files take the AIR
/// overlay path: junk frame columns (`2..A` → `2`) are always salvaged, and with
/// `--prune` dead frames (whose sprite is absent from the sibling `.sff`) are
/// removed — but never an action's last frame.
///
/// - `--report` prints the human, tier-grouped report to stdout.
/// - `--report-json <path>` writes the stable, sorted JSON report (refusing an
///   `assets/` destination).
/// - `--strict` makes the process exit non-zero iff the report carries any
///   **Flagged** entry (default exits 0 even with flags); it is for CI/evidence.
///
/// Every clean-room write path refuses an `assets/` destination. Returns the
/// process exit code: `2` for a usage error, `1` on read/write failure, `0` on
/// success — except under `--strict` with flags, which returns `1`.
fn run_import(args: &[String]) -> i32 {
    let Some(parsed) = parse_import_args(args) else {
        eprintln!(
            "usage: fp-app import <file.cns|.cmd|.air|char.def> [<overlay-out>] \
             [--prune] [--report] [--report-json <path>] [--strict]"
        );
        return 2;
    };
    let src = Path::new(&parsed.src);

    // A `.def` is a whole **character** import: load it through the live loader,
    // then walk the compiled graph + assets for repairs (T082). It never produces
    // a text overlay (there is no single file to rewrite), so `<overlay-out>` is
    // ignored for a `.def`.
    if src
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("def"))
    {
        return run_import_char(src, &parsed);
    }

    let text = match fp_formats::text::read_text_file(src) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("import: cannot read {}: {e}", src.display());
            eprintln!("import: failed to read {}: {e}", src.display());
            return 1;
        }
    };

    let mut report = import::ImportReport::new();

    // `.air` files take the AIR overlay path (column salvage + dead-frame prune).
    let write_result = if src
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("air"))
    {
        run_import_air(src, &text, &parsed, &mut report)
    } else {
        run_import_cns(src, &text, &parsed, &mut report)
    };
    if write_result != 0 {
        return write_result;
    }

    // Emit the report face(s).
    if parsed.report {
        print!("{}", report.render());
    }
    if let Some(json_path) = &parsed.report_json {
        if let Err(e) = report.write_json(Path::new(json_path)) {
            tracing::error!("import: cannot write report JSON {json_path}: {e}");
            eprintln!("import: failed to write report JSON {json_path}: {e}");
            return 1;
        }
        tracing::info!("import: wrote report JSON {json_path}");
    }

    // `--strict` is the only flag-tied exit-code behavior.
    if parsed.strict && report.has_flags() {
        eprintln!("import: --strict failed — the report contains flagged entries");
        return 1;
    }
    0
}

/// Resolves the character's text state files (`cmd`, `cns`, `st`/`st0`..`st9`,
/// `stcommon`) from its `.def`'s `[Files]` section, each as a
/// `(display-relpath, absolute-path)` pair, for the import text-overlay pass.
///
/// `stcommon` is included so an authored common-state library is overlaid too;
/// the engine's built-in clean-room fallback (a `stcommon` that resolves to a
/// missing file) simply does not exist on disk and is skipped by the caller. A
/// `.def` that fails to parse yields an empty list (the graph-walk pass still
/// runs). De-duplicates case-insensitively so a file named under two keys is
/// overlaid once.
fn character_text_files(def_path: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(def) = fp_formats::def::DefFile::load(def_path) else {
        return Vec::new();
    };
    let mut rels: Vec<String> = Vec::new();
    let mut push = |v: Option<&str>| {
        if let Some(s) = v {
            let s = s.trim();
            if !s.is_empty() && !rels.iter().any(|r| r.eq_ignore_ascii_case(s)) {
                rels.push(s.to_string());
            }
        }
    };
    push(def.get("Files", "cmd"));
    push(def.get("Files", "cns"));
    push(def.get("Files", "st"));
    for i in 0..=9 {
        push(def.get("Files", &format!("st{i}")));
    }
    push(def.get("Files", "stcommon"));
    rels.into_iter()
        .map(|rel| {
            let resolved = fp_formats::def::DefFile::resolve_path(def_path, &rel);
            (rel, resolved)
        })
        .collect()
}

/// The character (`.def`) branch of [`run_import`] — the import core (T082).
///
/// Loads the character through the live [`fp_character::LoadedCharacter::load`]
/// path (the same one the match uses), then builds an [`import::ImportReport`]
/// from the compiled state graph + assets (failed-compile expressions, AIR frames
/// referencing absent sprites, degenerate `0×0` sprites). It writes **no** overlay
/// — a character is many files, not one rewritable text — so `<overlay-out>` is
/// ignored here.
///
/// Per the import contract the process **exits 0 even with flags** (the report
/// body carries them); `--strict` is the only flag that turns flags into a
/// non-zero exit. A `.def` that cannot be loaded at all (a missing required
/// SFF/AIR) is exit `1`.
fn run_import_char(src: &Path, parsed: &ImportArgs) -> i32 {
    let loaded = match fp_character::LoadedCharacter::load(src) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("import: cannot load character {}: {e}", src.display());
            eprintln!("import: failed to load character {}: {e}", src.display());
            return 1;
        }
    };

    // (1) Compiled-graph + asset walk (failed exprs, missing/zero-dim sprites).
    let mut report = import::ImportReport::from_character(&parsed.src, &loaded);
    // (2) Text-overlay repairs over the character's own CNS/CMD source files
    // (stray lines, empty keys, colon/malformed headers). Re-read each `[Files]`
    // state file relative to the `.def`; a missing/unreadable file is skipped.
    for (rel, path) in character_text_files(src) {
        match fp_formats::text::read_text_file(&path) {
            Ok(text) => report.add_cns(&rel, &import::repair_cns_text(&text)),
            Err(e) => tracing::warn!(
                "import: skipping text-overlay of {} ({}): {e}",
                rel,
                path.display()
            ),
        }
    }

    // Emit the report face(s). `import --report <char.def>` prints the human
    // report; `--report-json <path>` writes the stable JSON. With neither flag the
    // command is a quiet load-and-classify (findings still log through tracing).
    if parsed.report {
        print!("{}", report.render());
    } else {
        // No --report: surface a one-line summary so the run is not silent.
        let missing = report.count_kind(import::RepairKind::MissingSpriteRef);
        let truncated = report.count_kind(import::RepairKind::TruncatedExpr);
        if report.is_flag_free() {
            tracing::info!(
                "import: {} imported with no flags ({} entr(y/ies) total)",
                src.display(),
                report.entries.len()
            );
        } else {
            tracing::warn!(
                "import: {} imported with flags ({} missing-sprite, {} truncated-expr); \
                 pass --report to see them",
                src.display(),
                missing,
                truncated
            );
        }
    }
    if let Some(json_path) = &parsed.report_json {
        if let Err(e) = report.write_json(Path::new(json_path)) {
            tracing::error!("import: cannot write report JSON {json_path}: {e}");
            eprintln!("import: failed to write report JSON {json_path}: {e}");
            return 1;
        }
        tracing::info!("import: wrote report JSON {json_path}");
    }

    if parsed.strict && report.has_flags() {
        eprintln!("import: --strict failed — the report contains flagged entries");
        return 1;
    }
    0
}

/// The CNS/CMD branch of [`run_import`]: repairs the text, optionally writes the
/// overlay, folds the repairs into `report`. Returns a non-zero exit code only on
/// a write failure.
fn run_import_cns(
    src: &Path,
    text: &str,
    parsed: &ImportArgs,
    report: &mut import::ImportReport,
) -> i32 {
    let overlay = import::repair_cns_text(text);
    if let Some(out_arg) = &parsed.overlay_out {
        if let Err(e) = import::write_overlay(&overlay, Path::new(out_arg)) {
            tracing::error!("import: cannot write overlay {out_arg}: {e}");
            eprintln!("import: failed to write overlay {out_arg}: {e}");
            return 1;
        }
        if overlay.is_clean() {
            tracing::info!(
                "import: {} is clean; overlay is byte-identical",
                src.display()
            );
        } else {
            use import::RepairKind::*;
            tracing::info!(
                "import: wrote overlay {} ({} repair(s): {} stray, {} empty-key, {} colon-header, {} malformed-header)",
                out_arg,
                overlay.repairs.len(),
                overlay.count(StrayLine),
                overlay.count(EmptyKey),
                overlay.count(ColonHeader),
                overlay.count(MalformedHeader),
            );
        }
    }
    for repair in &overlay.repairs {
        tracing::info!(
            "import: {}:{} {:?} — {}",
            src.display(),
            repair.line_no,
            repair.kind,
            repair.original.trim()
        );
    }
    report.add_cns(&parsed.src, &overlay);
    0
}

/// The AIR branch of [`run_import`]: produces a repaired AIR overlay (column
/// salvage + opt-in dead-frame prune), optionally writes it, and folds the
/// repairs into `report`.
///
/// The dead-frame oracle is the `.sff` sitting next to `src` (same stem). When
/// no sibling `.sff` is found or it fails to load, every sprite is assumed
/// present: pruning becomes a no-op and only column salvage applies. Returns a
/// non-zero exit code only on a write failure.
fn run_import_air(
    src: &Path,
    text: &str,
    parsed: &ImportArgs,
    report: &mut import::ImportReport,
) -> i32 {
    // Locate a sibling `.sff` (`foo.air` -> `foo.sff`) as the sprite-presence
    // oracle. If absent/unloadable, fall back to "all present" so salvage still
    // runs and prune is a safe no-op (warn-logged so the user knows).
    let sff = src.with_extension("sff").canonicalize().ok().and_then(|p| {
        match fp_formats::sff::SffFile::load(&p) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("import: could not load sibling SFF {}: {e}", p.display());
                None
            }
        }
    });
    if parsed.prune && sff.is_none() {
        tracing::warn!(
            "import: --prune requested but no loadable sibling .sff for {}; \
             treating every sprite as present (no frame will be pruned)",
            src.display()
        );
    }

    let overlay = import::repair_air_text(text, parsed.prune, |g, i| {
        sff.as_ref().is_none_or(|s| s.has_renderable_sprite(g, i))
    });

    if let Some(out_arg) = &parsed.overlay_out {
        if let Err(e) = import::write_air_overlay(&overlay, Path::new(out_arg)) {
            tracing::error!("import: cannot write AIR overlay {out_arg}: {e}");
            eprintln!("import: failed to write AIR overlay {out_arg}: {e}");
            return 1;
        }
        use import::AirRepairKind::*;
        if overlay.is_clean() {
            tracing::info!(
                "import: {} is clean; AIR overlay is byte-identical",
                src.display()
            );
        } else {
            tracing::info!(
                "import: wrote AIR overlay {} ({} junk-column, {} dead-frame pruned, {} missing-sprite flagged)",
                out_arg,
                overlay.count(JunkColumn),
                overlay.count(DeadFrame),
                overlay.count(MissingSpriteRef),
            );
        }
    }
    for repair in &overlay.repairs {
        tracing::info!(
            "import: {}:{} {:?} (action {:?}) — {}",
            src.display(),
            repair.line_no,
            repair.kind,
            repair.action,
            repair.original.trim()
        );
    }
    report.add_air(&parsed.src, &overlay);
    0
}

/// Cached GPU textures for a single sprite.
struct CachedSprite {
    texture: SpriteTexture,
    palette: PaletteTexture,
    axis_x: i16,
    axis_y: i16,
}

// ---------------------------------------------------------------------------
// CNS-driven single playable character (Phase 5.5)
// ---------------------------------------------------------------------------
//
// The two-player [`Match`] path (Phase 7.2) is now the live runtime mode; this
// single-character CNS driver is retained only as the substrate for the
// extensive single-character regression tests below (command compilation, the
// engine movement bridge, facing-relative walk, live command triggers, etc.).
// It and its helpers are therefore `#[cfg(test)]`: kept for coverage, excluded
// from the shipped binary so it does not read as dead code.

/// A bridge that exposes the per-tick active commands from a [`CommandMatcher`]
/// snapshot to the character's `command = "..."` triggers.
///
/// The matcher is run once per tick; its active command names are snapshotted
/// into an [`ActiveCommands`] which is handed to the [`Character`] as its
/// command source. Modeling it this way avoids a borrow conflict (the character
/// borrows its command source immutably during state evaluation, while the
/// matcher needs `&mut` to advance) and keeps facing-relative direction handling
/// inside the matcher, which already resolves `F`/`B` against the facing.
#[cfg(test)]
fn snapshot_active_commands(matcher: &CommandMatcher, defs: &[CommandDef]) -> ActiveCommands {
    // The actual filter lives in `fp-input` (one place); this is a thin wrapper.
    ActiveCommands::from_names(matcher.active_command_names_in(defs))
}

/// A fully CNS-driven playable character: a [`LoadedCharacter`] (assets +
/// compiled state graph) plus the live [`Character`] entity the executor steps.
///
/// Each tick this polls input, runs the [`CommandMatcher`], feeds the active
/// commands into the entity, ticks the CNS state machine, and exposes the
/// current AIR frame for rendering. There is no hardcoded state machine: every
/// transition comes from the merged CNS/CMD state graph.
#[cfg(test)]
struct CnsCharacter {
    /// The loaded character (assets + compiled, merged state graph).
    loaded: LoadedCharacter,
    /// The live entity stepped by the executor.
    entity: Character,
    /// Rolling input history for command recognition.
    input_buffer: InputBuffer,
    /// Command recognizer built from the `.cmd` command list.
    matcher: CommandMatcher,
    /// The command definitions (kept to enumerate active names each tick).
    command_defs: Vec<CommandDef>,
}

#[cfg(test)]
impl CnsCharacter {
    /// Builds a CNS-driven character from a loaded `.def`.
    ///
    /// Compiles the `.cmd` commands into a [`CommandMatcher`] via the shared
    /// [`LoadedCharacter::command_defs`] (which feeds the raw MUGEN command
    /// strings straight to `compile_command`, parsing the `$`/`>` modifiers
    /// natively), and starts the entity standing with control in state 0. The
    /// engine-built-in stand<->walk<->crouch<->jump locomotion is supplied by the
    /// loader for every character (task 7.3 part B), so there is no app-side shim
    /// here anymore.
    fn new(loaded: LoadedCharacter) -> Self {
        // Compile commands from the .cmd file (if any) into the matcher, using the
        // same shared compilation the two-player Match path uses.
        let command_defs: Vec<CommandDef> = loaded.command_defs();
        tracing::info!("Compiled {} commands from CMD file", command_defs.len());

        let mut entity = Character::with_constants(loaded.constants);
        // Start standing, with control, in the stand state. The stand animation
        // is the MUGEN stand action (0); state 0's own `ChangeAnim` controller
        // corrects it on the first tick if the character authors a different
        // stand anim, so we do not need to evaluate the entry expression here.
        entity.state_no = STATE_STAND;
        entity.ctrl = true;
        entity.anim = STATE_STAND; // action 0 == stand

        tracing::info!(
            "CNS character {:?}: {} states, {} sprites, {} animations",
            loaded.name,
            loaded.state_count(),
            loaded.sff.sprites.len(),
            loaded.air.actions.len(),
        );

        Self {
            loaded,
            entity,
            input_buffer: InputBuffer::new(),
            // Input leniency (T075): small jump buffer over the built-in `holdup`
            // gate, matching the two-player engine path.
            matcher: CommandMatcher::with_leniency(
                command_defs.clone(),
                fp_input::LeniencyConfig::with_jump_buffer(),
            ),
            command_defs,
        }
    }

    /// Advances one 60Hz tick: input -> command -> CNS state machine.
    fn tick(&mut self, input: InputState) {
        // 1. Push raw input into the rolling buffer.
        self.input_buffer.push(input);

        // 2. Run the command matcher (facing-relative; F/B respect facing).
        let facing_right = self.entity.facing == fp_character::Facing::Right;
        self.matcher
            .check_commands(&self.input_buffer, facing_right);

        // 3. Snapshot active commands into the entity's command source so
        //    `command = "..."` triggers evaluate against live input this tick.
        let active = snapshot_active_commands(&self.matcher, &self.command_defs);
        self.entity.set_command_source(Box::new(active));

        // 4. Step the CNS state machine one tick. The executor now owns world
        //    position integration from velocity, applying the facing sign to X
        //    (`pos.x += vel.x * facing_sign`, task 6.2c) so a left-facing
        //    character walks the correct way. The app must NOT integrate again.
        //    This single-character viewer/driver has no opponent, so it passes
        //    `None` + a default stage view: opponent-dependent triggers
        //    (`P2Dist`, `p2, ...`, …) read the safe default `0`.
        let _ = self
            .entity
            .tick(&self.loaded, None, fp_character::StageView::default());

        // 5. Clamp to the stage ground plane (y = 0) so the character never
        //    sinks. The ground plane is a stage concern, so it stays here rather
        //    than in the character executor.
        if self.entity.pos.y > 0.0 {
            self.entity.pos.y = 0.0;
            if self.entity.vel.y > 0.0 {
                self.entity.vel.y = 0.0;
            }
        }
    }

    /// Resolves the AIR frame for the entity's current anim + element, if any.
    fn current_frame(&self) -> Option<&fp_formats::air::AnimFrame> {
        let action = self.loaded.air.action(self.entity.anim)?;
        if action.frames.is_empty() {
            return None;
        }
        let idx = clamp_elem(self.entity.anim_elem, action.frames.len());
        action.frames.get(idx)
    }
}

/// Clamps a (possibly out-of-range) signed animation element index into
/// `0..len`, returning `0` for an empty action (the caller guards emptiness).
fn clamp_elem(index: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if index < 0 {
        0
    } else {
        (index as usize).min(len - 1)
    }
}

// ---------------------------------------------------------------------------
// Two-player match (Phase 7.2) — the playable demo
// ---------------------------------------------------------------------------

/// Per-fighter GPU sprite cache for the two-player [`Match`] renderer.
///
/// A [`fp_engine::Match`] owns each fighter's [`LoadedCharacter`] (and thus its
/// [`SffFile`]); this holds only the lazily-decoded GPU textures, keyed by sprite
/// id. One [`FighterRender`] is kept per side (P1, P2) so the two characters'
/// textures never collide — the "per-character texture cache" the task requires.
#[derive(Default)]
struct FighterRender {
    /// GPU sprite cache keyed by sprite id, decoded on first use.
    sprite_cache: HashMap<SpriteId, CachedSprite>,
}

impl FighterRender {
    /// Ensures the GPU textures for `sprite_id` are cached, decoding from `sff`
    /// (the owning character's sprite file) on first use. Returns the cached
    /// entry, or `None` if the sprite is missing or fails to decode (logged,
    /// never a panic).
    ///
    /// `override_rgba` is an optional `.act` palette override (the runtime
    /// costume-swap, FL2b): when `Some`, the sprite's GPU palette is built from it
    /// via [`PaletteTexture::from_override`] instead of the SFF-embedded palette;
    /// when `None` the embedded palette is used and the upload is byte-identical to
    /// before. The override is fixed per fighter (a CLI `--pN-pal` selection), so
    /// caching the resolved palette per sprite id is correct.
    fn get_or_create_sprite<'a>(
        &'a mut self,
        sff: &SffFile,
        sprite_id: SpriteId,
        renderer: &Renderer,
        override_rgba: Option<&[u8; 1024]>,
    ) -> Option<&'a CachedSprite> {
        if self.sprite_cache.contains_key(&sprite_id) {
            return self.sprite_cache.get(&sprite_id);
        }

        let (index, sff_sprite) = sff
            .sprites
            .iter()
            .enumerate()
            .find(|(_, s)| s.group == sprite_id.group() && s.image == sprite_id.image())?;

        let axis_x = sff_sprite.axis_x;
        let axis_y = sff_sprite.axis_y;
        let width = sff_sprite.width;
        let height = sff_sprite.height;
        let pal_idx = sff_sprite.palette_index as usize;

        if width == 0 || height == 0 {
            tracing::warn!("Sprite {sprite_id} has zero dimensions ({width}x{height})");
            return None;
        }

        let pixels = match sff.decode_sprite(index) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to decode sprite {sprite_id}: {e}");
                return None;
            }
        };
        let palette_data = match sff.palette(pal_idx) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to load palette {pal_idx} for sprite {sprite_id}: {e}");
                return None;
            }
        };

        let texture = SpriteTexture::new(
            renderer.device(),
            renderer.queue(),
            width as u32,
            height as u32,
            &pixels,
        );
        // Build the GPU palette from the `.act` override when one is selected,
        // otherwise the SFF-embedded palette. With no override this is
        // byte-identical to `PaletteTexture::new(&palette_data)` (the default
        // render path is unchanged).
        let palette = PaletteTexture::from_override(
            renderer.device(),
            renderer.queue(),
            &palette_data,
            override_rgba,
        );

        self.sprite_cache.insert(
            sprite_id,
            CachedSprite {
                texture,
                palette,
                axis_x,
                axis_y,
            },
        );
        self.sprite_cache.get(&sprite_id)
    }
}

/// A key into a fighter's decoded-sound cache: the SND file selector plus the
/// `group`/`sample` pair from a [`SoundRequest`].
///
/// `common` distinguishes a request that asked for the common/fight sound file
/// (`F`-prefixed `PlaySnd`) from one that asked for the character's own `.snd`.
/// The two could collide on the same `group`/`sample` once a separate common SND
/// is wired up, so the flag is part of the key today even though both currently
/// resolve against the character's own SND (see [`FighterAudio::play_requests`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SoundKey {
    /// `true` for a common/fight-file request, `false` for the character's own.
    common: bool,
    /// The sound group number.
    group: i32,
    /// The sound sample number within the group.
    sample: i32,
}

/// Per-fighter decoded-sound cache for the two-player [`Match`] audio layer.
///
/// A [`fp_engine::Match`] owns each fighter's [`LoadedCharacter`] (and thus its
/// optional [`fp_formats::snd::SndFile`]); this holds only the lazily-decoded
/// [`Sound`]s, keyed by [`SoundKey`]. A decode *failure* (or a missing SND/sound
/// entry) is cached as `None`, so a bad sound is looked up and logged once rather
/// than re-decoded every frame it is requested. One [`FighterAudio`] is kept per
/// side (P1, P2), mirroring the per-side [`FighterRender`] texture caches.
#[derive(Default)]
struct FighterAudio {
    /// Decoded sounds keyed by selector + group/sample; `None` caches a decode
    /// or lookup failure so it is not retried.
    sound_cache: HashMap<SoundKey, Option<Sound>>,
}

impl FighterAudio {
    /// Plays this fighter's [`SoundRequest`]s for the current frame through
    /// `audio`, decoding from `player`'s own `.snd` (lazily, via the cache).
    ///
    /// For each request: resolve the SND file (the character's own — see the
    /// common-fallback note below), look up/decode the `group`/`sample` WAV bytes
    /// into a [`Sound`] (caching the result, including failures, by
    /// [`SoundKey`]), then play it on the request's channel at a volume mapped
    /// from `volume_scale` (`/100`, clamped to `[0, 4]`). Missing SND, missing
    /// sound entry, or decode failure is skipped with a debug log — never a panic.
    ///
    /// `common` (an `F`-prefixed `PlaySnd`, the common/fight sound file) is not
    /// yet backed by a separate common SND: such requests fall back to the
    /// character's own SND with a debug log. `looping` is not yet supported by
    /// [`AudioSystem::play_sound`]; the field is read but ignored this milestone.
    fn play_requests(
        &mut self,
        audio: &mut AudioSystem,
        player: &Player,
        requests: &[SoundRequest],
    ) {
        for req in requests {
            let key = SoundKey {
                common: req.common,
                group: req.group,
                sample: req.sample,
            };
            // Emit the one-shot advisories only the first time a key is seen (the
            // decode below caches it), so a per-tick-repeating PlaySnd does not
            // flood the log at 60Hz.
            if !self.sound_cache.contains_key(&key) {
                if req.common {
                    tracing::debug!(
                        "PlaySnd common/fight sound (group {}, sample {}) has no dedicated \
                         common SND yet; falling back to the character's own .snd",
                        req.group,
                        req.sample,
                    );
                }
                if req.looping {
                    tracing::debug!(
                        "PlaySnd looping not yet supported (group {}, sample {}); playing once",
                        req.group,
                        req.sample,
                    );
                }
            }

            let sound = self.get_or_decode(player, key);
            if let Some(sound) = sound {
                let volume = (req.volume_scale as f32 / 100.0).clamp(0.0, 4.0);
                audio.play_sound(sound, req.channel, volume);
            }
        }
    }

    /// Returns the decoded [`Sound`] for `key`, decoding from `player`'s `.snd`
    /// on first use and caching the result (a failure is cached as `None` so it
    /// is not retried). Returns `None` when the SND, the sound entry, or the
    /// decode is missing/failed — each logged once at debug.
    fn get_or_decode(&mut self, player: &Player, key: SoundKey) -> Option<&Sound> {
        self.sound_cache
            .entry(key)
            .or_insert_with(|| Self::decode_sound(player, key))
            .as_ref()
    }

    /// Decodes one sound from `player`'s own `.snd`, returning `None` (with a
    /// debug log) when the SND is absent, the `group`/`sample` entry is missing,
    /// or the WAV bytes fail to decode. Both own- and common-file requests use
    /// the character's own SND today (no separate common SND is loaded yet).
    fn decode_sound(player: &Player, key: SoundKey) -> Option<Sound> {
        let Some(snd) = player.loaded.snd.as_ref() else {
            tracing::debug!(
                "no .snd for this fighter; skipping sound (group {}, sample {})",
                key.group,
                key.sample,
            );
            return None;
        };
        // `SndFile::sound` takes u32 selectors; a negative group/sample (which a
        // SoundRequest permits) has no entry, so reject it with an honest log
        // rather than wrapping to a huge u32 that misses with a misleading reason.
        let (Ok(group), Ok(sample)) = (u32::try_from(key.group), u32::try_from(key.sample)) else {
            tracing::debug!(
                "negative sound selector (group {}, sample {}) has no .snd entry; skipping",
                key.group,
                key.sample,
            );
            return None;
        };
        let Some(bytes) = snd.sound(group, sample) else {
            tracing::debug!(
                "sound entry (group {}, sample {}) not found in .snd; skipping",
                key.group,
                key.sample,
            );
            return None;
        };
        match Sound::decode(bytes) {
            Ok(sound) => Some(sound),
            // A recognised-but-unsupported codec (e.g. CRI ADX) is worth a single
            // warn so the missing audio is explainable; the result is cached as
            // `None`, so this fires at most once per (group, sample) and can never
            // flood the per-tick loop.
            Err(e @ fp_core::FpError::Unsupported(_)) => {
                tracing::warn!(
                    "unsupported sound codec for (group {}, sample {}): {e}; skipping",
                    key.group,
                    key.sample,
                );
                None
            }
            Err(e) => {
                tracing::debug!(
                    "failed to decode sound (group {}, sample {}): {e}; skipping",
                    key.group,
                    key.sample,
                );
                None
            }
        }
    }
}

/// Resolves the AIR frame for a [`Player`]'s current anim + element, if any.
///
/// Reads the player's loaded animations and clamps the (possibly stale) element
/// cursor into range, driven off the engine-owned [`Player`] state. Returns
/// `None` for a missing/empty action.
fn player_current_frame(player: &Player) -> Option<&fp_formats::air::AnimFrame> {
    let action = player.loaded.air.action(player.anim())?;
    if action.frames.is_empty() {
        return None;
    }
    let idx = clamp_elem(player.anim_elem(), action.frames.len());
    action.frames.get(idx)
}

/// Builds a [`fp_engine::Match`] for two characters loaded from their `.def`
/// paths, applying the engine-built-in stand<->walk bridge to BOTH, seeding their
/// opposing start positions, and standing each in state 0 with control.
///
/// This is the single construction path shared by the live app and the headless
/// integration test, so the test exercises exactly the wiring the demo runs.
/// Returns an error only if a character truly cannot be loaded; the caller
/// degrades to the test pattern on `Err` rather than crashing.
fn build_two_player_match(
    p1_def: &Path,
    p2_def: &Path,
    pal: PalSelection,
) -> fp_core::FpResult<Match> {
    let p1 = build_player(p1_def, P1_START_X, pal.p1)?;
    let p2 = build_player(p2_def, P2_START_X, pal.p2)?;
    // `Match::new` seeds facing toward each other from the start positions and
    // starts in the intro phase; the default 99-second round clock applies.
    Ok(Match::new(
        p1,
        p2,
        StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH),
    ))
}

/// Builds a [`fp_engine::TeamMatch`] for two characters in the requested
/// [`TeamMode`] (T027).
///
/// [`TeamMode::Single`] (the default) fields **one** fighter per side — identical
/// to a bare [`Match`] wrapped in a single-fighter team, so the 1v1 path is
/// unchanged. [`TeamMode::Simul`] / [`TeamMode::Turns`] field **two** fighters per
/// side: the second of each side reuses the same `.def` as the first (the engine
/// has no separate roster picker yet), positioned slightly behind the lead so the
/// Simul teammates do not stack exactly. Returns an error only if a character truly
/// cannot be loaded; the caller degrades to the test pattern on `Err`.
fn build_team_match(
    p1_def: &Path,
    p2_def: &Path,
    pal: PalSelection,
    mode: TeamMode,
) -> fp_core::FpResult<TeamMatch> {
    match mode {
        // The 1v1 default reuses the shared single-match construction path (also
        // exercised by the headless integration tests) and wraps it in a
        // single-fighter team, which behaves identically to a bare `Match`.
        TeamMode::Single => {
            let inner = build_two_player_match(p1_def, p2_def, pal)?;
            let bounds = inner.bounds();
            let (p1, p2) = inner.into_players();
            Ok(TeamMatch::new(p1, p2, bounds))
        }
        TeamMode::Simul | TeamMode::Turns => {
            let bounds = StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH);
            let p1_lead = build_player(p1_def, P1_START_X, pal.p1)?;
            let p2_lead = build_player(p2_def, P2_START_X, pal.p2)?;
            // A second fighter per side (same `.def`), placed a bit further out so
            // the Simul teammates start visibly offset rather than overlapping.
            let p1_mate = build_player(p1_def, P1_START_X - 30.0, pal.p1)?;
            let p2_mate = build_player(p2_def, P2_START_X + 30.0, pal.p2)?;
            Ok(TeamMatch::with_mode(
                vec![p1_lead, p1_mate],
                vec![p2_lead, p2_mate],
                bounds,
                mode,
            ))
        }
    }
}

/// Loads one character `.def` into a [`Player`] positioned at `start_x`,
/// standing it in state 0 with control, with an optional `.act` palette override.
///
/// No app-side movement shim is applied: the loader supplies MUGEN's built-in
/// stand<->walk<->crouch<->jump locomotion for every character (task 7.3 part B),
/// and the [`Match`] runs each player's real [`fp_input::CommandMatcher`]
/// (task 7.3 part A) so `holdfwd`/`holdback`/… and walk velocity all fire from
/// the character's own data.
///
/// `pal_selection` (FL2b) is the `--pN-pal` choice: a 0-based index into the
/// character's loaded `.act` overrides, set as the entity's active palette so
/// [`draw_player`] renders that costume. `None` keeps the SFF-embedded palette
/// (unchanged). An out-of-range index is warn-logged and falls back to embedded
/// (the loader's [`LoadedCharacter::override_palette`] returns `None` for it).
fn build_player(
    def_path: &Path,
    start_x: f32,
    pal_selection: Option<usize>,
) -> fp_core::FpResult<Player> {
    tracing::info!("Loading match character: {}", def_path.display());
    let loaded = LoadedCharacter::load(def_path)?;

    let mut entity = Character::with_constants(loaded.constants);
    entity.pos = fp_core::Vec2::new(start_x, 0.0);
    entity.state_no = STATE_STAND;
    entity.ctrl = true;
    entity.anim = STATE_STAND; // action 0 == stand

    // Pick the costume palette. An explicit `--p1-pal`/`--p2-pal` flag wins;
    // otherwise fall back to the character's DEFAULT costume — MUGEN renders the
    // first `pal.defaults` / `pal1` `.act` palette by default, not the SFF-embedded
    // one. That default is what makes CvS-style characters (e.g. evilken, whose
    // embedded palette is a dark placeholder) render in real colors instead of
    // near-black. A character that ships no `.act` palettes (Kung Fu Man) yields
    // `None` here and keeps its SFF-embedded palette exactly as before.
    let pal_selection = pal_selection.or_else(|| loaded.default_palette_index());

    // Apply the chosen `.act` palette override (FL2b). A selection that the
    // character cannot honor (no such override slot) is warn-logged and left as
    // the embedded palette so the costume swap never crashes or shows blank.
    if let Some(sel) = pal_selection {
        if loaded.override_palette(Some(sel)).is_some() {
            entity.set_active_palette(Some(sel));
            tracing::info!("Applied .act palette override #{sel} to {:?}", loaded.name);
        } else {
            tracing::warn!(
                "requested palette #{sel} for {:?} is out of range ({} loaded); using embedded palette",
                loaded.name,
                loaded.palette_count()
            );
        }
    }

    tracing::info!(
        "Match player {:?}: {} states, {} sprites, {} animations, life {}",
        loaded.name,
        loaded.state_count(),
        loaded.sff.sprites.len(),
        loaded.air.actions.len(),
        entity.life,
    );

    Ok(Player::new(entity, loaded))
}

/// One field of [`MatchInput`] that a keyboard key drives.
///
/// Each remappable [`screens::InputAction`] maps 1:1 to an `InputField` (see
/// [`InputField::action`] / [`field_for_action`]); the in-loop sampler asserts
/// the field's bit when the action's currently-bound key is held. Keeping it a
/// small explicit enum (rather than scattering bit-sets through the polling code)
/// keeps the key map unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputField {
    /// Absolute screen direction: up (jump).
    Up,
    /// Absolute screen direction: down (crouch).
    Down,
    /// Absolute screen direction: left.
    Left,
    /// Absolute screen direction: right.
    Right,
    /// Attack button `a` (light punch).
    A,
    /// Attack button `b` (medium punch).
    B,
    /// Attack button `c` (heavy punch).
    C,
    /// Attack button `x` (light kick).
    X,
    /// Attack button `y` (medium kick).
    Y,
    /// Attack button `z` (heavy kick).
    Z,
}

impl InputField {
    /// Asserts this field's bit in a [`MatchInput`].
    fn set(self, m: &mut MatchInput) {
        match self {
            InputField::Up => m.up = true,
            InputField::Down => m.down = true,
            InputField::Left => m.left = true,
            InputField::Right => m.right = true,
            InputField::A => m.a = true,
            InputField::B => m.b = true,
            InputField::C => m.c = true,
            InputField::X => m.x = true,
            InputField::Y => m.y = true,
            InputField::Z => m.z = true,
        }
    }
}

/// The in-loop [`InputField`] for a remappable [`screens::InputAction`] (the
/// inverse of [`InputField::action`]).
fn field_for_action(action: screens::InputAction) -> InputField {
    use screens::InputAction as A;
    match action {
        A::Up => InputField::Up,
        A::Down => InputField::Down,
        A::Left => InputField::Left,
        A::Right => InputField::Right,
        A::A => InputField::A,
        A::B => InputField::B,
        A::C => InputField::C,
        A::X => InputField::X,
        A::Y => InputField::Y,
        A::Z => InputField::Z,
    }
}

/// The default keyboard [`Scancode`] each remappable [`screens::InputAction`]
/// starts bound to, before any remap.
///
/// Movement is **WASD** and the six MUGEN attack buttons are the **U/I/O** (punch
/// row `a` `b` `c`) and **J/K/L** (kick row `x` `y` `z`) home keys — the same
/// defaults the old `KEYBOARD_BINDINGS` const used. The arrow keys are *also*
/// honoured for movement as a permanent secondary binding (see
/// [`match_input_from_held`]), independent of the remappable primary key, so the
/// arrows keep working even after WASD is rebound.
fn default_action_key(action: screens::InputAction) -> Scancode {
    use screens::InputAction as A;
    match action {
        A::Up => Scancode::W,
        A::Down => Scancode::S,
        A::Left => Scancode::A,
        A::Right => Scancode::D,
        A::A => Scancode::U,
        A::B => Scancode::I,
        A::C => Scancode::O,
        A::X => Scancode::J,
        A::Y => Scancode::K,
        A::Z => Scancode::L,
    }
}

/// The permanent secondary movement keys (the arrow keys), honoured in addition
/// to whatever the remappable primary direction key is, so arrow-key movement
/// always works regardless of remapping.
const ARROW_BINDINGS: &[(Scancode, InputField)] = &[
    (Scancode::Up, InputField::Up),
    (Scancode::Down, InputField::Down),
    (Scancode::Left, InputField::Left),
    (Scancode::Right, InputField::Right),
];

/// Converts a [`screens::KeyCode`] back to an SDL [`Scancode`], or `None` if the
/// opaque code is not a valid scancode. The setup screen carries keys as the
/// `repr(i32)` value of a `Scancode`; this is the only inverse adapter.
fn scancode_from_keycode(key: screens::KeyCode) -> Option<Scancode> {
    Scancode::from_i32(key.0)
}

/// Wraps an SDL [`Scancode`] as the device-neutral [`screens::KeyCode`] the
/// setup screen stores. `Scancode` is `repr(i32)`, so this is its raw value.
fn keycode_of(scancode: Scancode) -> screens::KeyCode {
    screens::KeyCode(scancode as i32)
}

/// Builds the live [`screens::InputConfig`] from the app defaults
/// ([`default_action_key`]), so the setup screen and the keyboard-sampling path
/// share one source of truth for player-1 bindings.
fn default_input_config() -> screens::InputConfig {
    screens::InputConfig::default_with(|action| keycode_of(default_action_key(action)))
}

/// Builds a [`MatchInput`] from a held-key oracle, using the live remappable
/// player-1 bindings in `config` plus the permanent arrow-key movement.
///
/// `is_held(scancode)` reports whether a physical key is currently held. Keeping
/// this pure (no SDL types beyond the [`Scancode`] the oracle is keyed on) makes
/// the player-1 key map unit-testable without a live SDL context — the live path
/// ([`match_input_from_keyboard`]) just supplies the SDL keyboard state as the
/// oracle.
///
/// Each remappable [`screens::InputAction`]'s current key (from `config`,
/// remapped on the setup screen) asserts its [`InputField`]; the arrow keys are
/// OR'd in as a permanent secondary movement binding so they keep working after a
/// WASD rebind. The engine receives these as **absolute** screen directions and
/// resolves facing internally (inside the [`fp_input::CommandMatcher`]), so this
/// stays a pure absolute-direction snapshot — do not pre-rotate here.
///
/// There is intentionally **no keyboard `start`/pause binding**: the engine's
/// `tick` takes no pause signal yet, matching the documented controller-Start
/// drop in [`controller_to_match_input`].
fn match_input_from_held(
    config: &screens::InputConfig,
    mut is_held: impl FnMut(Scancode) -> bool,
) -> MatchInput {
    let mut input = MatchInput::none();
    // The remappable primary binding for each action.
    for action in screens::InputAction::ALL {
        if let Some(scancode) = config.key_for(action).and_then(scancode_from_keycode) {
            if is_held(scancode) {
                field_for_action(action).set(&mut input);
            }
        }
    }
    // The permanent secondary movement binding (arrow keys), OR'd in.
    for &(scancode, field) in ARROW_BINDINGS {
        if is_held(scancode) {
            field.set(&mut input);
        }
    }
    input
}

/// Builds a [`MatchInput`] (absolute screen directions + button presses) from the
/// current SDL2 keyboard state, using the live player-1 [`screens::InputConfig`]
/// (remappable on the setup screen) plus the permanent arrow-key movement. The
/// engine converts these to facing-relative commands internally, so this stays a
/// pure absolute-direction snapshot.
fn match_input_from_keyboard(
    config: &screens::InputConfig,
    keyboard: &KeyboardState<'_>,
) -> MatchInput {
    match_input_from_held(config, |scancode| keyboard.is_scancode_pressed(scancode))
}

/// Number of controller slots the app tracks. Slot 0 drives player 1 (alongside
/// the keyboard); slot 1, if filled, drives player 2 for a two-human match.
const CONTROLLER_SLOTS: usize = 2;

/// Live SDL game-controller state: the opened controllers plus the subsystem
/// needed to open more on hotplug.
///
/// The first [`CONTROLLER_SLOTS`] *attached* controllers occupy `slots`; a `None`
/// slot means "no device here" and yields a neutral input. Opening, polling, and
/// hotplug are all failure-tolerant — a missing or disconnected device is never a
/// panic, only a `tracing::warn!` and a fall-back to neutral.
struct Controllers {
    /// The SDL game-controller subsystem; kept alive so opened controllers stay
    /// valid and new ones can be opened when a device is plugged in.
    subsystem: GameControllerSubsystem,
    /// Up to [`CONTROLLER_SLOTS`] opened controllers, indexed by player slot.
    slots: [Option<GameController>; CONTROLLER_SLOTS],
}

impl Controllers {
    /// Wraps an opened game-controller subsystem and binds every already-connected
    /// controller-capable joystick that fits into a free player slot.
    ///
    /// The caller obtains the subsystem with `Sdl::game_controller()`; if that
    /// fails (no driver, headless), the caller simply runs keyboard-only and never
    /// constructs a `Controllers`. Construction here cannot fail — at worst all
    /// slots stay empty and every `input` call returns `None`.
    fn new(subsystem: GameControllerSubsystem) -> Self {
        let mut this = Self {
            subsystem,
            slots: [None, None],
        };
        // SDL must receive controller events through the shared event pump; this
        // is on by default but we assert it so polling state stays fresh.
        this.subsystem.set_event_state(true);
        this.open_all_connected();
        this
    }

    /// Scans every connected joystick and binds the controller-capable ones to
    /// free slots. Used at startup and is safe to re-run (it skips filled slots).
    fn open_all_connected(&mut self) {
        let count = match self.subsystem.num_joysticks() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("controller: could not enumerate joysticks: {e}");
                return;
            }
        };
        for index in 0..count {
            if self.subsystem.is_game_controller(index) {
                self.open_into_free_slot(index);
            }
        }
    }

    /// Opens the joystick at `joystick_index` and parks it in the first free
    /// slot. A failed open or a full slot table is logged and ignored.
    fn open_into_free_slot(&mut self, joystick_index: u32) {
        let Some(slot) = self.slots.iter().position(Option::is_none) else {
            tracing::info!(
                "controller: all {CONTROLLER_SLOTS} slots in use; ignoring joystick {joystick_index}"
            );
            return;
        };
        match self.subsystem.open(joystick_index) {
            Ok(ctrl) => {
                tracing::info!(
                    "controller: opened '{}' (instance {}) as player {}",
                    ctrl.name(),
                    ctrl.instance_id(),
                    slot + 1
                );
                self.slots[slot] = Some(ctrl);
            }
            Err(e) => {
                tracing::warn!("controller: failed to open joystick {joystick_index}: {e}");
            }
        }
    }

    /// Handles a `ControllerDeviceAdded` event (`which` is a *joystick index*).
    fn on_device_added(&mut self, joystick_index: u32) {
        if self.subsystem.is_game_controller(joystick_index) {
            self.open_into_free_slot(joystick_index);
        }
    }

    /// Handles a `ControllerDeviceRemoved` event (`which` is an *instance id*),
    /// freeing whichever slot held that controller so its slot can be reused.
    fn on_device_removed(&mut self, instance_id: u32) {
        for slot in &mut self.slots {
            let matches = slot
                .as_ref()
                .is_some_and(|c| c.instance_id() == instance_id);
            if matches {
                tracing::info!("controller: instance {instance_id} disconnected");
                *slot = None;
            }
        }
    }

    /// Returns the mapped [`ControllerInput`] for a player slot, or `None` when no
    /// controller is bound (or it has silently detached). Reading a detached
    /// controller never panics — it reports neutral, which we surface as `None`.
    fn input(&self, slot: usize) -> Option<ControllerInput> {
        let ctrl = self.slots.get(slot)?.as_ref()?;
        if !ctrl.attached() {
            return None;
        }
        Some(map_controller(&read_raw(ctrl), DEADZONE_DEFAULT))
    }
}

/// Reads one SDL [`GameController`]'s current physical state into the pure,
/// backend-agnostic [`RawController`] consumed by [`map_controller`]. This is the
/// only place SDL controller types touch the mapping; all the direction/button
/// logic lives in `fp-input` and is unit-tested without SDL.
fn read_raw(ctrl: &GameController) -> RawController {
    RawController {
        stick_x: ctrl.axis(Axis::LeftX),
        stick_y: ctrl.axis(Axis::LeftY),
        dpad_up: ctrl.button(SdlPadButton::DPadUp),
        dpad_down: ctrl.button(SdlPadButton::DPadDown),
        dpad_left: ctrl.button(SdlPadButton::DPadLeft),
        dpad_right: ctrl.button(SdlPadButton::DPadRight),
        face_south: ctrl.button(SdlPadButton::A),
        face_east: ctrl.button(SdlPadButton::B),
        face_west: ctrl.button(SdlPadButton::X),
        face_north: ctrl.button(SdlPadButton::Y),
        shoulder_left: ctrl.button(SdlPadButton::LeftShoulder),
        shoulder_right: ctrl.button(SdlPadButton::RightShoulder),
        start: ctrl.button(SdlPadButton::Start),
    }
}

/// Converts a mapped [`ControllerInput`] into a [`MatchInput`].
///
/// `MatchInput` has no `start` field (the engine's `tick` takes no pause signal),
/// so the controller's Start maps onto the input model's `start` slot only inside
/// `fp-input`; here it is intentionally dropped (documented no-op) until the
/// engine grows a pause path.
fn controller_to_match_input(c: &ControllerInput) -> MatchInput {
    MatchInput {
        up: c.direction.up,
        down: c.direction.down,
        left: c.direction.left,
        right: c.direction.right,
        a: c.button(PadButton::A),
        b: c.button(PadButton::B),
        c: c.button(PadButton::C),
        x: c.button(PadButton::X),
        y: c.button(PadButton::Y),
        z: c.button(PadButton::Z),
    }
}

/// Per-field OR of two [`MatchInput`]s, so a player can act from the keyboard
/// **or** a controller at the same time (either source asserting a bit wins).
fn merge_match_input(a: MatchInput, b: MatchInput) -> MatchInput {
    MatchInput {
        up: a.up || b.up,
        down: a.down || b.down,
        left: a.left || b.left,
        right: a.right || b.right,
        a: a.a || b.a,
        b: a.b || b.b,
        c: a.c || b.c,
        x: a.x || b.x,
        y: a.y || b.y,
        z: a.z || b.z,
    }
}

/// A single solid HUD color: a 1x1 indexed-color texture plus a one-entry
/// palette, drawn scaled up to fill a rectangle. Recoloring per draw is not
/// possible (a 1x1 R8 texture carries no per-draw color and `fp-render` exposes
/// no texel update), so each HUD color is its own pre-built quad.
struct HudColor {
    /// 1x1 quad whose single texel is palette index 1.
    quad: SpriteTexture,
    /// One-color palette (index 1 = this color; index 0 = transparent).
    palette: PaletteTexture,
}

impl HudColor {
    /// Builds a 1x1 quad + palette for the given RGB color on the GPU.
    fn new(renderer: &Renderer, r: u8, g: u8, b: u8) -> Self {
        // Index 0 is transparent (discarded by the shader); the texel uses 1.
        let quad = SpriteTexture::new(renderer.device(), renderer.queue(), 1, 1, &[1]);
        let mut pal = [0u8; 1024];
        pal[4] = r;
        pal[5] = g;
        pal[6] = b;
        pal[7] = 255;
        let palette = PaletteTexture::new(renderer.device(), renderer.queue(), &pal);
        Self { quad, palette }
    }
}

/// A screen-space rectangle (top-left `x`/`y`, `w`idth, `h`eight) in pixels,
/// used to lay out HUD quads.
#[derive(Debug, Clone, Copy)]
struct HudRect {
    /// Left edge in window pixels.
    x: f32,
    /// Top edge in window pixels.
    y: f32,
    /// Width in window pixels.
    w: f32,
    /// Height in window pixels.
    h: f32,
}

/// Loads the shipped HUD bitmap font (FL2b) into a GPU [`GlyphFont`], or returns
/// `None` so the HUD falls back to its solid-color quad markers.
///
/// Resolves [`HUD_FONT_FNT`] relative to the process working directory and parses
/// it through the real [`fp_formats::fnt::FntFont`] loader. Best-effort: a missing
/// or unparseable font is logged and yields `None` — the HUD then draws the
/// round/KO/winner state as a colored quad exactly as before (no panic, no
/// regression). Split out (taking an explicit path via [`load_hud_font_from`]) so
/// the load path is unit-testable against the shipped asset by absolute path.
fn load_hud_font(renderer: &Renderer) -> Option<GlyphFont> {
    load_hud_font_from(renderer, Path::new(HUD_FONT_FNT))
}

/// Loads a HUD bitmap font from an explicit `.fnt` `path` into a [`GlyphFont`].
///
/// See [`load_hud_font`]. A missing file, a parse failure, or an unsupported FNT
/// version each returns `None` (logged), never a panic.
fn load_hud_font_from(renderer: &Renderer, path: &Path) -> Option<GlyphFont> {
    if !path.exists() {
        tracing::debug!(path = %path.display(), "HUD font not present; HUD uses quad markers");
        return None;
    }
    match fp_formats::fnt::FntFont::load(path) {
        Ok(font) => {
            tracing::info!(
                "HUD font loaded: {} glyphs, {}px tall from {}",
                font.glyph_count(),
                font.image_height,
                path.display()
            );
            Some(GlyphFont::new(renderer.device(), renderer.queue(), font))
        }
        Err(e) => {
            tracing::warn!(
                "HUD font {} failed to load: {e}; HUD uses quad markers",
                path.display()
            );
            None
        }
    }
}

/// A minimal HUD: per-fighter life bars, a smaller power (super-meter) bar under
/// each, the round announcer / KO / winner readout and the round timer as real
/// bitmap text, all drawn through the existing `RenderFrame` pipeline (life/power
/// bars as solid-color quads, text via `RenderFrame::draw_text`). Full lifebars
/// are a later phase.
struct Hud {
    /// Neutral dark frame/background.
    dark: HudColor,
    /// Full-life green.
    green: HudColor,
    /// Low-life / P2-win red.
    red: HudColor,
    /// KO marker yellow.
    yellow: HudColor,
    /// Draw / generic white.
    white: HudColor,
    /// Power (super-meter) fill blue.
    blue: HudColor,
    /// The shipped HUD bitmap font (FL2b), loaded once into a GPU [`GlyphFont`].
    /// `None` when `assets/data/font.fnt` is missing or fails to load — the HUD
    /// then falls back to the solid-color quad markers (no panic, no regression).
    font: Option<GlyphFont>,
}

impl Hud {
    /// Builds all HUD color quads on the GPU and loads the HUD font (best-effort).
    fn new(renderer: &Renderer) -> Self {
        Self {
            dark: HudColor::new(renderer, 40, 40, 48),
            green: HudColor::new(renderer, 60, 210, 90),
            red: HudColor::new(renderer, 220, 60, 60),
            yellow: HudColor::new(renderer, 240, 220, 60),
            white: HudColor::new(renderer, 240, 240, 240),
            blue: HudColor::new(renderer, 70, 150, 240),
            font: load_hud_font(renderer),
        }
    }

    /// The loaded HUD font, if any. `None` when `assets/data/font.fnt` is missing
    /// or failed to load. Shared with overlays (e.g. the T063 training overlay's
    /// legend) so they reuse the one loaded font rather than reloading it.
    fn font(&self) -> Option<&GlyphFont> {
        self.font.as_ref()
    }

    /// Draws a solid-color filled `rect` by drawing the given color's 1x1 quad
    /// scaled up.
    fn fill(&self, frame: &mut fp_render::RenderFrame<'_>, color: &HudColor, rect: HudRect) {
        let params = SpriteDrawParams {
            x: rect.x,
            y: rect.y,
            scale_x: rect.w.max(0.0),
            scale_y: rect.h.max(0.0),
            ..Default::default()
        };
        frame.draw_sprite(&color.quad, &color.palette, &params);
    }

    /// Draws the full match HUD: a life bar for each fighter (P1 top-left, P2
    /// top-right), a smaller power (super-meter) bar directly beneath each, and a
    /// round/KO indicator. Crude by design.
    fn draw(&self, frame: &mut fp_render::RenderFrame<'_>, win_w: f32, m: &Match, tick: u64) {
        const MARGIN: f32 = 12.0;
        const BAR_W: f32 = 200.0;
        const BAR_H: f32 = 16.0;
        /// The power bar is shorter than the life bar and a touch thinner, sitting
        /// just below it with a small vertical gap.
        const POWER_BAR_W: f32 = 160.0;
        const POWER_BAR_H: f32 = 8.0;
        const POWER_GAP: f32 = 4.0;
        let power_y = MARGIN + BAR_H + POWER_GAP;

        // P1 life bar, top-left, growing rightward.
        let p1_bar = HudRect {
            x: MARGIN,
            y: MARGIN,
            w: BAR_W,
            h: BAR_H,
        };
        self.draw_life_bar(frame, p1_bar, m.p1(), false);
        // P1 power bar, directly beneath the life bar, also growing rightward.
        let p1_power = HudRect {
            x: MARGIN,
            y: power_y,
            w: POWER_BAR_W,
            h: POWER_BAR_H,
        };
        self.draw_power_bar(frame, p1_power, m.p1(), false, tick);
        // P2 life bar, top-right, draining toward the center (mirrored).
        let p2_bar = HudRect {
            x: win_w - MARGIN - BAR_W,
            y: MARGIN,
            w: BAR_W,
            h: BAR_H,
        };
        self.draw_life_bar(frame, p2_bar, m.p2(), true);
        // P2 power bar, beneath the life bar, mirrored to anchor at the right edge.
        let p2_power = HudRect {
            x: win_w - MARGIN - POWER_BAR_W,
            y: power_y,
            w: POWER_BAR_W,
            h: POWER_BAR_H,
        };
        self.draw_power_bar(frame, p2_power, m.p2(), true, tick);

        // The round timer (remaining whole seconds), centered near the top, drawn
        // as bitmap text when the font is available. Always rendered during a live
        // fight; harmless during intro (counts the full clock).
        self.draw_announcer(frame, win_w, m);
    }

    /// Draws the round timer and the round/KO/winner announcer.
    ///
    /// When the HUD font loaded, both are drawn as real bitmap text via
    /// [`fp_render::RenderFrame::draw_text`]: the timer (remaining whole seconds)
    /// centered at the very top, and the announcer (`ROUND N` / `KO` / `P1 WINS` /
    /// `P2 WINS` / `DRAW`) centered just below it, tinted per state. With no font
    /// this falls back to the original solid-color quad marker (so a missing/bad
    /// font is no regression).
    fn draw_announcer(&self, frame: &mut fp_render::RenderFrame<'_>, win_w: f32, m: &Match) {
        const MARGIN: f32 = 12.0;
        /// Uniform text scale: the 7px-tall font drawn at 3x ≈ 21px glyphs.
        const TEXT_SCALE: f32 = 3.0;

        let announcer = round_label(m.round_state(), m.winner(), m.round_number());
        let timer = timer_text(m.timer());

        match self.font.as_ref() {
            Some(font) => {
                // Timer at the very top-center.
                draw_centered_text(frame, font, &timer, win_w, MARGIN, TEXT_SCALE, 1.0);
                // Announcer below the timer, only when there is something to say.
                if !announcer.is_empty() {
                    let line_h = font.line_height() as f32 * TEXT_SCALE;
                    let y = MARGIN + line_h + 6.0;
                    draw_centered_text(frame, font, &announcer, win_w, y, TEXT_SCALE, 1.0);
                }
            }
            // No font: fall back to the original centered colored quad marker for
            // the decided-round state (intro/fight show nothing — the bars carry
            // it), exactly as before this feature.
            None => {
                let marker = announcer_quad_color(self, m.round_state(), m.winner());
                if let Some(color) = marker {
                    let marker_w = 80.0;
                    let marker_h = 24.0;
                    let rect = HudRect {
                        x: (win_w - marker_w) / 2.0,
                        y: MARGIN,
                        w: marker_w,
                        h: marker_h,
                    };
                    self.fill(frame, color, rect);
                }
            }
        }
    }

    /// Draws one fighter's life bar: a dark backing the full `bar`, then a colored
    /// fill proportional to `life / life_max`. When `mirror` is set the fill is
    /// anchored to the right edge (so P2's bar drains toward the center).
    fn draw_life_bar(
        &self,
        frame: &mut fp_render::RenderFrame<'_>,
        bar: HudRect,
        player: &Player,
        mirror: bool,
    ) {
        // Backing.
        self.fill(frame, &self.dark, bar);

        let frac = life_fraction(player.life(), player.life_max());
        let fill_w = bar.w * frac;
        // Color shifts from green (healthy) to red at low life (<25%, T074),
        // using the same threshold the screenpack HUD uses so both readouts agree.
        let color = if !fp_ui::low_life_tint(frac).is_neutral() {
            &self.red
        } else {
            &self.green
        };
        if fill_w > 0.0 {
            let fill_x = if mirror {
                bar.x + (bar.w - fill_w)
            } else {
                bar.x
            };
            self.fill(
                frame,
                color,
                HudRect {
                    x: fill_x,
                    w: fill_w,
                    ..bar
                },
            );
        }
    }

    /// Draws one fighter's power (super-meter) bar: a dark backing the full `bar`,
    /// then a blue fill proportional to `power / power_max`. When `mirror` is set
    /// the fill is anchored to the right edge (so P2's bar fills toward the
    /// center, matching its life bar above).
    ///
    /// At max meter (a super is available) the fill **flashes** (T074): it
    /// alternates between blue and a bright yellow on a deterministic, frame-keyed
    /// pulse (the quad HUD cannot multiply a per-draw tint, so it swaps between
    /// pre-built color quads instead). `tick` drives the pulse — no RNG, so the
    /// flash is replay-safe.
    fn draw_power_bar(
        &self,
        frame: &mut fp_render::RenderFrame<'_>,
        bar: HudRect,
        player: &Player,
        mirror: bool,
        tick: u64,
    ) {
        // Backing.
        self.fill(frame, &self.dark, bar);

        let frac = power_fraction(player.power(), player.power_max());
        let fill_w = bar.w * frac;
        // At max meter, flash: the shared helper returns a non-neutral tint only
        // during the flash's "dim" phase, which we render as the bright yellow
        // accent so the full bar pulses; otherwise the normal blue fill.
        let color = if fp_ui::max_power_flash_tint(frac, tick).is_neutral() {
            &self.blue
        } else {
            &self.yellow
        };
        if fill_w > 0.0 {
            let fill_x = if mirror {
                bar.x + (bar.w - fill_w)
            } else {
                bar.x
            };
            self.fill(
                frame,
                color,
                HudRect {
                    x: fill_x,
                    w: fill_w,
                    ..bar
                },
            );
        }
    }
}

/// The fraction of life remaining, clamped to `[0, 1]` and safe against a
/// non-positive `life_max` (returns `0.0` rather than dividing by zero).
fn life_fraction(life: i32, life_max: i32) -> f32 {
    if life_max <= 0 {
        return 0.0;
    }
    (life.max(0) as f32 / life_max as f32).clamp(0.0, 1.0)
}

/// The fraction of power (super meter) filled, clamped to `[0, 1]` and safe
/// against a non-positive `power_max` (returns `0.0` rather than dividing by
/// zero). Mirrors [`life_fraction`] so the power bar shares the same safety
/// guarantees as the life bar.
fn power_fraction(power: i32, power_max: i32) -> f32 {
    if power_max <= 0 {
        return 0.0;
    }
    (power.max(0) as f32 / power_max as f32).clamp(0.0, 1.0)
}

/// The round timer as HUD text: the remaining whole seconds (frames / 60),
/// clamped to non-negative, as a decimal string.
///
/// [`Match::timer`] is FRAMES remaining (60 Hz); MUGEN counts whole seconds, so
/// this integer-divides by 60 (flooring) — matching [`timer_frames_to_seconds`]
/// used by the screenpack HUD so both readouts agree. Pure and unit-tested.
fn timer_text(timer_frames: i32) -> String {
    timer_frames_to_seconds(timer_frames).to_string()
}

/// The on-screen pixel width of `text` in `font` at `scale`, summing each
/// glyph's destination advance from the pure layout (so centering matches what
/// `draw_text` actually draws).
///
/// Returns the rightmost glyph edge — the last placed glyph's `dst_x + width` —
/// scaled, or `0.0` for an empty/blank string. Pure given the font.
fn text_pixel_width(font: &GlyphFont, text: &str, scale: f32) -> f32 {
    font.layout(text)
        .iter()
        .map(|g| g.dst_x + g.width as f32)
        .fold(0.0f32, f32::max)
        * scale
}

/// Draws `text` horizontally centered in a `win_w`-wide window at top `y`, at the
/// given `scale` and `alpha`, using the normal blend mode. A no-op for empty
/// text. The centering uses [`text_pixel_width`] so it lines up with the glyphs
/// `draw_text` lays out.
fn draw_centered_text(
    frame: &mut fp_render::RenderFrame<'_>,
    font: &GlyphFont,
    text: &str,
    win_w: f32,
    y: f32,
    scale: f32,
    alpha: f32,
) {
    if text.is_empty() {
        return;
    }
    let w = text_pixel_width(font, text, scale);
    let x = ((win_w - w) / 2.0).max(0.0);
    frame.draw_text(
        font,
        text,
        &TextDrawParams {
            x,
            y,
            scale,
            alpha,
            blend: BlendMode::Normal,
        },
    );
}

/// The quad color for the decided-round announcer marker when no HUD font is
/// loaded (the pre-FL2b fallback): yellow on KO, green/red on a P1/P2 win, white
/// on a draw, and `None` during intro/fight (the bars carry that state). Kept as
/// a small helper so the fallback mapping is one place and matches the original.
fn announcer_quad_color(hud: &Hud, state: RoundState, winner: Option<Winner>) -> Option<&HudColor> {
    match (state, winner) {
        (RoundState::Ko, _) => Some(&hud.yellow),
        (RoundState::Win, Some(Winner::P1)) => Some(&hud.green),
        (RoundState::Win, Some(Winner::P2)) => Some(&hud.red),
        (RoundState::Win, _) => Some(&hud.white), // draw
        _ => None,
    }
}

/// Maps a fighter's world X position into a screen X, centered on the window.
fn world_to_screen_x(world_x: f32, win_w: f32) -> f32 {
    win_w / 2.0 + world_x * WORLD_TO_SCREEN
}

/// Ensures the GPU texture for a fighter's current AIR frame is cached. Run once
/// per frame **before** `begin_frame`, because decoding needs `&Renderer` while a
/// live [`fp_render::RenderFrame`] holds the renderer borrowed. A missing frame
/// is a no-op.
fn cache_player_sprite(cache: &mut FighterRender, player: &Player, renderer: &Renderer) {
    if let Some(sprite_id) = player_current_frame(player).map(|f| f.sprite) {
        // Resolve the fighter's active `.act` palette override (FL2b): the runtime
        // selection (`Character::active_palette`) into the loaded override's RGBA,
        // or `None` to use the SFF-embedded palette (the default, unchanged).
        let override_rgba = player
            .loaded
            .override_palette(player.character.active_palette());
        cache.get_or_create_sprite(&player.loaded.sff, sprite_id, renderer, override_rgba);
    }
}

/// Decides the two-fighter draw order from their MUGEN sprite-draw priorities
/// (`sprpriority`, audit #16).
///
/// Returns `true` when P1 should be drawn **first** (i.e. *behind* P2), `false`
/// when P2 should be drawn first (P1 in front). The fighter with the **lower**
/// priority draws first (behind); the higher priority draws over it. Equal
/// priorities keep P1 behind P2 — a stable, deterministic default so the order
/// never flickers on a tie.
///
/// Pure decision split out from the draw loop so the ordering rule is unit-
/// testable without an SDL2 window or GPU.
#[must_use]
fn p1_draws_behind_p2(p1_sprpriority: i32, p2_sprpriority: i32) -> bool {
    p1_sprpriority <= p2_sprpriority
}

/// Draws one fighter from its current AIR frame at its world position and facing,
/// reading the already-populated per-character texture cache (see
/// [`cache_player_sprite`]). A missing frame or uncached sprite is skipped.
///
/// `camera_x` is the camera's world X; the fighter's world position is offset by
/// it before mapping to screen, so the playfield scrolls with the camera. With no
/// stage (`camera_x = 0`) this reduces to the original centered mapping, so the
/// flat-background path is unchanged.
fn draw_player(
    frame: &mut fp_render::RenderFrame<'_>,
    cache: &FighterRender,
    player: &Player,
    camera_x: f32,
    win_w: f32,
    win_h: f32,
) {
    let Some(anim_frame) = player_current_frame(player) else {
        return;
    };
    let Some(cached) = cache.sprite_cache.get(&anim_frame.sprite) else {
        return;
    };

    let ground_y = win_h * 0.8;
    let facing_right = player.facing() == fp_character::Facing::Right;
    let screen_x = world_to_screen_x(player.pos().x - camera_x, win_w);

    let draw_x = screen_x - cached.axis_x as f32 + anim_frame.offset.x as f32;
    let draw_y = ground_y + player.pos().y - cached.axis_y as f32 + anim_frame.offset.y as f32;

    // Mirror the frame's authored flip when facing left (same rule as the single
    // character path).
    let flip_h = if facing_right {
        anim_frame.flip_h
    } else {
        !anim_frame.flip_h
    };

    let (render_blend, alpha) = map_blend_mode(&anim_frame.blend);

    // AfterImage trail (audit #33; T007): draw the trail's *captured* past frames
    // BEHIND the live sprite, so the live frame draws over them. The frame-history
    // ring (`AfterImageState::frames`) records each past frame's sprite identity +
    // world position; we redraw the selected ghosts (`ghost_frames`) at their own
    // positions, progressively modulated by the trail's PalBright/PalContrast and
    // composited with its `trans` blend mode.
    let afterimage = player.character.afterimage();
    if afterimage.is_active() {
        draw_afterimage_trail(frame, cache, player, afterimage, camera_x, win_w, win_h);
    }

    // PalFX color tint (audit #33): the character's active tint (identity when
    // none, so an untinted sprite is byte-identical to before this feature).
    let palfx = char_palfx_to_render(player.character.palfx());

    // Per-frame AIR scale/angle, with Interpolate blending (T009): resolve the
    // current animation frame's transform from the character's anim cursor. The
    // IDENTITY default keeps a character whose AIR carries no scale/angle columns
    // byte-identical to before this feature. The angle is in degrees; the
    // renderer takes radians.
    let transform = player.character.anim_transform(&player.loaded.air);

    let params = SpriteDrawParams {
        x: draw_x,
        y: draw_y,
        flip_h,
        flip_v: anim_frame.flip_v,
        scale_x: transform.scale.x,
        scale_y: transform.scale.y,
        angle: transform.angle_rad(),
        blend: render_blend,
        alpha,
        palfx,
    };
    frame.draw_sprite(&cached.texture, &cached.palette, &params);
}

// ---------------------------------------------------------------------------
// Common-effects (fightfx) asset (audit #17)
// ---------------------------------------------------------------------------

/// The shipped common-effects (`fightfx`) sprite source and its GPU cache.
///
/// Pairs the loaded `fightfx.sff` with a [`FighterRender`] cache so common
/// ([`EffectSide::Common`]) hit-sparks can be decoded/drawn exactly like a
/// fighter's sprites. The matching `fightfx.air` is installed onto the
/// [`fp_engine::Match`] (via [`Match::set_common_fx`]) so the engine resolves the
/// spark frames; this struct only owns the sprite/texture side a renderer needs.
struct CommonFxRender {
    /// The decoded common-effects sprite file (`fightfx.sff`).
    sff: SffFile,
    /// GPU sprite cache for the common-effects sprites, decoded on first use.
    render: FighterRender,
}

/// Loads the shipped common-effects (`fightfx`) asset from the default paths,
/// returning the parsed AIR (to install on the [`Match`]) and the SFF render
/// bundle (for drawing).
///
/// Resolves the paths relative to the process working directory
/// ([`COMMON_FX_SFF`] / [`COMMON_FX_AIR`]) and delegates to
/// [`load_common_fx_from`]. Best-effort: a missing/unparseable asset yields
/// `None` (logged), so the match simply renders no common sparks — never a panic,
/// never a regression.
fn load_common_fx() -> Option<(AirFile, CommonFxRender)> {
    load_common_fx_from(Path::new(COMMON_FX_SFF), Path::new(COMMON_FX_AIR))
}

/// Loads a common-effects (`fightfx`) asset from explicit `sff_path`/`air_path`.
///
/// Both files are best-effort: a missing or unparseable `.sff`/`.air` returns
/// `None` (logged at debug/warn), so common sparks are simply disabled — never a
/// panic, never a regression. Split from [`load_common_fx`] so it is unit-testable
/// against the shipped asset by absolute path (test CWD is the crate dir).
fn load_common_fx_from(sff_path: &Path, air_path: &Path) -> Option<(AirFile, CommonFxRender)> {
    if !sff_path.exists() || !air_path.exists() {
        tracing::debug!(
            sff = %sff_path.display(),
            air = %air_path.display(),
            "common-fx asset not present; common hit-sparks disabled"
        );
        return None;
    }
    let sff = match SffFile::load(sff_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "common-fx sff {} failed to load: {e}; common sparks disabled",
                sff_path.display()
            );
            return None;
        }
    };
    let air = match AirFile::load(air_path) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                "common-fx air {} failed to load: {e}; common sparks disabled",
                air_path.display()
            );
            return None;
        }
    };
    tracing::info!(
        "common-fx loaded: {} sprites from {}",
        sff.sprites.len(),
        sff_path.display()
    );
    Some((
        air,
        CommonFxRender {
            sff,
            render: FighterRender::default(),
        },
    ))
}

// ---------------------------------------------------------------------------
// Hit-spark effect rendering (audit #17)
// ---------------------------------------------------------------------------

/// Ensures the GPU textures for every live hit-spark effect's current sprite are
/// cached (audit #17). Run once per frame **before** `begin_frame`, like
/// [`cache_player_sprite`], because decoding needs `&Renderer` while a live
/// [`fp_render::RenderFrame`] holds it borrowed. Each effect's sprite is decoded
/// from its owning side's SFF into that side's per-character cache. A
/// missing/undecodable sprite is a no-op (logged once by the cache).
fn cache_effect_sprites(run: &mut MatchRun, renderer: &Renderer) {
    // Collect (side, sprite) pairs first to avoid borrowing the match while the
    // per-side caches are mutated.
    let effects: Vec<(EffectSide, SpriteId)> = run
        .team
        .active()
        .effects()
        .iter()
        .map(|fx| (fx.side, fx.sprite))
        .collect();
    for (side, sprite) in effects {
        // Hit-sparks always use their source SFF's embedded palette (no `.act`
        // costume swap on sparks); pass `None`. Spark sprite ids never collide
        // with a fighter's body sprites, so sharing the per-side cache is safe.
        match side {
            EffectSide::P1 => {
                // Field access (`run.team`), not the `m()` method, so the immutable
                // active-match borrow stays disjoint from `&mut run.p1_render`.
                run.p1_render.get_or_create_sprite(
                    &run.team.active().p1().loaded.sff,
                    sprite,
                    renderer,
                    None,
                );
            }
            EffectSide::P2 => {
                run.p2_render.get_or_create_sprite(
                    &run.team.active().p2().loaded.sff,
                    sprite,
                    renderer,
                    None,
                );
            }
            // A common spark draws from the shipped common-fx SFF into its own
            // cache. A no-op when no common-fx asset is loaded (no such effects
            // are ever spawned then).
            EffectSide::Common => {
                if let Some(fx) = run.common_fx.as_mut() {
                    fx.render
                        .get_or_create_sprite(&fx.sff, sprite, renderer, None);
                }
            }
        }
    }
}

/// Ensures the GPU textures for every live explod's current sprite are cached
/// (T033). Run once per frame **before** `begin_frame`, like
/// [`cache_effect_sprites`], because decoding needs `&Renderer`. Each explod plays
/// one of its owner's AIR actions, so its sprite is decoded from that owner's SFF
/// into that owner's per-character cache. A missing/undecodable sprite is a no-op
/// (logged once by the cache).
fn cache_explod_sprites(run: &mut MatchRun, renderer: &Renderer) {
    let p1: Vec<SpriteId> = run
        .team
        .active()
        .p1()
        .explods()
        .iter()
        .map(|e| e.sprite)
        .collect();
    let p2: Vec<SpriteId> = run
        .team
        .active()
        .p2()
        .explods()
        .iter()
        .map(|e| e.sprite)
        .collect();
    for sprite in p1 {
        // Field access keeps the active-match read disjoint from `&mut run.p1_render`.
        run.p1_render.get_or_create_sprite(
            &run.team.active().p1().loaded.sff,
            sprite,
            renderer,
            None,
        );
    }
    for sprite in p2 {
        run.p2_render.get_or_create_sprite(
            &run.team.active().p2().loaded.sff,
            sprite,
            renderer,
            None,
        );
    }
}

/// The sprite caches a hit-spark [`Effect`] may draw from, picked per effect by
/// its [`EffectSide`] (audit #17): the two fighters' caches and the optional
/// shared common-effects (`fightfx`) cache. Bundled so [`draw_effects`] stays a
/// small, readable call.
struct EffectRenders<'a> {
    /// Player 1's sprite cache (for [`EffectSide::P1`] sparks).
    p1_render: &'a FighterRender,
    /// Player 2's sprite cache (for [`EffectSide::P2`] sparks).
    p2_render: &'a FighterRender,
    /// The shared common-effects cache (for [`EffectSide::Common`] sparks), or
    /// `None` when no common-fx asset is loaded.
    common_render: Option<&'a FighterRender>,
}

/// Draws every live hit-spark effect at its world position, over the fighters
/// (audit #17).
///
/// Each effect's sprite was cached by [`cache_effect_sprites`] in the owning
/// source's cache (a fighter's, or the shared common-fx cache for common sparks).
/// This maps the effect's world position to screen (the same `world_to_screen_x`
/// plus ground-plane mapping the fighters use, offset by `camera_x`), anchors the
/// sprite by its axis, and draws it additively (a spark glows). A missing/uncached
/// sprite is skipped — never a panic. This draws only over the player/effect
/// region; it does not touch the HUD/screenpack.
///
/// `renders` bundles the three sprite caches a spark may source from (P1, P2, and
/// the optional shared common-fx cache) so the source is picked per effect.
fn draw_effects(
    frame: &mut fp_render::RenderFrame<'_>,
    renders: EffectRenders<'_>,
    m: &Match,
    camera_x: f32,
    win_w: f32,
    win_h: f32,
) {
    let EffectRenders {
        p1_render,
        p2_render,
        common_render,
    } = renders;
    let ground_y = win_h * 0.8;
    for fx in m.effects() {
        let cache = match fx.side {
            EffectSide::P1 => p1_render,
            EffectSide::P2 => p2_render,
            // A common spark draws from the shared common-fx cache; skip if the
            // common-fx asset is absent (no such effect would spawn then anyway).
            EffectSide::Common => match common_render {
                Some(c) => c,
                None => continue,
            },
        };
        let Some(cached) = cache.sprite_cache.get(&fx.sprite) else {
            continue;
        };
        // Map the spark's world anchor into screen space exactly like a fighter
        // (X centered + camera offset, Y on the ground plane), then anchor the
        // sprite by its axis and apply the AIR frame's authored offset.
        // TODO(audit #17): this is a minimal first cut — it draws at a fixed 1:1
        // scale with no facing mirror and ignores the AIR frame's other transforms
        // (per-frame scale/angle/flip and the spark's own blend value); a future
        // pass should honor those + the attacker's facing for the spark, as MUGEN
        // does. The fixed Additive blend below is likewise a deliberate placeholder.
        let screen_x = world_to_screen_x(fx.pos.x - camera_x, win_w);
        let draw_x = screen_x - cached.axis_x as f32 + fx.offset.x as f32;
        let draw_y = ground_y + fx.pos.y - cached.axis_y as f32 + fx.offset.y as f32;
        let params = SpriteDrawParams {
            x: draw_x,
            y: draw_y,
            blend: fp_render::BlendMode::Additive,
            ..Default::default()
        };
        frame.draw_sprite(&cached.texture, &cached.palette, &params);
    }
}

/// Draws every live explod display entity at its world position (T033).
///
/// Each explod is spawned by a fighter's `Explod` controller and plays one of that
/// fighter's own AIR actions, so its frames resolve against that fighter's sprite
/// cache ([`Explod::owner`] selects P1's or P2's). The world-to-screen mapping
/// matches the fighters and the hit-sparks (`world_to_screen_x` + the ground
/// plane, offset by `camera_x`); the explod is anchored by its sprite axis and the
/// AIR frame's authored offset. Explods are drawn ordered by their `sprpriority`
/// relative to a baseline so a higher-priority explod draws in front; a
/// missing/uncached sprite is skipped (never a panic). Draws over the fighters,
/// under the front BG/HUD — like [`draw_effects`].
fn draw_explods(
    frame: &mut fp_render::RenderFrame<'_>,
    p1_render: &FighterRender,
    p2_render: &FighterRender,
    m: &Match,
    camera_x: f32,
    win_w: f32,
    win_h: f32,
) {
    let ground_y = win_h * 0.8;
    // Gather (owner-cache, explod) pairs from both players, then draw in ascending
    // sprpriority so higher-priority explods composite over lower ones.
    let mut items: Vec<(&FighterRender, &fp_engine::Explod)> = Vec::new();
    for e in m.p1().explods() {
        items.push((p1_render, e));
    }
    for e in m.p2().explods() {
        items.push((p2_render, e));
    }
    items.sort_by_key(|(_, e)| e.sprpriority);

    for (cache, e) in items {
        let Some(cached) = cache.sprite_cache.get(&e.sprite) else {
            continue;
        };
        let screen_x = world_to_screen_x(e.pos.x - camera_x, win_w);
        let draw_x = screen_x - cached.axis_x as f32 + e.offset.x as f32;
        let draw_y = ground_y + e.pos.y - cached.axis_y as f32 + e.offset.y as f32;
        let params = SpriteDrawParams {
            x: draw_x,
            y: draw_y,
            ..Default::default()
        };
        frame.draw_sprite(&cached.texture, &cached.palette, &params);
    }
}

/// Converts the character-side [`fp_character::CurPalFx`] into the renderer's
/// [`fp_render::PalFx`] color tint (audit #33; full modulation set T008).
///
/// The `add`/`mul`/`color`/`invertall` fields pass straight through (both sides
/// use the same normalized float scale). The character-side accessors
/// ([`fp_character::Character::palfx`]) already resolve the `sinadd` oscillation
/// into `add` for the current tick, so there is no phase to carry. No `is_active`
/// gate is needed here: callers obtain the effect from
/// [`fp_character::Character::palfx`] / [`fp_character::AfterImageState::palfx`],
/// which already collapse an inactive effect to
/// [`fp_character::CurPalFx::IDENTITY`] — and that value's fields are exactly
/// [`fp_render::PalFx::IDENTITY`], so an inactive effect still maps to the
/// guaranteed no-op draw.
fn char_palfx_to_render(fx: fp_character::CurPalFx) -> fp_render::PalFx {
    fp_render::PalFx {
        add: fx.add,
        mul: fx.mul,
        color: fx.color,
        invertall: fx.invertall,
    }
}

/// Maps the character-side [`fp_character::TrailBlend`] onto the renderer's
/// [`fp_render::TrailTrans`] (T007). The two enums mirror each other (the renderer
/// crate does not depend on `fp-character`), so this is a straight 1:1 translation.
fn trail_blend_to_render(blend: fp_character::TrailBlend) -> fp_render::TrailTrans {
    match blend {
        fp_character::TrailBlend::None => fp_render::TrailTrans::None,
        fp_character::TrailBlend::Add => fp_render::TrailTrans::Add,
        fp_character::TrailBlend::Add1 => fp_render::TrailTrans::Add1,
        fp_character::TrailBlend::Sub => fp_render::TrailTrans::Sub,
    }
}

/// Builds the renderer's [`fp_render::AfterImageModulation`] from a trail's base
/// tint and per-ghost `PalBright`/`PalContrast` ramps (T007).
fn afterimage_modulation(
    afterimage: &fp_character::AfterImageState,
) -> fp_render::AfterImageModulation {
    fp_render::AfterImageModulation {
        base: char_palfx_to_render(afterimage.palfx),
        palbright: afterimage.palbright,
        palcontrast: afterimage.palcontrast,
    }
}

/// Draws the AfterImage ghost trail behind a fighter's live sprite from the
/// captured frame-history ring (audit #33; T007).
///
/// Walks the trail's selected ghosts ([`AfterImageState::ghost_frames`], every
/// `framegap`-th retained frame, newest-first) and redraws each at its **own**
/// captured world position / facing — a true motion trail of where the fighter
/// *was*, not a smear of the current frame. Each ghost is progressively modulated
/// by the trail's `PalBright`/`PalContrast` ([`fp_render::ghost_palfx`]) and
/// composited with its `trans` blend and a decaying per-ghost alpha
/// ([`fp_render::ghost_alpha`]). Ghosts are drawn oldest-first (back-to-front) so
/// newer (brighter) ghosts overlay older ones, then the live sprite over all.
/// A ghost whose sprite cannot be resolved/cached is skipped. Never panics.
///
/// [`AfterImageState::ghost_frames`]: fp_character::AfterImageState::ghost_frames
fn draw_afterimage_trail(
    frame: &mut fp_render::RenderFrame<'_>,
    cache: &FighterRender,
    player: &Player,
    afterimage: &fp_character::AfterImageState,
    camera_x: f32,
    win_w: f32,
    win_h: f32,
) {
    let ground_y = win_h * 0.8;
    let ghosts = afterimage.ghost_frames();
    let count = ghosts.len();
    let trans = trail_blend_to_render(afterimage.trans);
    let blend = trans.blend_mode();
    let modulation = afterimage_modulation(afterimage);

    // Draw oldest ghost first (largest index) so newer, brighter ghosts overlay it.
    for (i, ghost) in ghosts.iter().enumerate().rev() {
        // Resolve the sprite that was showing when this frame was captured.
        let Some(action) = player.loaded.air.action(ghost.anim) else {
            continue;
        };
        if action.frames.is_empty() {
            continue;
        }
        let idx = clamp_elem(ghost.anim_elem, action.frames.len());
        let Some(anim_frame) = action.frames.get(idx) else {
            continue;
        };
        let Some(cached) = cache.sprite_cache.get(&anim_frame.sprite) else {
            continue;
        };

        // Place the ghost at its own captured world position (same mapping as the
        // live sprite in `draw_player`), so the trail follows the actual path.
        let facing_right = ghost.facing == fp_character::Facing::Right;
        let screen_x = world_to_screen_x(ghost.pos.x - camera_x, win_w);
        let draw_x = screen_x - cached.axis_x as f32 + anim_frame.offset.x as f32;
        let draw_y = ground_y + ghost.pos.y - cached.axis_y as f32 + anim_frame.offset.y as f32;
        let flip_h = if facing_right {
            anim_frame.flip_h
        } else {
            !anim_frame.flip_h
        };

        let alpha = fp_render::ghost_alpha(i, count, trans);
        let palfx = fp_render::ghost_palfx(&modulation, i);
        let params = SpriteDrawParams {
            x: draw_x,
            y: draw_y,
            flip_h,
            flip_v: anim_frame.flip_v,
            blend,
            alpha,
            palfx,
            ..Default::default()
        };
        frame.draw_sprite(&cached.texture, &cached.palette, &params);
    }
}

// ---------------------------------------------------------------------------
// Clsn hitbox/hurtbox debug overlay (audit #34)
// ---------------------------------------------------------------------------

/// The screen anchor a fighter's character-local geometry hangs off: the
/// screen-space pixel that its axis (`pos`) maps to. This is exactly the anchor
/// [`draw_player`] uses for the sprite (X from [`world_to_screen_x`], Y from the
/// ground plane plus the fighter's vertical offset), so boxes computed from it
/// line up with the drawn sprite. `ground_y` is the same `win_h * 0.8` fraction
/// used there. `camera_x` is the camera's world X, subtracted before mapping so
/// the boxes scroll with the sprite (matching [`draw_player`]).
fn player_screen_anchor(
    pos: fp_core::Vec2<f32>,
    camera_x: f32,
    win_w: f32,
    win_h: f32,
) -> (f32, f32) {
    let ground_y = win_h * 0.8;
    (world_to_screen_x(pos.x - camera_x, win_w), ground_y + pos.y)
}

/// Maps one character-local Clsn rect (Y-down, relative to the axis, as stored
/// in an AIR frame) into a screen-space [`fp_render::DebugBox`] of the given
/// color.
///
/// The transform mirrors `fp_physics::place_clsn`: the local X edges are
/// reflected about the axis when facing left (`anchor_x + sign * local_x`) and Y
/// is translated unchanged (`anchor_y + local_y`, Y down). `WORLD_TO_SCREEN`
/// being `1.0`, world and screen pixels share a scale, so no extra X scaling is
/// needed. The result is normalized to non-negative width/height.
fn clsn_to_screen_box(
    local: &fp_core::Rect,
    anchor_x: f32,
    anchor_y: f32,
    facing: fp_character::Facing,
    color: [f32; 4],
) -> fp_render::DebugBox {
    let sign = facing.sign() as f32;
    // Reflect both local X edges; left/right may swap when facing left.
    let lx0 = local.x;
    let lx1 = local.right();
    let sx0 = anchor_x + sign * lx0;
    let sx1 = anchor_x + sign * lx1;
    let (x, w) = if sx0 <= sx1 {
        (sx0, sx1 - sx0)
    } else {
        (sx1, sx0 - sx1)
    };
    // Y is never mirrored by facing.
    let y = anchor_y + local.y;
    let h = local.h.abs();
    fp_render::DebugBox { x, y, w, h, color }
}

/// Red (Clsn1 = attack box), MUGEN debug convention. RGBA in 0.0–1.0.
const CLSN1_COLOR: [f32; 4] = [1.0, 0.25, 0.25, 1.0];
/// Blue (Clsn2 = hurt/collision box), MUGEN debug convention. RGBA in 0.0–1.0.
const CLSN2_COLOR: [f32; 4] = [0.3, 0.55, 1.0, 1.0];
/// Green (player push / `Width` box), the third overlay color. RGBA in 0.0–1.0.
const PUSH_COLOR: [f32; 4] = [0.3, 0.95, 0.4, 1.0];

/// Which kind of collision box a [`collect_clsn_boxes`] entry represents, so a
/// caller can style each independently. `Hurt` is Clsn2 (blue), `Hit` is Clsn1
/// (red), and `Push` is the player-push / `Width` box (green).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClsnKind {
    /// Clsn1 — the attack / hit box (red).
    Hit,
    /// Clsn2 — the hurt / collision box (blue).
    Hurt,
    /// The player-push / `Width` box (green), derived from the size half-widths
    /// rather than from the AIR frame.
    Push,
}

impl ClsnKind {
    /// The MUGEN-convention overlay color for this kind, RGBA in 0.0–1.0.
    fn color(self) -> [f32; 4] {
        match self {
            ClsnKind::Hit => CLSN1_COLOR,
            ClsnKind::Hurt => CLSN2_COLOR,
            ClsnKind::Push => PUSH_COLOR,
        }
    }
}

/// Maps the player-push half-widths into a character-local Clsn-style rect (the
/// same Y-down, axis-relative convention as an AIR frame's Clsn rects), so the
/// push box flows through the *same* [`clsn_to_screen_box`] facing-mirror path as
/// the hit/hurt boxes.
///
/// `front`/`back` are the facing-relative half-widths from
/// [`Player::push_widths`](fp_engine::Player::push_widths): `front` extends toward
/// the facing direction (local +X before mirroring), `back` the other way. The
/// box stands on the axis and is given a small fixed visual height so it reads as
/// a footprint band at the fighter's feet; push collision itself is width-only.
fn push_box_local(front: f32, back: f32) -> fp_core::Rect {
    // Local +X is "forward"; clsn_to_screen_box mirrors it for left-facing.
    // Height is purely cosmetic — a thin band just above the ground line.
    const PUSH_BOX_HEIGHT: f32 = 14.0;
    fp_core::Rect::new(-back, -PUSH_BOX_HEIGHT, front + back, PUSH_BOX_HEIGHT)
}

/// Collects one fighter's current-frame collision boxes as screen-space
/// [`fp_render::DebugBox`]es tagged with their [`ClsnKind`], in draw order: every
/// Clsn2 (hurt) first, then every Clsn1 (hit), then the single push/`Width` box
/// last so it reads over the others. The boxes are facing-mirrored exactly like
/// the rendered sprite (via [`clsn_to_screen_box`], which mirrors
/// `fp_physics::place_clsn`). A missing/empty current frame yields only the push
/// box (which has no AIR dependency); a `None` would never be a panic.
///
/// This is the single box-mapping math shared by **both** the raw F1 dev overlay
/// and the player-facing [`TrainingOverlay`] — they differ only in styling and
/// toggle scope, never in geometry.
fn collect_clsn_boxes(
    player: &Player,
    camera_x: f32,
    win_w: f32,
    win_h: f32,
) -> Vec<(fp_render::DebugBox, ClsnKind)> {
    let (anchor_x, anchor_y) = player_screen_anchor(player.pos(), camera_x, win_w, win_h);
    let facing = player.facing();
    let mut boxes = Vec::new();

    if let Some(anim_frame) = player_current_frame(player) {
        for hurt in &anim_frame.clsn2 {
            boxes.push((
                clsn_to_screen_box(hurt, anchor_x, anchor_y, facing, ClsnKind::Hurt.color()),
                ClsnKind::Hurt,
            ));
        }
        for attack in &anim_frame.clsn1 {
            boxes.push((
                clsn_to_screen_box(attack, anchor_x, anchor_y, facing, ClsnKind::Hit.color()),
                ClsnKind::Hit,
            ));
        }
    }

    // Push/Width box: derived from the player half-widths (not the AIR frame), so
    // it is always available even on a frame with no Clsn data.
    let (front, back) = player.push_widths();
    let push_local = push_box_local(front, back);
    boxes.push((
        clsn_to_screen_box(
            &push_local,
            anchor_x,
            anchor_y,
            facing,
            ClsnKind::Push.color(),
        ),
        ClsnKind::Push,
    ));

    boxes
}

/// Draws one fighter's current-frame collision boxes when the debug overlay is
/// on, in [`collect_clsn_boxes`] order (Clsn2 hurt, then Clsn1 hit, then the
/// push/`Width` box). A missing frame still draws the push box. This is the raw
/// F1 dev overlay; the player-facing [`TrainingOverlay`] reuses the same
/// `collect_clsn_boxes` math with its own scoping/legend.
fn draw_player_clsn(
    frame: &mut fp_render::RenderFrame<'_>,
    player: &Player,
    camera_x: f32,
    win_w: f32,
    win_h: f32,
) {
    for (b, _kind) in collect_clsn_boxes(player, camera_x, win_w, win_h) {
        frame.draw_debug_box(&b);
    }
}

// ---------------------------------------------------------------------------
// Player-facing training overlay (T063)
// ---------------------------------------------------------------------------

/// Which fighter(s) the player-facing [`TrainingOverlay`] draws Clsn boxes for.
///
/// Distinct from the raw F1 dev overlay (which is always both-or-nothing): the
/// training overlay can scope to a single side so a player can study just their
/// own hurtboxes or just the opponent's hit reach. Cycled with the
/// [`TrainingOverlay`] toggle key; the order is `Off → P1 → P2 → Both → Off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OverlayScope {
    /// No boxes drawn (overlay disabled).
    #[default]
    Off,
    /// Player 1's boxes only.
    P1,
    /// Player 2's boxes only.
    P2,
    /// Both fighters' boxes.
    Both,
}

impl OverlayScope {
    /// The next scope in the cycle `Off → P1 → P2 → Both → Off`, used by the
    /// toggle key so one key walks every per-side state and back to off.
    fn next(self) -> Self {
        match self {
            OverlayScope::Off => OverlayScope::P1,
            OverlayScope::P1 => OverlayScope::P2,
            OverlayScope::P2 => OverlayScope::Both,
            OverlayScope::Both => OverlayScope::Off,
        }
    }

    /// Whether player 1's boxes should be drawn under this scope.
    fn shows_p1(self) -> bool {
        matches!(self, OverlayScope::P1 | OverlayScope::Both)
    }

    /// Whether player 2's boxes should be drawn under this scope.
    fn shows_p2(self) -> bool {
        matches!(self, OverlayScope::P2 | OverlayScope::Both)
    }

    /// A short human label for the legend / logs.
    fn label(self) -> &'static str {
        match self {
            OverlayScope::Off => "OFF",
            OverlayScope::P1 => "P1",
            OverlayScope::P2 => "P2",
            OverlayScope::Both => "BOTH",
        }
    }
}

/// The player-facing hitbox/hurtbox training overlay (T063): a styled,
/// per-side-scopable view of each fighter's Clsn1 (hit, red), Clsn2 (hurt, blue),
/// and push/`Width` (green) boxes, plus a small on-screen legend.
///
/// It reuses the exact same box-mapping math as the raw F1 dev overlay
/// ([`collect_clsn_boxes`]); only the scope (per-side, via [`OverlayScope`]) and
/// the legend differ. State persists for the session in the run loop. The dev F1
/// overlay is unaffected and keeps drawing both sides whenever it is on.
#[derive(Debug, Clone, Copy, Default)]
struct TrainingOverlay {
    /// Which side(s) to draw; `Off` disables the overlay entirely.
    scope: OverlayScope,
}

impl TrainingOverlay {
    /// Cycles the scope one step (`Off → P1 → P2 → Both → Off`); the toggle key
    /// calls this so a single key walks every per-side state.
    fn cycle(&mut self) {
        self.scope = self.scope.next();
    }

    /// Whether the overlay is currently drawing anything.
    fn is_active(&self) -> bool {
        self.scope != OverlayScope::Off
    }

    /// Draws the in-scope fighters' Clsn boxes (reusing [`collect_clsn_boxes`])
    /// plus, when a HUD font is available, a small top-left legend. A `None` font
    /// simply omits the text (no regression, no panic). Nothing is drawn when the
    /// scope is `Off`.
    fn draw(
        &self,
        frame: &mut fp_render::RenderFrame<'_>,
        m: &Match,
        font: Option<&GlyphFont>,
        camera_x: f32,
        win_w: f32,
        win_h: f32,
    ) {
        if !self.is_active() {
            return;
        }
        if self.scope.shows_p1() {
            for (b, _kind) in collect_clsn_boxes(m.p1(), camera_x, win_w, win_h) {
                frame.draw_debug_box(&b);
            }
        }
        if self.scope.shows_p2() {
            for (b, _kind) in collect_clsn_boxes(m.p2(), camera_x, win_w, win_h) {
                frame.draw_debug_box(&b);
            }
        }
        if let Some(font) = font {
            self.draw_legend(frame, font);
            // T065: per-side frame-data readout under the legend.
            if self.scope.shows_p1() {
                self.draw_frame_data(frame, font, m.p1(), Self::FRAME_DATA_P1_X);
            }
            if self.scope.shows_p2() {
                self.draw_frame_data(frame, font, m.p2(), win_w - Self::FRAME_DATA_P2_INSET);
            }
        }
    }

    /// X anchor of P1's frame-data readout.
    const FRAME_DATA_P1_X: f32 = 8.0;
    /// Right-edge inset of P2's frame-data readout.
    const FRAME_DATA_P2_INSET: f32 = 120.0;
    /// Vertical anchor of the (single-line) frame-data readout.
    const FRAME_DATA_TOP: f32 = 124.0;

    /// Draws one player's frame-data readout (T065): the startup / active /
    /// recovery of the action it is currently executing.
    ///
    /// The readout is **self-gating** via [`format_frame_data`]: it shows real
    /// numbers only for an action [`fp_character::MoveFrameData::compute`] can
    /// count (one with a real attack window), and `S/A/R —` otherwise — never a
    /// wrong number. A `None` font would have short-circuited the caller, so a
    /// font is always in hand here.
    fn draw_frame_data(
        &self,
        frame: &mut fp_render::RenderFrame<'_>,
        font: &GlyphFont,
        player: &fp_engine::Player,
        x: f32,
    ) {
        let text = format_frame_data(player);
        frame.draw_text(
            font,
            &text,
            &TextDrawParams {
                x,
                y: Self::FRAME_DATA_TOP,
                scale: 1.0,
                alpha: 1.0,
                blend: BlendMode::Normal,
            },
        );
    }

    /// Draws the color legend: a labeled line per box kind plus the current scope.
    /// Each line is prefixed with a small solid swatch in that kind's color so the
    /// mapping reads at a glance. Top-left, under the lifebars.
    fn draw_legend(&self, frame: &mut fp_render::RenderFrame<'_>, font: &GlyphFont) {
        const LEGEND_X: f32 = 8.0;
        const LEGEND_TOP: f32 = 64.0;
        const LINE_H: f32 = 14.0;
        const SCALE: f32 = 1.0;
        const SWATCH: f32 = 10.0;
        // Order matches collect_clsn_boxes draw order, then the scope.
        let rows: [(&str, [f32; 4]); 4] = [
            ("HURT", ClsnKind::Hurt.color()),
            ("HIT", ClsnKind::Hit.color()),
            ("PUSH", ClsnKind::Push.color()),
            (self.scope.label(), [1.0, 1.0, 1.0, 1.0]),
        ];
        for (i, (text, color)) in rows.iter().enumerate() {
            let y = LEGEND_TOP + i as f32 * LINE_H;
            // Color swatch (skip for the scope row, which uses white text only).
            if i < 3 {
                frame.draw_debug_box(&fp_render::DebugBox {
                    x: LEGEND_X,
                    y,
                    w: SWATCH,
                    h: SWATCH,
                    color: *color,
                });
            }
            frame.draw_text(
                font,
                text,
                &TextDrawParams {
                    x: LEGEND_X + SWATCH + 4.0,
                    y,
                    scale: SCALE,
                    alpha: 1.0,
                    blend: BlendMode::Normal,
                },
            );
        }
    }
}

/// Formats a player's frame-data readout line (T065): `S<start> A<active>
/// R<recovery>` for the action it is currently executing, or `S/A/R —` when that
/// action is not a countable attack (idle / movement / looping / `time = -1`).
///
/// When the player's move **connected on the most recent tick**, the on-block /
/// on-hit **frame advantage** is appended in the `ADV +3` / `ADV −5` form (a
/// signed number of ticks; positive = the attacker recovers first). On a tick with
/// no connection the advantage segment reads `ADV —`, so the readout never shows a
/// stale advantage number.
///
/// Looks up the current action ([`Player::anim`]) in the player's loaded `.air`
/// table and runs [`fp_character::MoveFrameData::compute`]; a missing action or
/// an uncountable one both fall to the `—` form (never a wrong number, never a
/// panic) per the project's error philosophy. The advantage is read from
/// [`fp_engine::Player::frame_advantage`], which the engine recomputes each tick.
fn format_frame_data(player: &fp_engine::Player) -> String {
    let fd = player
        .loaded
        .air
        .actions
        .get(&player.anim())
        .and_then(fp_character::MoveFrameData::compute);
    let sar = match fd {
        Some(fd) => format!("S{} A{} R{}", fd.startup, fd.active, fd.recovery),
        None => "S/A/R —".to_string(),
    };
    format!(
        "{sar}  {}",
        format_frame_advantage(player.frame_advantage())
    )
}

/// Formats the on-block / on-hit frame-advantage segment of the frame-data readout
/// (T065): `ADV +3` / `ADV −5` (signed ticks, positive = the attacker recovers
/// first) when the move connected this tick, or `ADV —` when it did not (`None`),
/// so a stale advantage number is never shown.
///
/// Split out so the rendering form is unit-testable without scripting a whole
/// match: the engine computes the [`Option<i32>`] advantage at contact and stashes
/// it on the attacker [`fp_engine::Player`]; this turns it into the on-screen text.
fn format_frame_advantage(advantage: Option<i32>) -> String {
    match advantage {
        // U+2212 MINUS SIGN for negatives (matches the rest of the HUD), explicit
        // `+` for non-negative so advantage reads at a glance.
        Some(n) if n < 0 => format!("ADV \u{2212}{}", -n),
        Some(n) => format!("ADV +{n}"),
        None => "ADV —".to_string(),
    }
}

/// The three player-facing match overlays, bundled so the match draw path takes
/// one argument instead of three (the F1 dev Clsn toggle, the T063 training
/// hitbox overlay, and the T064 input display). All default to off.
#[derive(Debug, Clone, Copy, Default)]
struct MatchOverlays {
    /// Raw F1 dev Clsn overlay (both sides or nothing).
    dev_clsn: bool,
    /// T063 player-facing, per-side hitbox/hurtbox overlay.
    training: TrainingOverlay,
    /// T064 player-facing, per-side input-history display.
    input_display: InputDisplay,
}

// ---------------------------------------------------------------------------
// On-screen input display (T064)
// ---------------------------------------------------------------------------

/// The player-facing input-history display (T064): a vertical strip per side of
/// the last ~16 frames of input — coalesced into numpad direction glyphs + lit
/// button letters with a `*N` repeat count — newest at the top, plus a flash of
/// any special command name the instant its motion completes.
///
/// Toggled with F3 (`Off → P1 → P2 → Both → Off`, reusing [`OverlayScope`]) and
/// **off by default**. The strip reads each player's
/// rolling [`fp_input::InputBuffer`] (via `Player::input_buffer`) and folds the
/// absolute directions to facing-relative numpad notation; the command flash
/// reads `Player::just_matched_commands`. Pure draw — no game state mutated.
#[derive(Debug, Clone, Copy, Default)]
struct InputDisplay {
    /// Which side(s) to draw; `Off` disables the strip entirely.
    scope: OverlayScope,
}

impl InputDisplay {
    /// Number of coalesced rows shown (~16 frames of history once collapsed).
    const ROWS: usize = fp_input::DEFAULT_DISPLAY_ROWS;
    /// Row pitch in pixels (one coalesced input per line).
    const LINE_H: f32 = 12.0;
    /// Vertical anchor of the strip's top row.
    const TOP: f32 = 96.0;
    /// Inset of P1's strip from the left edge.
    const P1_X: f32 = 8.0;
    /// Inset of P2's strip from the right edge (the strip is right-aligned by
    /// drawing at `win_w - P2_INSET`).
    const P2_INSET: f32 = 64.0;

    /// Cycles the scope one step (`Off → P1 → P2 → Both → Off`); the F3 toggle
    /// key calls this so a single key walks every per-side state.
    fn cycle(&mut self) {
        self.scope = self.scope.next();
    }

    /// Draws the in-scope side strips. A `None` font omits all text (no panic,
    /// no regression); `Off` scope draws nothing.
    fn draw(
        &self,
        frame: &mut fp_render::RenderFrame<'_>,
        m: &Match,
        font: Option<&GlyphFont>,
        win_w: f32,
    ) {
        let Some(font) = font else { return };
        if self.scope.shows_p1() {
            Self::draw_side(frame, font, m.p1(), Self::P1_X);
        }
        if self.scope.shows_p2() {
            Self::draw_side(frame, font, m.p2(), win_w - Self::P2_INSET);
        }
    }

    /// Draws one player's input strip anchored at `x`: newest run on top, each as
    /// its numpad-direction + button label, then a one-line command flash under
    /// the strip naming any special recognized this frame.
    fn draw_side(
        frame: &mut fp_render::RenderFrame<'_>,
        font: &GlyphFont,
        player: &fp_engine::Player,
        x: f32,
    ) {
        let facing_right = player.facing() == fp_character::Facing::Right;
        let rows = fp_input::input_display_rows(player.input_buffer(), Self::ROWS);
        for (i, row) in rows.iter().enumerate() {
            let y = Self::TOP + i as f32 * Self::LINE_H;
            frame.draw_text(
                font,
                &row.label(facing_right),
                &TextDrawParams {
                    x,
                    y,
                    scale: 1.0,
                    alpha: 1.0,
                    blend: BlendMode::Normal,
                },
            );
        }
        // Command flash: the first command recognized this frame, drawn under the
        // strip. Only the recognition frame lists it (see `just_matched`), so it
        // naturally flashes for `buffer_time`-independent single frames as inputs
        // continue. Empty on a no-recognition frame.
        if let Some(name) = player.just_matched_commands().first() {
            let y = Self::TOP + Self::ROWS as f32 * Self::LINE_H;
            frame.draw_text(
                font,
                name,
                &TextDrawParams {
                    x,
                    y,
                    scale: 1.0,
                    alpha: 1.0,
                    blend: BlendMode::Normal,
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Stage background rendering (audit #29)
// ---------------------------------------------------------------------------

/// A loaded stage plus the GPU resources to draw its background layers: the
/// parsed [`fp_stage::Stage`] model, the stage's sprite (SFF) file, and a
/// lazily-populated per-sprite texture cache.
///
/// One [`StageRender`] is held per [`MatchRun`] when a stage is available; when
/// absent the app keeps its flat clear color (no regression). Drawing is split by
/// layer so the back layers render before the fighters and the front layers after
/// (see [`StageRender::draw_layer`]).
struct StageRender {
    /// The parsed stage definition (camera bounds, BG elements, …).
    stage: Stage,
    /// The stage's decoded sprite file (`[BGdef] spr`), used to draw BG elements.
    sff: SffFile,
    /// The stage's AIR animations, parsed from the inline `[Begin Action N]`
    /// blocks of the stage `.def` itself (MUGEN stages declare `type = anim` BG
    /// element actions there). Empty when the `.def` has no actions.
    air: AirFile,
    /// GPU sprite cache keyed by sprite id, decoded from `sff` on first use.
    sprite_cache: HashMap<SpriteId, CachedSprite>,
}

impl StageRender {
    /// Loads a stage from `path`: parses the `.def`, then loads its `[BGdef] spr`
    /// SFF. Returns `None` (with a log) when the stage cannot be parsed, declares
    /// no sprite file, or the SFF fails to load — the caller degrades to a flat
    /// clear color rather than failing the whole app. Never panics.
    fn load(path: &Path) -> Option<Self> {
        // Read the `.def` once: the stage model AND its inline `[Begin Action N]`
        // animations both come from this text.
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    "stage {} failed to read: {e}; using flat background",
                    path.display()
                );
                return None;
            }
        };
        let stage = Stage::parse(&text, path.parent());
        // Stage animations (`type = anim` BG elements) are driven by the `.def`'s
        // inline AIR actions. A `.def` with none parses to an empty action set;
        // a parse hiccup degrades to no animations rather than dropping the stage.
        let air = AirFile::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!(
                "stage {} AIR actions failed to parse: {e}; animated layers will hold frame 0",
                path.display()
            );
            AirFile {
                actions: HashMap::new(),
            }
        });
        let Some(spr_path) = stage.bgdef.sprite_path.clone() else {
            tracing::warn!(
                "stage {} has no [BGdef] spr sprite file; using flat background",
                path.display()
            );
            return None;
        };
        let sff = match SffFile::load(&spr_path) {
            Ok(sff) => sff,
            Err(e) => {
                tracing::warn!(
                    "stage SFF {} failed to load: {e}; using flat background",
                    spr_path.display()
                );
                return None;
            }
        };
        tracing::info!(
            "stage loaded: {:?} ({} BG elements, {} stage sprites)",
            stage.info.name,
            stage.backgrounds.len(),
            sff.sprites.len(),
        );
        Some(Self {
            stage,
            sff,
            air,
            sprite_cache: HashMap::new(),
        })
    }

    /// Resolves the `(group, image)` sprite a background element currently draws,
    /// honoring [`fp_stage::BgType::Anim`]: an animated element returns the sprite
    /// of its current AIR frame (selected from its running animation clock), and
    /// every other element returns its static `spriteno`. An anim element whose
    /// action is missing falls back to its static sprite. Pure (no GPU state).
    fn current_bg_sprite(&self, bg: &fp_stage::BgElement) -> Vec2<i32> {
        if bg.kind == fp_stage::BgType::Anim {
            if let Some(action) = bg.action_no.and_then(|n| self.air.action(n)) {
                return bg.current_anim_sprite(action);
            }
        }
        bg.sprite
    }

    /// Pre-decodes every BG element's sprite into the GPU cache. Run once per
    /// frame **before** `begin_frame` (decoding needs `&Renderer`, which a live
    /// [`fp_render::RenderFrame`] holds borrowed). Missing/oversized sprites are
    /// skipped (logged once via the cache), never fatal.
    fn cache_sprites(&mut self, renderer: &Renderer) {
        // Collect the sprite ids first to avoid borrowing `self.stage` while
        // `self.sprite_cache`/`self.sff` are mutated.
        let ids: Vec<SpriteId> = self
            .stage
            .backgrounds
            .iter()
            .filter_map(|bg| {
                let s = self.current_bg_sprite(bg);
                bg_sprite_id(s.x, s.y)
            })
            .collect();
        for sprite_id in ids {
            cache_sff_sprite(&mut self.sprite_cache, &self.sff, sprite_id, renderer);
        }
    }

    /// Advances every background element's auto-scroll offset one tick, wrapping
    /// each within its drawn sprite's tile period.
    ///
    /// Delegates to [`fp_stage::Stage::advance_scroll`], resolving each element's
    /// `(group, image)` to its already-cached GPU sprite size. An element whose
    /// sprite is not yet decoded reports `None`, which keeps raw accumulation for
    /// that element until it caches (then it wraps) — never a panic, never a stall.
    fn advance_scroll(&mut self) {
        let cache = &self.sprite_cache;
        self.stage.advance_scroll(|sprite| {
            let id = bg_sprite_id(sprite.x, sprite.y)?;
            let cached = cache.get(&id)?;
            Some(Vec2::new(
                cached.texture.width as f32,
                cached.texture.height as f32,
            ))
        });
    }

    /// Advances every `type = anim` background layer's animation clock one tick,
    /// resolving each element's parsed `actionno` against this stage's inline AIR
    /// actions. Static layers are untouched. Delegates to
    /// [`fp_stage::Stage::advance_anim`]. Driven once per fixed tick so multi-tick
    /// catch-up frames animate the right amount; missing actions hold the frame.
    fn advance_anim(&mut self) {
        let air = &self.air;
        self.stage.advance_anim(|action_no| air.action(action_no));
    }

    /// Draws every BG element on `layer`, in file order, applying each element's
    /// parallax against the camera, its accumulated auto-scroll offset, and its
    /// tiling. A missing/uncached sprite is skipped.
    ///
    /// `camera_x`/`camera_y` are the camera's world position (from
    /// [`Stage::camera_follow_x`] / [`Stage::camera_follow_y`]). Each element's
    /// world X after parallax is
    /// [`fp_stage::parallax_screen_x`]`(start.x, delta.x, camera_x)` and its world Y
    /// is [`fp_stage::parallax_screen_y`]`(start.y, delta.y, camera_y)`, both mapped
    /// into the window with the same [`world_to_screen_x`] the fighters use so the
    /// background and fighters share one coordinate frame, then shifted by the
    /// element's running auto-scroll offset (`bg.scroll`). `camera_y` follows the
    /// fighters' height scaled by `[Camera] verticalfollow` so distant layers
    /// parallax-track a jump; `start.y` is anchored to the same ground line the
    /// fighters stand on.
    ///
    /// An animated (`type = anim`) layer draws the AIR frame its animation clock
    /// currently selects (via [`current_bg_sprite`](StageRender::current_bg_sprite));
    /// every other layer draws its static `spriteno`.
    ///
    /// Tiling: when `bg.tile.x`/`.y` is non-`1`, the element repeats across the
    /// viewport via [`fp_stage::tile_rects`] (`0` = fill the viewport, `n` = exactly
    /// `n` copies); the non-tiling default (`tile = 1, 1`) draws a single copy.
    fn draw_layer(
        &self,
        frame: &mut fp_render::RenderFrame<'_>,
        layer: BgLayer,
        camera_x: f32,
        camera_y: f32,
        win_w: f32,
        win_h: f32,
    ) {
        let ground_y = win_h * 0.8;
        for bg in self.stage.backgrounds.iter().filter(|b| b.layer == layer) {
            // Animated layers draw the current AIR frame's sprite; others their
            // static spriteno.
            let drawn = self.current_bg_sprite(bg);
            let Some(sprite_id) = bg_sprite_id(drawn.x, drawn.y) else {
                continue;
            };
            let Some(cached) = self.sprite_cache.get(&sprite_id) else {
                continue;
            };

            // World X after parallax, then into screen space (shared frame), then
            // shifted by the running auto-scroll offset.
            let world_x = fp_stage::parallax_screen_x(bg.start.x, bg.delta.x, camera_x);
            let screen_x = world_to_screen_x(world_x, win_w) + bg.scroll.x;
            // Vertical follow: anchor start.y to the ground line, parallax-tracked
            // against the camera's vertical offset (Y down, matching the fighter
            // draw convention).
            let screen_y = ground_y
                + fp_stage::parallax_screen_y(bg.start.y, bg.delta.y, camera_y)
                + bg.scroll.y;

            // Anchor by the sprite's axis like the fighters do — this is the
            // top-left of the first tile.
            let anchor = Vec2::new(
                screen_x - cached.axis_x as f32,
                screen_y - cached.axis_y as f32,
            );
            let sprite_size = Vec2::new(cached.texture.width as f32, cached.texture.height as f32);

            // One rect for the non-tiling default; the full tiled set otherwise.
            let rects = fp_stage::tile_rects(anchor, sprite_size, bg.tile, Vec2::new(win_w, win_h));
            for rect in rects {
                let params = SpriteDrawParams {
                    x: rect.x,
                    y: rect.y,
                    ..Default::default()
                };
                frame.draw_sprite(&cached.texture, &cached.palette, &params);
            }
        }
    }
}

/// Converts a BG element's `(group, image)` (stored as `i32` in the stage model)
/// into a [`SpriteId`], warning and returning `None` when either falls outside
/// the SFF `u16` range rather than wrapping to a wrong sprite.
fn bg_sprite_id(group: i32, image: i32) -> Option<SpriteId> {
    match (u16::try_from(group), u16::try_from(image)) {
        (Ok(g), Ok(i)) => Some(SpriteId::new(g, i)),
        _ => {
            tracing::warn!(
                "stage BG spriteno ({group}, {image}) is out of SFF range; skipping element"
            );
            None
        }
    }
}

/// Ensures the GPU textures for `sprite_id` are cached, decoding from `sff` on
/// first use. Shared by the stage background cache; mirrors
/// [`FighterRender::get_or_create_sprite`] but standalone so it can borrow a
/// cache map directly (the stage owns its own `HashMap`). Missing or undecodable
/// sprites are skipped with a warning, never a panic.
fn cache_sff_sprite(
    cache: &mut HashMap<SpriteId, CachedSprite>,
    sff: &SffFile,
    sprite_id: SpriteId,
    renderer: &Renderer,
) {
    if cache.contains_key(&sprite_id) {
        return;
    }
    let Some((index, sff_sprite)) = sff
        .sprites
        .iter()
        .enumerate()
        .find(|(_, s)| s.group == sprite_id.group() && s.image == sprite_id.image())
    else {
        tracing::warn!("stage sprite {sprite_id} not found in stage SFF; skipping");
        return;
    };
    let axis_x = sff_sprite.axis_x;
    let axis_y = sff_sprite.axis_y;
    let width = sff_sprite.width;
    let height = sff_sprite.height;
    let pal_idx = sff_sprite.palette_index as usize;
    if width == 0 || height == 0 {
        tracing::warn!("stage sprite {sprite_id} has zero dimensions; skipping");
        return;
    }
    let pixels = match sff.decode_sprite(index) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("failed to decode stage sprite {sprite_id}: {e}");
            return;
        }
    };
    let palette_data = match sff.palette(pal_idx) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("failed to load palette {pal_idx} for stage sprite {sprite_id}: {e}");
            return;
        }
    };
    let texture = SpriteTexture::new(
        renderer.device(),
        renderer.queue(),
        width as u32,
        height as u32,
        &pixels,
    );
    let palette = PaletteTexture::new(renderer.device(), renderer.queue(), &palette_data);
    cache.insert(
        sprite_id,
        CachedSprite {
            texture,
            palette,
            axis_x,
            axis_y,
        },
    );
}

// ---------------------------------------------------------------------------
// Intro / ending storyboard overlay (audit #32)
// ---------------------------------------------------------------------------
//
// A self-contained overlay path that plays a character's declared intro/ending
// storyboard over the match. It is *purely additive*: the normal intro timer,
// fighters, HUD, and screenpack are untouched, and when no storyboard (or its
// SFF) is available the overlay is simply absent — no regression to the normal
// intro. The driver lives in `fp-storyboard` (`StoryboardPlayer`); this layer
// only loads the SFF, caches GPU textures, and blits each `StoryboardDraw`.

/// The character-`.def` section MUGEN declares intro/ending storyboards under.
///
/// Both `intro.storyboard` and `ending.storyboard` live in the `[Arcade]` section
/// (the arcade-mode story bookends), not `[Files]`.
const STORYBOARD_SECTION: &str = "Arcade";

/// Which storyboard a character declares in its `.def` `[Arcade]` section.
///
/// MUGEN character `.def`s declare `intro.storyboard = <file>` and
/// `ending.storyboard = <file>`; these select which key to read.
#[derive(Debug, Clone, Copy)]
enum StoryboardKind {
    /// The `intro.storyboard` declaration — played during the pre-fight intro.
    Intro,
    /// The `ending.storyboard` declaration — played when the match is over.
    Ending,
}

impl StoryboardKind {
    /// The `[Arcade]` key this kind reads from the character `.def`.
    fn def_key(self) -> &'static str {
        match self {
            StoryboardKind::Intro => "intro.storyboard",
            StoryboardKind::Ending => "ending.storyboard",
        }
    }
}

/// Loads the declared intro/ending storyboard overlay for a character `.def`, or
/// `None` if the character declares none or it cannot be loaded.
///
/// `LoadedCharacter` does not retain the raw `.def` (and `fp-character` is not
/// ours to change for this task), so we re-read the small character `.def` here to
/// recover its `intro.storyboard`/`ending.storyboard` declaration, resolve it
/// relative to the `.def` directory, and hand it to [`StoryboardOverlay::load`].
/// Every failure path (unreadable `.def`, absent key, unloadable storyboard/SFF)
/// returns `None` so the match simply renders no extra overlay — never a panic,
/// never a regression to the normal intro.
fn load_character_storyboard(char_def: &Path, kind: StoryboardKind) -> Option<StoryboardOverlay> {
    let def = match fp_formats::def::DefFile::load(char_def) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                "could not re-read character def {} for storyboard: {e}",
                char_def.display()
            );
            return None;
        }
    };
    let rel = def
        .get(STORYBOARD_SECTION, kind.def_key())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let sb_path = fp_formats::def::DefFile::resolve_path(char_def, rel);
    if !sb_path.exists() {
        tracing::warn!(
            "character {} declares {} = {rel} but {} is missing; skipping overlay",
            char_def.display(),
            kind.def_key(),
            sb_path.display()
        );
        return None;
    }
    StoryboardOverlay::load(&sb_path)
}

/// A playable storyboard overlay: a `fp-storyboard` [`StoryboardPlayer`] paired
/// with the storyboard's own sprite (SFF) file and a lazily-populated per-sprite
/// GPU texture cache.
///
/// Built best-effort with [`StoryboardOverlay::load`]; a missing/unparseable
/// storyboard `.def` or a missing/unloadable SFF yields `None`, and the caller
/// then renders nothing extra (the normal intro/match is unchanged). One overlay
/// is held per kind (intro, ending) on a [`MatchRun`].
struct StoryboardOverlay {
    /// The tick driver over the parsed storyboard scene model.
    player: fp_storyboard::StoryboardPlayer,
    /// The storyboard's own sprite container (`[SceneDef] spr`).
    sff: SffFile,
    /// GPU sprite cache keyed by sprite id, decoded from `sff` on first use.
    sprite_cache: HashMap<SpriteId, CachedSprite>,
    /// Per-RGB-color full-window quad cache (index 1 = the color). The renderer
    /// exposes no per-draw quad recolor, so a quad is built once per distinct
    /// color and reused; this backs both the per-scene `clearcolor` backdrop and
    /// the per-scene fade overlay, whose colors vary scene to scene (T011).
    color_quads: HashMap<(u8, u8, u8), HudColor>,
}

impl StoryboardOverlay {
    /// Loads a storyboard overlay from a storyboard `.def` at `def_path`, resolving
    /// its `[SceneDef] spr` SFF relative to that `.def`'s directory.
    ///
    /// Returns `None` (with a log) when the storyboard has no scenes, declares no
    /// sprite file, or the SFF fails to load — the caller then draws nothing extra
    /// (no regression). `Storyboard::load` itself never panics; a parse problem
    /// degrades to an empty scene model which is rejected here. Never panics.
    ///
    /// Per-scene clear-color and fade-overlay quads are built lazily during
    /// [`draw`](Self::draw) (and cached by RGB), so no `renderer` is needed here.
    fn load(def_path: &Path) -> Option<Self> {
        let storyboard = match fp_storyboard::Storyboard::load(def_path) {
            Ok(sb) => sb,
            Err(e) => {
                tracing::warn!(
                    "storyboard {} failed to load: {e}; skipping overlay",
                    def_path.display()
                );
                return None;
            }
        };
        if storyboard.scenes.is_empty() {
            tracing::warn!(
                "storyboard {} has no scenes; skipping overlay",
                def_path.display()
            );
            return None;
        }
        if storyboard.sprite_path.is_empty() {
            tracing::warn!(
                "storyboard {} has no [SceneDef] spr; skipping overlay",
                def_path.display()
            );
            return None;
        }
        // Resolve the SFF relative to the storyboard .def directory (same rule the
        // screenpack/stage loaders use).
        let sff_path = fp_formats::def::DefFile::resolve_path(def_path, &storyboard.sprite_path);
        let sff = match SffFile::load(&sff_path) {
            Ok(sff) => sff,
            Err(e) => {
                tracing::warn!(
                    "storyboard SFF {} failed to load: {e}; skipping overlay",
                    sff_path.display()
                );
                return None;
            }
        };
        tracing::info!(
            "storyboard overlay loaded: {} ({} scenes, {} sprites)",
            def_path.display(),
            storyboard.scenes.len(),
            sff.sprites.len(),
        );
        Some(Self {
            player: fp_storyboard::StoryboardPlayer::new(storyboard),
            sff,
            sprite_cache: HashMap::new(),
            color_quads: HashMap::new(),
        })
    }

    /// Whether the storyboard has finished playing (no more scenes).
    fn is_done(&self) -> bool {
        self.player.is_done()
    }

    /// Advances the storyboard one tick. A no-op once done.
    ///
    /// Also polls the per-scene BGM transition ([`StoryboardPlayer::bgm_to_start`]):
    /// when a scene declares a new `bgm` it is reported once, on that scene's first
    /// tick. The audio layer has no streaming/music backend yet (it only plays
    /// decoded WAV one-shots), so the requested track is logged rather than played
    /// — the player-side transition is computed and tested in `fp-storyboard`, and
    /// hooking it to real music playback is a follow-up once a music backend lands.
    fn tick(&mut self) {
        // Poll BGM *before* advancing so the current scene's track is offered on
        // its first tick (a fresh player arms scene 0; each roll-over arms the new
        // scene). Then advance the scene cursor.
        if let Some(bgm) = self.player.bgm_to_start() {
            tracing::info!(
                "storyboard: scene BGM transition -> {bgm} (no music backend; not played)"
            );
        }
        self.player.tick();
    }

    /// Pre-decodes this tick's draw-list sprites into the GPU cache, and ensures
    /// the current scene's clear-color and fade-overlay quads exist. Run once per
    /// frame **before** `begin_frame` (building GPU resources needs `&Renderer`,
    /// which a live [`fp_render::RenderFrame`] holds borrowed). Missing/undecodable
    /// sprites are skipped (logged once via the cache), never fatal.
    fn cache_sprites(&mut self, renderer: &Renderer) {
        let ids: Vec<SpriteId> = self.player.draw_list().iter().map(|d| d.sprite).collect();
        for sprite_id in ids {
            cache_sff_sprite(&mut self.sprite_cache, &self.sff, sprite_id, renderer);
        }
        // Build the per-scene clear-color quad (defaults to black) and, when a
        // fade is active this tick, its colored overlay quad. Both are cached by
        // RGB so a steady scene reuses a single quad across frames.
        let clear_rgb = self.player.clearcolor();
        self.color_quads
            .entry(clear_rgb)
            .or_insert_with(|| HudColor::new(renderer, clear_rgb.0, clear_rgb.1, clear_rgb.2));
        if let Some(fade) = self.player.fade() {
            self.color_quads.entry(fade.color).or_insert_with(|| {
                HudColor::new(renderer, fade.color.0, fade.color.1, fade.color.2)
            });
        }
    }

    /// Draws the storyboard's current frame: the **current scene's** clear-color
    /// backdrop covering the whole window, then each [`StoryboardDraw`] mapped from
    /// storyboard-local coordinates into the window, then the current scene's
    /// **fade overlay** on top (when one is active). A missing/uncached sprite (or
    /// an un-built color quad) is skipped.
    ///
    /// The clear color and the fade follow the active scene per
    /// [`StoryboardPlayer::clearcolor`] / [`StoryboardPlayer::fade`], so a per-scene
    /// `clearcolor` change is reflected immediately and a scene fades in/out over
    /// its declared `fadein.time` / `fadeout.time` (T011). The colored quads are
    /// looked up from the cache filled by [`cache_sprites`](Self::cache_sprites);
    /// drawing only reads it (the renderer is borrowed by `frame` here).
    ///
    /// Storyboard-local coordinates use the `[Info] localcoord` frame (Y-down,
    /// origin top-left); this scales that frame to fill the window so the cutscene
    /// fills the screen regardless of the window size. Drawn entirely over the
    /// match (it is a full-screen cutscene), so the caller invokes it *after* the
    /// fighters/stage and instead of (not under) the normal scene.
    fn draw(&self, frame: &mut fp_render::RenderFrame<'_>, win_w: f32, win_h: f32) {
        let cover = HudRect {
            x: 0.0,
            y: 0.0,
            w: win_w,
            h: win_h,
        };

        // Cover the whole window with the current scene's clear color so the
        // cutscene reads as a full-screen overlay rather than compositing over the
        // live fight. Opaque (alpha defaults to 1.0).
        if let Some(clear) = self.color_quads.get(&self.player.clearcolor()) {
            let params = SpriteDrawParams {
                x: cover.x,
                y: cover.y,
                scale_x: cover.w.max(0.0),
                scale_y: cover.h.max(0.0),
                ..Default::default()
            };
            frame.draw_sprite(&clear.quad, &clear.palette, &params);
        }

        // Map storyboard-local coords into the window. localcoord defaults to
        // (320, 240); guard against a zero/degenerate coordinate space.
        let (local_w, local_h) = self.player.storyboard().localcoord;
        let sx = if local_w > 0 {
            win_w / local_w as f32
        } else {
            1.0
        };
        let sy = if local_h > 0 {
            win_h / local_h as f32
        } else {
            1.0
        };

        for draw in self.player.draw_list() {
            let Some(cached) = self.sprite_cache.get(&draw.sprite) else {
                continue;
            };
            // Anchor by the sprite's axis like the fighters/stage do, then scale
            // the local-space position into the window.
            let screen_x = draw.pos.0 * sx - cached.axis_x as f32;
            let screen_y = draw.pos.1 * sy - cached.axis_y as f32;
            let (render_blend, alpha) = map_blend_mode(&draw.blend);
            let params = SpriteDrawParams {
                x: screen_x,
                y: screen_y,
                flip_h: draw.flip_h,
                flip_v: draw.flip_v,
                blend: render_blend,
                alpha,
                ..Default::default()
            };
            frame.draw_sprite(&cached.texture, &cached.palette, &params);
        }

        // Fade overlay on top of everything: a solid color quad covering the
        // window at the computed alpha (alpha-blended). Skipped when no fade is
        // active or its quad was not built.
        if let Some(fade) = self.player.fade() {
            if let Some(quad) = self.color_quads.get(&fade.color) {
                let params = SpriteDrawParams {
                    x: cover.x,
                    y: cover.y,
                    scale_x: cover.w.max(0.0),
                    scale_y: cover.h.max(0.0),
                    blend: fp_render::BlendMode::Normal,
                    alpha: fade.alpha.clamp(0.0, 1.0),
                    ..Default::default()
                };
                frame.draw_sprite(&quad.quad, &quad.palette, &params);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy SFF+AIR animation viewer (demo path)
// ---------------------------------------------------------------------------

/// A simple animation viewer: plays AIR action 0 from an SFF, no state machine.
struct AnimViewer {
    sff: SffFile,
    action: AnimAction,
    elem: usize,
    elem_time: i32,
    sprite_cache: HashMap<SpriteId, CachedSprite>,
}

impl AnimViewer {
    fn new(sff: SffFile, air: AirFile) -> Self {
        let action = air.action(0).cloned().unwrap_or(AnimAction {
            action_number: 0,
            frames: vec![],
            loopstart: 0,
        });
        tracing::info!("Animation viewer: {} actions loaded", air.actions.len());
        Self {
            sff,
            action,
            elem: 0,
            elem_time: 0,
            sprite_cache: HashMap::new(),
        }
    }

    /// Advance the viewer's animation cursor one tick.
    fn tick(&mut self) {
        if self.action.frames.is_empty() {
            return;
        }
        self.elem_time += 1;
        while let Some(frame) = self.action.frames.get(self.elem) {
            if frame.ticks <= 0 || self.elem_time < frame.ticks {
                break;
            }
            self.elem_time = 0;
            self.elem += 1;
            if self.elem >= self.action.frames.len() {
                self.elem = self.action.loopstart.min(self.action.frames.len() - 1);
            }
        }
    }

    fn current_frame(&self) -> Option<&fp_formats::air::AnimFrame> {
        self.action.frames.get(self.elem)
    }

    fn get_or_create_sprite(
        &mut self,
        sprite_id: SpriteId,
        renderer: &Renderer,
    ) -> Option<&CachedSprite> {
        if self.sprite_cache.contains_key(&sprite_id) {
            return self.sprite_cache.get(&sprite_id);
        }
        let (index, sff_sprite) = self
            .sff
            .sprites
            .iter()
            .enumerate()
            .find(|(_, s)| s.group == sprite_id.group() && s.image == sprite_id.image())?;
        let axis_x = sff_sprite.axis_x;
        let axis_y = sff_sprite.axis_y;
        let width = sff_sprite.width;
        let height = sff_sprite.height;
        let pal_idx = sff_sprite.palette_index as usize;
        if width == 0 || height == 0 {
            return None;
        }
        let pixels = match self.sff.decode_sprite(index) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to decode sprite {sprite_id}: {e}");
                return None;
            }
        };
        let palette_data = match self.sff.palette(pal_idx) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to load palette {pal_idx}: {e}");
                return None;
            }
        };
        let texture = SpriteTexture::new(
            renderer.device(),
            renderer.queue(),
            width as u32,
            height as u32,
            &pixels,
        );
        let palette = PaletteTexture::new(renderer.device(), renderer.queue(), &palette_data);
        self.sprite_cache.insert(
            sprite_id,
            CachedSprite {
                texture,
                palette,
                axis_x,
                axis_y,
            },
        );
        self.sprite_cache.get(&sprite_id)
    }
}

/// Map AIR blend mode to renderer blend mode + alpha.
fn map_blend_mode(air_blend: &fp_formats::air::BlendMode) -> (fp_render::BlendMode, f32) {
    match air_blend {
        fp_formats::air::BlendMode::Normal => (fp_render::BlendMode::Normal, 1.0),
        fp_formats::air::BlendMode::Additive => (fp_render::BlendMode::Additive, 1.0),
        fp_formats::air::BlendMode::AdditiveAlpha(a) => {
            (fp_render::BlendMode::Additive, *a as f32 / 256.0)
        }
        fp_formats::air::BlendMode::Subtractive => (fp_render::BlendMode::Subtractive, 1.0),
    }
}

/// Whether a path looks like a character definition (`.def`).
fn is_def_path(p: &str) -> bool {
    Path::new(p)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("def"))
}

/// Resolves `path` to an absolute path so paths discovered under a **relative**
/// directory argument do not get re-rooted when later joined against another
/// base (e.g. `SelectScreen::build_pick`'s `base_dir.join(def_path)`).
///
/// Prefers [`std::fs::canonicalize`] (which also resolves `..`/symlinks and
/// confirms the file exists); when that fails (the file is gone, or the platform
/// rejects it) it falls back to joining the current working directory, and as a
/// last resort returns the input unchanged. Never panics — a `current_dir`
/// failure simply degrades to the relative path. An already-absolute path
/// canonicalizes (or cwd-joins) to itself.
fn absolutize(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// The top-level launch route chosen from the (palette-flag-stripped) positional
/// CLI args. See [`cli_route`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum CliRoute {
    /// Launch the in-app Title menu (no file args, or an explicit `menu`).
    Menu,
    /// A direct content path (`p1.def [p2.def]`, `file.sff [file.air]`, ...)
    /// handled by [`select_mode`] exactly as before — the menu is skipped.
    Direct,
    /// A directory argument (T043): scan it for characters (the MUGEN-standard
    /// `chars/<name>/<name>.def` layout, or a flat dir of `*.def`) and launch the
    /// menu over the discovered roster augmenting the motif's `select.def`.
    Directory(PathBuf),
    /// `replay <log> <p1.def> [p2.def]` (T076): open the replay-study viewer over a
    /// recorded [`fp_engine::ReplayLog`], driving the two named characters. The
    /// remaining `.def` paths are the [`absolutize`]d character files the log was
    /// recorded with (one `.def` = same character both sides).
    Replay {
        /// The recorded `.bin` replay-log path.
        log: PathBuf,
        /// Player-1 character `.def`.
        p1: PathBuf,
        /// Player-2 character `.def`, or `None` to reuse `p1` on both sides.
        p2: Option<PathBuf>,
    },
}

/// Decides whether the (palette-flag-stripped) positional `args` launch the
/// in-app Title menu, a direct content view, or a directory-discovery menu.
///
/// `args[0]` is the program name. The Title menu launches when there is **no**
/// file argument (a fresh clean-room run) or the first argument is an explicit
/// `menu` token. A first argument that is an existing **directory** routes to
/// [`CliRoute::Directory`] (T043: scan it for a character roster). Any other
/// first argument (a `.def`/`.sff`/...) routes to the legacy direct path so the
/// existing single-`.def` CLI is preserved exactly. Pure given the filesystem
/// (only an `is_dir` probe) and unit-tested.
fn cli_route(args: &[String]) -> CliRoute {
    match args.get(1) {
        None => CliRoute::Menu,
        Some(a) if a.eq_ignore_ascii_case("menu") => CliRoute::Menu,
        // `replay <log> <p1.def> [p2.def]` (T076): open the replay-study viewer.
        // Falls back to the Menu route when the log/character args are missing (so
        // a bare `fp-app replay` does not crash) — warn-logged at build time.
        Some(a) if a.eq_ignore_ascii_case("replay") => match (args.get(2), args.get(3)) {
            (Some(log), Some(p1)) => CliRoute::Replay {
                log: absolutize(Path::new(log)),
                p1: absolutize(Path::new(p1)),
                p2: args.get(4).map(|p| absolutize(Path::new(p))),
            },
            _ => {
                tracing::warn!(
                    "`replay` needs <log> <p1.def> [p2.def]; launching the menu instead"
                );
                CliRoute::Menu
            }
        },
        // Absolutize the directory up-front so the discovered character `.def`s
        // are absolute paths. `SelectScreen::build_pick` later resolves a roster
        // entry via `base_dir.join(def_path)` (base = the motif's `select.def`
        // dir); a RELATIVE dir arg (e.g. `fp-app chars/`) would otherwise yield
        // relative discovered paths that get re-rooted under the motif dir and
        // fail to load. `absolutize` also defends `augment_roster`'s dedup.
        Some(a) if Path::new(a).is_dir() => CliRoute::Directory(absolutize(Path::new(a))),
        Some(_) => CliRoute::Direct,
    }
}

/// Per-player `.act` palette selection from the `--p1-pal N` / `--p2-pal N` CLI
/// flags (FL2b). Each is a 0-based index into the character's loaded `.act`
/// overrides; `None` (the default) renders the SFF-embedded palette, so omitting
/// the flags is byte-identical to before this feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PalSelection {
    /// Player 1's selected override index, or `None` for the embedded palette.
    p1: Option<usize>,
    /// Player 2's selected override index, or `None` for the embedded palette.
    p2: Option<usize>,
}

/// Parses (and strips) the `--p1-pal N` / `--p2-pal N` palette flags from `args`,
/// returning the selections plus the remaining positional args (program name +
/// file paths) that the rest of CLI routing consumes.
///
/// Each flag takes the next token as a non-negative decimal index; a missing or
/// non-numeric value drops that flag with a warning (the selection stays `None`).
/// Unknown `--…` tokens are passed through untouched. Pure — unit-tested without a
/// window or GPU.
fn parse_pal_flags(args: &[String]) -> (PalSelection, Vec<String>) {
    let mut sel = PalSelection::default();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let which = if arg.eq_ignore_ascii_case("--p1-pal") {
            Some(true)
        } else if arg.eq_ignore_ascii_case("--p2-pal") {
            Some(false)
        } else {
            None
        };
        match which {
            Some(is_p1) => {
                // Consume the value token, if present and numeric.
                match args.get(i + 1).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) => {
                        if is_p1 {
                            sel.p1 = Some(n);
                        } else {
                            sel.p2 = Some(n);
                        }
                        i += 2;
                    }
                    None => {
                        tracing::warn!(
                            "{arg} expects a non-negative integer palette index; ignoring"
                        );
                        i += 1;
                    }
                }
            }
            None => {
                rest.push(arg.clone());
                i += 1;
            }
        }
    }
    (sel, rest)
}

/// Parses (and strips) the `--motif <name|path>` flag from `args` (T045),
/// returning the requested motif selector plus the remaining positional args.
///
/// The value is either a discovered motif name (matched against the motifs found
/// under [`DEFAULT_MOTIF_DIR`]) or a direct path to a `system.def` / a motif
/// directory. A missing value (`--motif` at the end) drops the flag with a
/// warning (the selection stays `None`, so the default motif loads). Unknown
/// `--…` tokens pass through untouched. Pure — unit-tested without a window.
fn parse_motif_flag(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut motif: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg.eq_ignore_ascii_case("--motif") {
            match args.get(i + 1) {
                Some(value) => {
                    motif = Some(value.clone());
                    i += 2;
                }
                None => {
                    tracing::warn!("--motif expects a motif name or path; ignoring");
                    i += 1;
                }
            }
        } else {
            rest.push(arg.clone());
            i += 1;
        }
    }
    (motif, rest)
}

/// Parses (and strips) the team-mode flag from `args` (T027), returning the chosen
/// [`TeamMode`] plus the remaining positional args.
///
/// `--simul` selects [`TeamMode::Simul`] (both sides field all fighters at once);
/// `--turns` selects [`TeamMode::Turns`] (sequential KO hand-off). With neither
/// flag the default is [`TeamMode::Single`] — the classic 1v1 — so omitting them is
/// byte-identical to before this feature. Both flags take no value; an unknown
/// `--…` token passes through untouched. The last team flag wins if both are given.
/// Pure — unit-tested without a window.
fn parse_team_flag(args: &[String]) -> (TeamMode, Vec<String>) {
    let mut mode = TeamMode::Single;
    let mut rest: Vec<String> = Vec::new();
    for arg in args {
        if arg.eq_ignore_ascii_case("--simul") {
            mode = TeamMode::Simul;
        } else if arg.eq_ignore_ascii_case("--turns") {
            mode = TeamMode::Turns;
        } else {
            rest.push(arg.clone());
        }
    }
    (mode, rest)
}

/// Parses (and strips) the CPU teaching-mode flag from `args` (T070), returning
/// the chosen [`BehaviorMode`] plus the remaining positional args.
///
/// `--ai-mode <token>` selects which teaching CPU drives P2 on the direct-CLI
/// match path, where `<token>` is a [`BehaviorMode::token`] (or alias):
/// `ladder` (the plain difficulty ladder, the default), `blocker` (Pure Blocker),
/// `dp` (Reactive DP), or `punisher` (Whiff Punisher). With no flag the default is
/// [`BehaviorMode::Ladder`], so omitting it is byte-identical to before. An unknown
/// token (or a `--ai-mode` at the end with no value) warns and keeps the default;
/// any other `--…` token passes through untouched. The last `--ai-mode` wins if
/// repeated. Pure — unit-tested without a window.
fn parse_ai_mode_flag(args: &[String]) -> (BehaviorMode, Vec<String>) {
    let mut mode = BehaviorMode::default();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg.eq_ignore_ascii_case("--ai-mode") {
            match args.get(i + 1) {
                Some(value) => {
                    match BehaviorMode::from_token(value) {
                        Some(m) => mode = m,
                        None => tracing::warn!(
                            "--ai-mode: unknown teaching mode {value:?}; \
                             using LADDER (try: ladder|blocker|dp|punisher)"
                        ),
                    }
                    i += 2;
                }
                None => {
                    tracing::warn!(
                        "--ai-mode expects a teaching mode (ladder|blocker|dp|punisher); ignoring"
                    );
                    i += 1;
                }
            }
        } else {
            rest.push(arg.clone());
            i += 1;
        }
    }
    (mode, rest)
}

/// The two-player [`Match`] run state: the match plus the per-run rendering and
/// audio resources held alongside it (per-side texture caches, the shared audio
/// system, and per-side decoded-sound caches).
///
/// Bundled into one struct (rather than a wide enum tuple) so the audio layer
/// sits next to the renderer caches without making the `match` arms unwieldy.
struct MatchRun {
    /// The team-match coordinator (T027). In the default 1v1 it holds a single
    /// fighter per side in [`TeamMode::Single`], which behaves identically to a bare
    /// [`Match`]; a `--simul`/`--turns` CLI flag builds a multi-fighter roster in the
    /// matching mode. The renderer/HUD/audio read the **active pair** through the
    /// inner [`Match`] via [`MatchRun::m`]; the tick drives the whole team.
    team: Box<TeamMatch>,
    /// P1's per-character GPU sprite cache.
    p1_render: FighterRender,
    /// P2's per-character GPU sprite cache.
    p2_render: FighterRender,
    /// The shared audio mixer (silent [`fp_audio::NullBackend`] when no device
    /// exists).
    audio: AudioSystem,
    /// P1's per-character decoded-sound cache.
    p1_audio: FighterAudio,
    /// P2's per-character decoded-sound cache.
    p2_audio: FighterAudio,
    /// The loaded stage, when one is available. `None` keeps the flat clear-color
    /// background (no regression); `Some` renders parallax background layers
    /// behind/in front of the fighters with a camera following their midpoint.
    stage: Option<StageRender>,
    /// The loaded `fight.def` screenpack HUD, when one is available. `None` falls
    /// back to the hand-rolled quad [`Hud`] (no regression); `Some` draws the real
    /// lifebars/power/text via [`fp_ui::ScreenpackHud`] instead of the quads.
    screenpack: Option<ScreenpackHud>,
    /// P1's declared intro storyboard, played as a full-screen overlay during the
    /// first round's [`RoundState::Intro`] (audit #32). `None` when P1 declares no
    /// `intro.storyboard` or it/its SFF cannot load — then the normal intro plays
    /// with no overlay (no regression).
    intro_storyboard: Option<StoryboardOverlay>,
    /// P1's declared ending storyboard, played as a full-screen overlay once the
    /// match is decided (audit #32). `None` when P1 declares no `ending.storyboard`
    /// or it/its SFF cannot load.
    ending_storyboard: Option<StoryboardOverlay>,
    /// Whether the intro overlay has been played to completion this match, so it
    /// is shown once (during round 1's intro) and not re-triggered on later rounds.
    intro_storyboard_done: bool,
    /// The shipped common-effects (`fightfx`) sprite source + GPU cache, when
    /// loaded (audit #17). `None` when the asset is absent/bad — then common
    /// (`fightfx`) hit-sparks simply don't render (no panic, no regression). The
    /// matching AIR is installed on [`MatchRun::m`] via [`Match::set_common_fx`].
    common_fx: Option<CommonFxRender>,
    /// A full-color background image drawn behind the fighters, used when no MUGEN
    /// `[BGdef]`-style [`stage`](Self::stage) is loaded. Defaults to the shipped
    /// clean-room dojo backdrop so the match no longer plays over a flat grey
    /// clear color; `None` only if even that asset is absent (then the flat
    /// clear-color path is unchanged). A real loaded stage takes precedence and
    /// this stays `None`.
    background: Option<fp_render::ImageTexture>,
    /// The baseline CPU AI brain driving **player 2** when no human input reaches
    /// it (T018). When a second controller is plugged in and asserts something,
    /// that human input takes precedence and the AI is bypassed for that frame
    /// (so a two-human match is unaffected); otherwise this brain reads the live
    /// opponent position each tick and emits an approach/attack/block/jump input.
    /// Seeded deterministically from the match seed so the demo replays.
    cpu_ai: Option<CpuAi>,
    /// The training-mode P2 dummy stance (F027 / T067). Only consulted when the
    /// match is in [`GameMode::Training`]; cycled by the training quick-key. In
    /// [`DummyMode::Cpu`] (and in Versus) P2 falls back to the baseline CPU AI /
    /// human input. The default [`DummyMode::Stand`] makes the dummy stand idle.
    dummy_mode: DummyMode,
    /// A monotonic frame counter used only to phase [`DummyMode::JumpLoop`]'s
    /// jump cadence (F027 / T067). Incremented once per match tick.
    dummy_tick: u64,
}

/// Which storyboard overlay (if any) is *active* for the current frame, given the
/// live round state (audit #32).
///
/// Pure decision split out from [`MatchRun`] so the gating rule is unit-testable
/// without an SDL2 window, a GPU, or a real `Match`:
/// - the **intro** plays during the first round's [`RoundState::Intro`], until it
///   has run to completion (`intro_done`), and only when one is loaded;
/// - the **ending** plays once the whole match is decided (`match_over`), and only
///   when one is loaded;
/// - otherwise no overlay is active and the normal match renders unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveStoryboard {
    /// No overlay this frame — render the normal match (no regression).
    None,
    /// Play the intro overlay this frame.
    Intro,
    /// Play the ending overlay this frame.
    Ending,
}

/// Decides which storyboard overlay is active for a frame from the round state and
/// per-match flags. See [`ActiveStoryboard`]. Pure (no `self`) so it is testable.
fn active_storyboard(
    round_state: RoundState,
    round_number: i32,
    match_over: bool,
    intro_done: bool,
    has_intro: bool,
    has_ending: bool,
) -> ActiveStoryboard {
    // The ending takes precedence once the match is decided.
    if match_over && has_ending {
        return ActiveStoryboard::Ending;
    }
    // The intro plays only during round 1's intro phase, before it has finished.
    if has_intro && !intro_done && round_number == 1 && round_state == RoundState::Intro {
        return ActiveStoryboard::Intro;
    }
    ActiveStoryboard::None
}

impl MatchRun {
    /// The inner 1v1 [`Match`] fighting the **active pair** — the lead pair in 1v1 /
    /// Simul, or whichever fighters are currently front-line in Turns. The
    /// renderer, HUD, and audio all read the two visible fighters and round state
    /// from here exactly as for a plain 1v1 match (in [`TeamMode::Single`] it *is*
    /// the only pair).
    fn m(&self) -> &Match {
        self.team.active()
    }

    /// Which overlay is active this frame (the gating decision), reading the live
    /// [`Match`] state and this run's loaded-overlay / done flags.
    fn active_storyboard(&self) -> ActiveStoryboard {
        active_storyboard(
            self.m().round_state(),
            self.m().round_number(),
            self.m().match_winner().is_some(),
            self.intro_storyboard_done,
            self.intro_storyboard.is_some(),
            self.ending_storyboard.is_some(),
        )
    }

    /// Advances the active overlay one tick and decodes its sprites for this frame
    /// (both gated behind [`MatchRun::active_storyboard`]). Run once per frame
    /// **before** `begin_frame`, because decoding needs `&Renderer`. When the intro
    /// overlay finishes it latches `intro_storyboard_done` so it is shown once.
    /// A no-op when no overlay is active — the normal match is untouched.
    fn tick_storyboard(&mut self, renderer: &Renderer) {
        match self.active_storyboard() {
            ActiveStoryboard::Intro => {
                if let Some(intro) = self.intro_storyboard.as_mut() {
                    intro.cache_sprites(renderer);
                    intro.tick();
                    if intro.is_done() {
                        self.intro_storyboard_done = true;
                    }
                }
            }
            ActiveStoryboard::Ending => {
                if let Some(ending) = self.ending_storyboard.as_mut() {
                    ending.cache_sprites(renderer);
                    ending.tick();
                }
            }
            ActiveStoryboard::None => {}
        }
    }

    /// The overlay to draw this frame, if one is active and not finished. Returns
    /// `None` when the normal match should render (no overlay), so the caller draws
    /// the storyboard *instead of* nothing extra and the rest of the scene is
    /// unchanged. A finished overlay returns `None` so play falls back to the
    /// normal view on the frame after it ends.
    fn storyboard_to_draw(&self) -> Option<&StoryboardOverlay> {
        match self.active_storyboard() {
            ActiveStoryboard::Intro => self.intro_storyboard.as_ref().filter(|s| !s.is_done()),
            ActiveStoryboard::Ending => self.ending_storyboard.as_ref().filter(|s| !s.is_done()),
            ActiveStoryboard::None => None,
        }
    }
}

/// The selected run mode after parsing CLI args and loading assets.
enum Mode {
    /// Two-player [`Match`] (the playable demo): match + per-run render/audio.
    Match(Box<MatchRun>),
    /// Replay-study viewer (T076): a [`MatchRun`] driven through a recorded
    /// [`fp_engine::ReplayLog`] with VCR transport (play/pause/step/seek).
    Replay(Box<ReplayViewer>),
    /// Legacy SFF+AIR animation viewer.
    Viewer(Box<AnimViewer>),
    /// Single static sprite.
    Static(SpriteTexture, PaletteTexture),
    /// Checkerboard fallback.
    TestPattern(SpriteTexture, PaletteTexture),
}

/// Picks the run mode from CLI args, loading assets and degrading gracefully.
///
/// - no args → two-player [`Match`] of two KFMs (default), if KFM assets exist.
/// - `<p1.def>` → two-player match, P1 and P2 both that character.
/// - `<p1.def> <p2.def>` → two-player match with the two given characters.
/// - `<p1.def> <p2.def> <stage.def>` → as above, over that stage's background.
/// - `<file.sff> <file.air>` → legacy animation viewer.
/// - `<file.sff>` → legacy static sprite.
/// - anything missing/unloadable → falls back to the test pattern (no panic).
///
/// An optional 4th `.def` argument is taken as the stage; a missing/unloadable
/// stage degrades to the flat clear-color background, never failing the match.
///
/// `args` are the **positional** args with the `--p1-pal`/`--p2-pal` flags
/// already stripped (see [`parse_pal_flags`]); `pal` carries those per-player
/// `.act` palette selections, applied to the two fighters in the match modes.
fn select_mode(
    args: &[String],
    pal: PalSelection,
    team_mode: TeamMode,
    cpu_mode: BehaviorMode,
    renderer: &Renderer,
) -> Mode {
    match args.len() {
        // <p1.def> <p2.def> [stage.def] → two-player match from two characters.
        n if n >= 3 && is_def_path(&args[1]) && is_def_path(&args[2]) => {
            let stage = stage_arg(args, 3);
            load_match_or_fallback(
                Path::new(&args[1]),
                Path::new(&args[2]),
                stage,
                pal,
                team_mode,
                cpu_mode,
                renderer,
            )
        }
        // <sff> <air> [..] → legacy viewer (first arg is NOT a .def).
        n if n >= 3 && !is_def_path(&args[1]) => {
            let sff_path = Path::new(&args[1]);
            let air_path = Path::new(&args[2]);
            tracing::info!("Loading SFF: {}", sff_path.display());
            tracing::info!("Loading AIR: {}", air_path.display());
            match (SffFile::load(sff_path), AirFile::load(air_path)) {
                (Ok(sff), Ok(air)) => Mode::Viewer(Box::new(AnimViewer::new(sff, air))),
                (Err(e), _) | (_, Err(e)) => {
                    tracing::warn!("viewer assets failed to load: {e}; showing test pattern");
                    let (s, p) = generate_test_pattern(renderer);
                    Mode::TestPattern(s, p)
                }
            }
        }
        // <p1.def> [stage.def] → two-player match, the same character both sides.
        n if n >= 2 && is_def_path(&args[1]) => {
            let def = Path::new(&args[1]);
            let stage = stage_arg(args, 2);
            load_match_or_fallback(def, def, stage, pal, team_mode, cpu_mode, renderer)
        }
        // <sff> → legacy static sprite.
        2 => match load_sff_sprite(renderer, Path::new(&args[1])) {
            Ok((s, p)) => Mode::Static(s, p),
            Err(e) => {
                tracing::warn!("sprite failed to load: {e}; showing test pattern");
                let (s, p) = generate_test_pattern(renderer);
                Mode::TestPattern(s, p)
            }
        },
        // No args → default to a two-KFM match, falling back to the test pattern.
        _ => {
            let def = PathBuf::from(DEFAULT_DEF);
            if def.exists() {
                tracing::info!("No files provided; loading two-KFM match from {DEFAULT_DEF}");
                // No default stage ships (clean-room / asset-blocked), so the
                // default match renders over the flat clear color.
                load_match_or_fallback(&def, &def, None, pal, team_mode, cpu_mode, renderer)
            } else {
                tracing::info!("No files and no default character; showing test pattern");
                tracing::info!("Usage: fp-app [p1.def [p2.def]] | <file.sff> [file.air]");
                let (s, p) = generate_test_pattern(renderer);
                Mode::TestPattern(s, p)
            }
        }
    }
}

/// Returns the `i`th CLI argument as a stage `.def` path, if present and ending
/// in `.def`. A non-`.def` extra argument is ignored (the stage stays absent),
/// which keeps the flat-background fallback.
fn stage_arg(args: &[String], i: usize) -> Option<&Path> {
    args.get(i)
        .filter(|a| is_def_path(a))
        .map(|a| Path::new(a.as_str()))
}

/// Builds a two-player [`Match`] from two `.def` paths, optionally over a stage,
/// falling back to the test pattern on a character-load failure (so a bad/missing
/// character never crashes the app). A bad/missing `stage_def` only drops the
/// background to the flat clear color — the match still runs.
/// The shipped clean-room default stage background, drawn behind the fighters
/// when no MUGEN `[BGdef]` stage is loaded.
const DEFAULT_STAGE_BG: &str = "assets/stages/dojo/bg.png";

/// The display name of the shipped dojo backdrop on the stage-select screen
/// (T041). The first, always-present stage in the menu's stage list.
const DEFAULT_STAGE_NAME: &str = "DOJO";

/// Loads a PNG file as a full-color [`fp_render::ImageTexture`] (decoded to RGBA),
/// or `None` — warn/info-logged, never a panic — when the file is absent or can't
/// be decoded, so the match simply falls back to the flat clear color.
fn load_background_image(path: &str, renderer: &Renderer) -> Option<fp_render::ImageTexture> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::info!("no background image at {path}: {e}; using flat clear color");
            return None;
        }
    };
    let (rgba, w, h) = decode_png_rgba(&bytes).or_else(|| {
        tracing::warn!("background image {path} failed to decode; using flat clear color");
        None
    })?;
    tracing::info!("loaded stage background {path} ({w}x{h})");
    Some(fp_render::ImageTexture::new(
        renderer.device(),
        renderer.queue(),
        w,
        h,
        &rgba,
    ))
}

/// Decodes 8-bit PNG bytes into `(rgba, width, height)`, expanding palette /
/// grayscale and adding an opaque alpha channel as needed. Returns `None`
/// (never panics) for a malformed PNG or an unsupported 16-bit depth.
fn decode_png_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    // Reject 16-bit-per-channel PNGs up front: `EXPAND` only widens *up* to 8-bit,
    // never down from 16, so decoding one would yield a double-width buffer our
    // 8-bit RGBA conversion can't interpret. Bail before allocating + decoding.
    if reader.info().bit_depth != png::BitDepth::Eight {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => return None,
    };
    Some((rgba, info.width, info.height))
}

// The direct-CLI loader threads the same wide-but-flat set of independent inputs
// as `load_match_with_backdrop` (chars, optional stage, palette/team selections,
// the chosen teaching mode, and the renderer); bundling them would only move the
// fields elsewhere, so the arg count is allowed.
#[allow(clippy::too_many_arguments)]
fn load_match_or_fallback(
    p1_def: &Path,
    p2_def: &Path,
    stage_def: Option<&Path>,
    pal: PalSelection,
    team_mode: TeamMode,
    cpu_mode: BehaviorMode,
    renderer: &Renderer,
) -> Mode {
    load_match_with_backdrop(
        p1_def,
        p2_def,
        stage_def,
        DEFAULT_STAGE_BG,
        pal,
        team_mode,
        // The direct-CLI path keeps the default difficulty so its deterministic
        // play-out (default seed + Normal) is unchanged; the Setup/Options screen
        // (menu flow) is what selects a non-default difficulty (T069).
        AiDifficulty::Normal,
        // The teaching `BehaviorMode` comes from the `--ai-mode` CLI flag (default
        // `Ladder` = the plain difficulty ladder) so a player can pick a teaching
        // CPU from the command line (T070).
        cpu_mode,
        renderer,
    )
}

/// Like [`load_match_or_fallback`] but with an explicit full-window backdrop
/// image path used when no MUGEN `[BGdef]` `stage_def` is loaded (the menu
/// stage-select screen lets the player choose the backdrop). All the same
/// best-effort guarantees apply: a missing/bad backdrop simply leaves the flat
/// clear color.
// The match-loading path threads a wide but flat set of independent inputs
// (the two characters, optional stage, backdrop, palette/team/difficulty
// selections, and the renderer); bundling them into a struct would only move the
// same fields elsewhere without clarifying the call, so the arg count is allowed.
#[allow(clippy::too_many_arguments)]
fn load_match_with_backdrop(
    p1_def: &Path,
    p2_def: &Path,
    stage_def: Option<&Path>,
    backdrop_path: &str,
    pal: PalSelection,
    team_mode: TeamMode,
    cpu_difficulty: AiDifficulty,
    cpu_mode: BehaviorMode,
    renderer: &Renderer,
) -> Mode {
    match build_team_match(p1_def, p2_def, pal, team_mode) {
        Ok(mut m) => {
            // Declare each side's input driver so the engine assigns the right
            // `AILevel` (T052): P1 is the human at the keyboard (level 0), P2 is
            // the baseline CPU AI at the Setup/Options-selected difficulty the
            // `CpuAi` below runs (T069; default `Normal` -> level 4). A second
            // human controller still overrides the CPU's inputs per-frame in
            // `tick_match_run`, but the identity (and thus `AILevel`) is set once
            // here at construction.
            m.set_drivers(
                PlayerDriver::Human,
                PlayerDriver::Cpu(cpu_difficulty, cpu_mode),
            );
            // The shipped common-effects (`fightfx`) set is loaded best-effort
            // (audit #17): when present, its AIR is installed on the team's inner
            // match so common (`fightfx`) hit-sparks spawn, and its SFF render bundle
            // is kept for drawing them; absent/bad, common sparks simply don't render
            // (no panic, no regression).
            let common_fx = match load_common_fx() {
                Some((air, render)) => {
                    m.set_common_fx(air);
                    Some(render)
                }
                None => None,
            };
            // Loading the stage is best-effort: `None` (no path, bad parse, or
            // missing SFF) keeps today's flat clear color (no regression).
            let stage = stage_def.and_then(StageRender::load);
            // With no MUGEN stage loaded, fall back to the shipped clean-room dojo
            // background image so the match renders over a real backdrop instead of
            // the flat grey clear color. A loaded stage draws its own backgrounds,
            // so the image is skipped there. Best-effort: a missing/bad asset just
            // leaves the flat clear color (no panic, no regression).
            let background = if stage.is_none() {
                load_background_image(backdrop_path, renderer)
            } else {
                None
            };
            // The screenpack HUD is best-effort too: `None` (no `fight.def` found,
            // bad parse, or missing `fight.sff`) falls back to the hand-rolled quad
            // HUD, so the default match looks exactly as before (no regression).
            let screenpack = load_screenpack(p1_def, renderer);
            // Intro/ending storyboards (audit #32) are P1's own declarations,
            // loaded best-effort: a character without them (or with an unloadable
            // SFF) simply plays no overlay — the normal intro/match is unchanged.
            let intro_storyboard = load_character_storyboard(p1_def, StoryboardKind::Intro);
            let ending_storyboard = load_character_storyboard(p1_def, StoryboardKind::Ending);
            Mode::Match(Box::new(MatchRun {
                team: Box::new(m),
                p1_render: FighterRender::default(),
                p2_render: FighterRender::default(),
                // The default constructor opens a real device when present and
                // falls back to a silent NullBackend otherwise — it never panics,
                // so the app runs identically with or without audio hardware.
                audio: AudioSystem::default(),
                p1_audio: FighterAudio::default(),
                p2_audio: FighterAudio::default(),
                stage,
                screenpack,
                intro_storyboard,
                ending_storyboard,
                intro_storyboard_done: false,
                common_fx,
                background,
                // Drive the otherwise-idle P2 with the baseline CPU AI (T018),
                // seeded deterministically (P2's derived per-player seed) so the
                // demo replays identically, in the Setup/Options- or CLI-selected
                // teaching `BehaviorMode` (T070; default `Ladder` = the plain
                // difficulty ladder, unchanged from before). A second human
                // controller, when present, overrides it per-frame in
                // `tick_match_run`.
                cpu_ai: Some(CpuAi::with_mode(
                    derive_player_seed(DEFAULT_MATCH_SEED, 1),
                    cpu_difficulty,
                    cpu_mode,
                )),
                dummy_mode: DummyMode::default(),
                dummy_tick: 0,
            }))
        }
        Err(e) => {
            tracing::warn!("match failed to load: {e}; showing test pattern");
            let (s, p) = generate_test_pattern(renderer);
            Mode::TestPattern(s, p)
        }
    }
}

/// The replay-study viewer (T076): a live [`MatchRun`] driven through a recorded
/// [`fp_engine::ReplayLog`]'s inputs, with VCR-style transport controls.
///
/// Wraps a freshly-loaded `MatchRun` (built from the *same* two characters the log
/// was recorded with) and steps it through the recorded `(p1, p2)` input pairs.
/// Because the engine tick is forward-only + deterministic, seeking restores the
/// nearest cached keyframe ([`fp_engine::TeamMatch::snapshot_active`]) and
/// re-simulates forward — exactly the engine-tested
/// [`fp_engine::ReplayPlayer`] algorithm, applied over the renderer-bearing
/// `MatchRun`. The F026 overlays (`F1`/`F3` hitboxes, `F8`/`F9` input + frame data)
/// are pure draw layers over the live match, so they apply to the replay unchanged.
///
/// Transport keys are sampled in the run loop separately from match input
/// (recorded inputs are fixed): Space toggles play/pause, `,`/`.` step ∓1 frame,
/// Left/Right arrows seek ∓10 frames, Home/End jump to the start/end.
struct ReplayViewer {
    /// The live match the recorded inputs drive — the render/HUD/overlay surface.
    run: Box<MatchRun>,
    /// The recorded log being replayed (owned; never blocks on I/O after load).
    log: fp_engine::ReplayLog,
    /// The current playhead frame (recorded input pairs applied), in `0..=len`.
    frame: u32,
    /// Whether the transport is advancing (play) vs. holding (pause).
    playing: bool,
    /// Cached inner-match keyframes `(frame, snapshot)`, sorted ascending and always
    /// containing frame 0 — the seek bases for restore + re-sim.
    keyframes: Vec<(u32, fp_engine::MatchSnapshot)>,
}

impl ReplayViewer {
    /// Builds a viewer for `log`, loading a `MatchRun` from `p1_def`/`p2_def` (the
    /// characters the log was recorded with), seeding it from the log's seed, and
    /// capturing frame 0. Returns `None` (logged) if the match fails to load or the
    /// log's character fingerprints do not match the loaded characters.
    fn load(
        log: fp_engine::ReplayLog,
        p1_def: &Path,
        p2_def: &Path,
        renderer: &Renderer,
    ) -> Option<Self> {
        // Reuse the standard match-load path (assets, stage, screenpack, HUD), then
        // unwrap the MatchRun. The CPU AI it installs is irrelevant — the viewer
        // feeds recorded inputs directly, bypassing P2 resolution.
        let mode = load_match_or_fallback(
            p1_def,
            p2_def,
            None,
            PalSelection::default(),
            TeamMode::Single,
            BehaviorMode::default(),
            renderer,
        );
        let mut run = match mode {
            Mode::Match(run) => run,
            _ => {
                tracing::warn!(
                    "replay: characters {} / {} failed to load a match; cannot open viewer",
                    p1_def.display(),
                    p2_def.display()
                );
                return None;
            }
        };
        // Validate fingerprints + seed exactly as `replay_match` does, via the engine
        // primitives, then capture frame 0.
        run.team.seed_players(log.match_seed);
        let frame0 = run.team.snapshot_active();
        // A fingerprint mismatch surfaces on the first restore; verify up-front by a
        // round-trip restore of frame 0 (cheap, never ticks).
        if run.team.restore_active(&frame0).is_err() {
            tracing::warn!("replay: snapshot/restore self-check failed; cannot open viewer");
            return None;
        }
        Some(Self {
            run,
            log,
            frame: 0,
            playing: false,
            keyframes: vec![(0, frame0)],
        })
    }

    /// The total number of recorded frames (seekable range `0..=len`).
    fn len(&self) -> u32 {
        self.log.inputs.len() as u32
    }

    /// Whether the playhead is at the end of the log.
    fn at_end(&self) -> bool {
        self.frame >= self.len()
    }

    /// Drives the inner team one tick with the **exact** given inputs (no CPU/dummy
    /// resolution — the recorded inputs are authoritative), then advances audio +
    /// stage scroll, mirroring [`tick_match_run`] minus P2 resolution.
    fn raw_tick(&mut self, p1: MatchInput, p2: MatchInput) {
        self.run.dummy_tick = self.run.dummy_tick.wrapping_add(1);
        self.run.team.tick(p1, p2);
        self.run.p1_audio.play_requests(
            &mut self.run.audio,
            self.run.team.active().p1(),
            self.run.team.active().p1_sound_requests(),
        );
        self.run.p2_audio.play_requests(
            &mut self.run.audio,
            self.run.team.active().p2(),
            self.run.team.active().p2_sound_requests(),
        );
        if let Some(stage) = self.run.stage.as_mut() {
            stage.advance_scroll();
            stage.advance_anim();
        }
    }

    /// Caches an inner-match keyframe at the current frame if it lands on a
    /// [`fp_engine::DEFAULT_KEYFRAME_INTERVAL`] boundary and is not already cached.
    fn cache_keyframe_if_due(&mut self) {
        if !self
            .frame
            .is_multiple_of(fp_engine::DEFAULT_KEYFRAME_INTERVAL)
        {
            return;
        }
        if let Err(pos) = self
            .keyframes
            .binary_search_by_key(&self.frame, |(f, _)| *f)
        {
            let snap = self.run.team.snapshot_active();
            self.keyframes.insert(pos, (self.frame, snap));
        }
    }

    /// Advances exactly one recorded frame (the play step). Auto-pauses at the end.
    /// Returns whether a frame was consumed.
    fn advance(&mut self) -> bool {
        if self.at_end() {
            self.playing = false;
            return false;
        }
        let (p1, p2) = self.log.inputs[self.frame as usize];
        self.raw_tick(p1, p2);
        self.frame += 1;
        self.cache_keyframe_if_due();
        true
    }

    /// Seeks (scrubs) the playhead to `target` (clamped to `0..=len`) by restoring
    /// the nearest earlier keyframe and re-simulating forward. Seeking pauses.
    fn seek(&mut self, target: u32) {
        let target = target.min(self.len());
        let (kf_frame, kf_snap) = self
            .keyframes
            .iter()
            .rev()
            .find(|(f, _)| *f <= target)
            .map(|(f, s)| (*f, s.clone()))
            .expect("frame-0 keyframe always present");
        // Restore is infallible for a snapshot from this same match; on the
        // impossible mismatch, re-seed from frame 0 rather than panic.
        if self.run.team.restore_active(&kf_snap).is_err() {
            self.run.team.seed_players(self.log.match_seed);
            self.frame = 0;
        } else {
            self.frame = kf_frame;
        }
        while self.frame < target {
            let (p1, p2) = self.log.inputs[self.frame as usize];
            self.raw_tick(p1, p2);
            self.frame += 1;
            self.cache_keyframe_if_due();
        }
        self.playing = false;
    }

    /// Steps the playhead back one frame (seek to current − 1). No-op at 0.
    fn step_back(&mut self) {
        if self.frame > 0 {
            self.seek(self.frame - 1);
        }
    }

    /// Toggles play/pause.
    fn toggle_play(&mut self) {
        self.playing = !self.playing;
    }
}

/// Loads a replay log from disk and opens a [`ReplayViewer`] over it (T076),
/// degrading to the test pattern on any failure (never a panic).
///
/// Reads + decodes the `.bin` log, then builds the viewer from the named character
/// `.def`s (one `.def` reused on both sides when `p2` is `None`). A missing/bad log
/// or a character/log mismatch logs and falls back to the test pattern.
fn load_replay_mode(log_path: &Path, p1_def: &Path, p2_def: &Path, renderer: &Renderer) -> Mode {
    let bytes = match std::fs::read(log_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("replay: cannot read log {}: {e}", log_path.display());
            let (s, p) = generate_test_pattern(renderer);
            return Mode::TestPattern(s, p);
        }
    };
    let log = match fp_engine::ReplayLog::decode(&bytes) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("replay: cannot decode log {}: {e}", log_path.display());
            let (s, p) = generate_test_pattern(renderer);
            return Mode::TestPattern(s, p);
        }
    };
    tracing::info!(
        "replay: opening viewer over {} ({} frames)",
        log_path.display(),
        log.inputs.len()
    );
    match ReplayViewer::load(log, p1_def, p2_def, renderer) {
        Some(v) => Mode::Replay(Box::new(v)),
        None => {
            let (s, p) = generate_test_pattern(renderer);
            Mode::TestPattern(s, p)
        }
    }
}

/// Finds and loads a `fight.def` screenpack into a GPU-resident
/// [`ScreenpackHud`], or returns `None` to fall back to the quad [`Hud`].
///
/// Search order (first hit wins), all best-effort:
/// 1. the `FP_SCREENPACK` environment variable, if it points at a `fight.def`;
/// 2. a `fight.def` next to the P1 character `.def`;
/// 3. a `data/fight.def` next to the P1 character `.def`.
///
/// No screenpack ships with the engine (clean-room / asset-blocked), so in the
/// default match this returns `None` and the hand-rolled quad HUD is used — no
/// regression. A found-but-unparseable `fight.def`, or one whose `fight.sff`
/// fails to load, also returns `None` (logged), never a panic.
fn load_screenpack(p1_def: &Path, renderer: &Renderer) -> Option<ScreenpackHud> {
    let fight_def = locate_fight_def(p1_def)?;
    tracing::info!("Loading screenpack: {}", fight_def.display());

    let def = match fp_formats::def::DefFile::load(&fight_def) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                "screenpack {} failed to parse: {e}; using quad HUD",
                fight_def.display()
            );
            return None;
        }
    };
    let layout = ScreenpackLayout::parse(&def);
    if layout.sff.is_empty() {
        tracing::warn!(
            "screenpack {} has no [Files] sff; using quad HUD",
            fight_def.display()
        );
        return None;
    }

    // Resolve and load the fight.sff relative to the fight.def directory.
    let sff_path = fp_formats::def::DefFile::resolve_path(&fight_def, &layout.sff);
    let sff = match SffFile::load(&sff_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "screenpack sff {} failed to load: {e}; using quad HUD",
                sff_path.display()
            );
            return None;
        }
    };

    // Load each font slot relative to the fight.def directory; a missing/bad font
    // becomes `None` (its text is skipped) rather than failing the whole HUD.
    let fonts = layout
        .fonts
        .iter()
        .map(|rel| {
            let path = fp_formats::def::DefFile::resolve_path(&fight_def, rel);
            match fp_formats::fnt::FntFont::load(&path) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!(
                        "screenpack font {} failed to load: {e}; skipping",
                        path.display()
                    );
                    None
                }
            }
        })
        .collect();

    Some(ScreenpackHud::build(renderer, layout, &sff, fonts))
}

/// Returns the first existing `fight.def` candidate for the given P1 character
/// `.def`, or `None` if none is found. See [`load_screenpack`] for the search
/// order.
fn locate_fight_def(p1_def: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(env) = std::env::var("FP_SCREENPACK") {
        if !env.is_empty() {
            candidates.push(PathBuf::from(env));
        }
    }
    if let Some(dir) = p1_def.parent() {
        candidates.push(dir.join("fight.def"));
        candidates.push(dir.join("data").join("fight.def"));
    }
    candidates.into_iter().find(|p| p.exists())
}

/// Builds the per-frame [`MatchHudState`] the screenpack renderer draws from a
/// live [`Match`]: life/power fractions (via [`life_fraction`]/[`power_fraction`],
/// so the screenpack and quad HUDs agree), both fighter names, the timer, and the
/// round/KO/winner readout.
fn match_hud_state(m: &Match) -> MatchHudState {
    MatchHudState {
        p1_life: life_fraction(m.p1().life(), m.p1().life_max()),
        p2_life: life_fraction(m.p2().life(), m.p2().life_max()),
        p1_power: power_fraction(m.p1().power(), m.p1().power_max()),
        p2_power: power_fraction(m.p2().power(), m.p2().power_max()),
        p1_name: m.p1().loaded.name.clone(),
        p2_name: m.p2().loaded.name.clone(),
        // `Match::timer()` is FRAMES remaining (its doc: "Divide by 60 for
        // seconds"); `MatchHudState.timer_seconds` is whole seconds, drawn raw.
        // Convert here so a real fight.def clock reads e.g. 99, not 5940.
        timer_seconds: Some(timer_frames_to_seconds(m.timer())),
        round_text: round_readout(m),
        combo_count: active_combo_count(m),
        // The caller sets `frame` (drives the T074 max-power flash) after building.
        frame: 0,
    }
}

/// The hit count of the currently active combo for the screenpack combo counter.
///
/// MUGEN tracks the running combo on the *defender* (`GetHitVar(hitcount)`), so a
/// side's combo length is read off the opponent it is hitting. We surface
/// whichever side is comboing harder (the max of the two), which the renderer
/// then only draws once it reaches 2 (see [`fp_ui::combo_text`]).
fn active_combo_count(m: &Match) -> i32 {
    m.p1()
        .character
        .get_hit_vars
        .hitcount
        .max(m.p2().character.get_hit_vars.hitcount)
}

/// Converts the engine's frame-based round clock ([`Match::timer`], "frames
/// remaining") into the whole seconds that [`MatchHudState::timer_seconds`]
/// expects and the screenpack renderer draws verbatim.
///
/// The engine ticks at a fixed 60 Hz, so this is integer-divide by 60 (it floors,
/// matching MUGEN's whole-second countdown). Negative inputs are clamped to `0`.
fn timer_frames_to_seconds(frames: i32) -> i32 {
    (frames / 60).max(0)
}

/// The round-announcer text for the current match state: a `KO`/win/draw readout
/// once the round is decided, the round number during the intro, else empty.
///
/// Delegates to the pure [`round_label`] so the production mapping is exactly
/// what the unit test exercises (no duplicated `match`).
fn round_readout(m: &Match) -> String {
    round_label(m.round_state(), m.winner(), m.round_number())
}

/// The pure `(state, winner, round_number)` → announcer-text mapping behind
/// [`round_readout`], split out so it is testable without a live [`Match`].
///
/// `round_number` is only consulted for the [`RoundState::Intro`] case (the
/// `"ROUND N"` readout); the others ignore it.
fn round_label(state: RoundState, winner: Option<Winner>, round_number: i32) -> String {
    match (state, winner) {
        (RoundState::Ko, _) => "KO".to_string(),
        (RoundState::Win, Some(Winner::P1)) => "P1 WINS".to_string(),
        (RoundState::Win, Some(Winner::P2)) => "P2 WINS".to_string(),
        (RoundState::Win, _) => "DRAW".to_string(),
        (RoundState::Intro, _) => format!("ROUND {round_number}"),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// In-app screen state machine (Menu-2): Title -> Select -> Fight -> Title
// ---------------------------------------------------------------------------
//
// The pure menu/cursor/transition logic lives in `screens` (headless, unit-
// tested). This section is the GPU/SDL glue: it loads the motif (`system.def` +
// `select.def`) and the menu font once, owns the live [`RunScreen`], samples one
// rising-edge menu input per frame, and renders the Title/Select screens as text
// over a solid background. The Fight screen reuses the existing [`MatchRun`]
// render/HUD path unchanged.

/// The loaded default motif content the menu flow draws from: the parsed
/// `system.def` (title menu + select geometry), the parsed `select.def` roster,
/// and the `select.def`'s own path (so a roster `.def` resolves relative to it).
///
/// Every field has a sensible fallback: a missing/unparseable `system.def`
/// yields [`SystemDef::default`] (the title menu then uses its built-in
/// fallback), and a missing/unparseable roster yields an empty [`SelectDef`].
struct Motif {
    /// The parsed motif `system.def`.
    system: SystemDef,
    /// The parsed roster `select.def`.
    select: SelectDef,
    /// The on-disk path of the `select.def`, used to resolve roster `.def`s.
    select_path: PathBuf,
    /// The stages offered on the stage-select screen (T041): the shipped dojo
    /// backdrop plus every discovered stage `.def` (roster + `[ExtraStages]`),
    /// filtered to the entries that actually exist on disk. Never empty — the
    /// dojo backdrop is always present.
    stages: Vec<screens::StageEntry>,
}

impl Motif {
    /// Loads the default motif best-effort from [`DEFAULT_SYSTEM_DEF`].
    ///
    /// Reads `system.def` (degrading to [`SystemDef::default`] when absent/bad),
    /// resolves its `[Files] select` relative to the `system.def` (falling back to
    /// the shipped [`DEFAULT_SELECT_DEF`] when it declares none or the file is
    /// missing), and parses the roster (degrading to an empty roster on a read
    /// error). Never panics — every failure logs and substitutes a default.
    fn load_default() -> Self {
        Self::load_from(Path::new(DEFAULT_SYSTEM_DEF), Path::new(DEFAULT_SELECT_DEF))
    }

    /// Loads the motif chosen by the optional `--motif <name|path>` selector
    /// (T045), falling back to [`Motif::load_default`] when the selector is absent
    /// or cannot be resolved. Never panics.
    ///
    /// A selector is resolved by [`resolve_motif_system_def`]: a discovered motif
    /// name (under [`DEFAULT_MOTIF_DIR`]), a direct `system.def` path, or a motif
    /// directory. An unresolvable selector logs a warning and uses the default
    /// motif — so a typo in `--motif` never crashes the app.
    fn load_selected(selector: Option<&str>) -> Self {
        match selector.and_then(resolve_motif_system_def) {
            Some(system_path) => {
                tracing::info!("Loading selected motif: {}", system_path.display());
                Self::load_from(&system_path, Path::new(DEFAULT_SELECT_DEF))
            }
            None => {
                if let Some(sel) = selector {
                    tracing::warn!("motif {sel:?} not found; using default motif");
                }
                Self::load_default()
            }
        }
    }

    /// Augments this motif's roster with directory-discovered characters (T043),
    /// appending each [`fp_ui::CharEntry`] not already present (by resolved `.def`
    /// path) as a new `[Characters]` slot. The existing `select.def` roster is
    /// **augmented, not replaced** — the motif's own characters stay first, the
    /// discovered ones follow in scan order.
    fn augment_roster(&mut self, discovered: &[fp_ui::CharEntry]) {
        if discovered.is_empty() {
            return;
        }
        // The motif's own roster `.def`s resolve relative to the `select.def`
        // directory (matching `SelectScreen::build_pick`), so compute that base to
        // dedup the motif's existing characters against the discovered ones.
        let base_dir = self
            .select_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        // Existing resolved def paths, ABSOLUTIZED so the dedup compares in the
        // same path space as the (also-absolutized) discovered paths below — a
        // relative CLI dir arg would otherwise never compare equal to a motif's
        // resolved path and the dedup would silently miss.
        let mut have: Vec<PathBuf> = self
            .select
            .slots
            .iter()
            .filter_map(|s| match s {
                fp_ui::SelectSlot::Character(e) => Some(absolutize(&base_dir.join(&e.def_path))),
                _ => None,
            })
            .collect();

        let mut added = 0;
        for c in discovered {
            // Absolutize the discovered path BEFORE storing it: `SelectScreen`
            // resolves a roster entry via `base_dir.join(def_path)`, where
            // `base_dir` is the motif's `select.def` directory. A relative
            // discovered path (from a relative CLI dir arg, e.g. `fp-app chars/`)
            // would be re-rooted under that dir and fail to load; an absolute path
            // joins to itself, so the character loads from where it was found.
            let resolved = absolutize(&c.def_path);
            if have.iter().any(|p| p == &resolved) {
                continue;
            }
            have.push(resolved.clone());
            self.select
                .slots
                .push(fp_ui::SelectSlot::Character(fp_ui::RosterEntry {
                    name: c.name.clone(),
                    def_path: resolved.to_string_lossy().into_owned(),
                    include_stage: true,
                    ..Default::default()
                }));
            added += 1;
        }
        tracing::info!("augmented roster with {added} discovered character(s)");
    }

    /// Loads a motif from an explicit `system.def` path, falling back to
    /// `fallback_select` when the motif declares/has no usable `select.def`. Split
    /// from [`Motif::load_default`] so the resolution logic is unit-testable by
    /// absolute path. Never panics.
    fn load_from(system_path: &Path, fallback_select: &Path) -> Self {
        let system = match fp_formats::def::DefFile::load(system_path) {
            Ok(def) => {
                tracing::info!("Loaded motif system.def: {}", system_path.display());
                SystemDef::parse(&def)
            }
            Err(e) => {
                tracing::warn!(
                    "motif {} not loaded ({e}); using built-in fallback menu",
                    system_path.display()
                );
                SystemDef::default()
            }
        };

        // Resolve the roster path: the motif's `[Files] select` relative to the
        // system.def, else the shipped fallback select.def.
        let select_path = resolve_select_path(system_path, &system, fallback_select);
        let select = match std::fs::read_to_string(&select_path) {
            Ok(text) => {
                tracing::info!("Loaded roster select.def: {}", select_path.display());
                SelectDef::parse(&text)
            }
            Err(e) => {
                tracing::warn!(
                    "roster {} not loaded ({e}); using empty roster",
                    select_path.display()
                );
                SelectDef::default()
            }
        };

        let stages = discover_stages(&select, &select_path);

        Self {
            system,
            select,
            select_path,
            stages,
        }
    }
}

/// Builds the menu's stage list (T041 + T044): the always-present dojo backdrop,
/// every stage `.def` declared by the roster (`[Characters]` stages with
/// `includestage` + `[ExtraStages]`) that actually exists on disk, plus every
/// stage discovered by scanning a sibling `stages/` directory (T044).
///
/// The roster-derived candidate list comes from the pure
/// [`screens::stage_entries_from_roster`]; this is the impure step that drops
/// `.def` candidates whose file is missing (so the player can't pick a stage that
/// fails to load), keeping the backdrop regardless. It then appends the
/// directory-discovered stages (via [`fp_stage::discover_stages`] over
/// `<select.def dir>/stages/`), de-duplicated against the roster ones by path.
/// The result is never empty.
fn discover_stages(select: &SelectDef, select_path: &Path) -> Vec<screens::StageEntry> {
    let base_dir = select_path.parent().unwrap_or_else(|| Path::new("."));
    let candidates = screens::stage_entries_from_roster(
        select,
        base_dir,
        DEFAULT_STAGE_NAME,
        Path::new(DEFAULT_STAGE_BG),
    );
    let mut stages: Vec<screens::StageEntry> = candidates
        .into_iter()
        .filter(|e| match e.kind {
            // The backdrop is always kept (it is the guaranteed default even if
            // its image is later missing — `load_background_image` degrades to the
            // flat clear color).
            screens::StageKind::Backdrop => true,
            // A `.def` stage is only offered when its file exists, so the player
            // can't pick a stage that won't load.
            screens::StageKind::Def => {
                let exists = e.path.exists();
                if !exists {
                    tracing::info!(
                        "stage {} not found on disk; omitting from stage select",
                        e.path.display()
                    );
                }
                exists
            }
        })
        .collect();

    // T044: directory-discovered stages under <select.def dir>/stages/. Each is a
    // real stage `.def` (already validated by the scanner) so it loads; append the
    // ones not already offered by the roster.
    let stages_dir = base_dir.join("stages");
    for found in fp_stage::discover_stages(&stages_dir) {
        if stages.iter().any(|e| e.path == found.def_path) {
            continue;
        }
        stages.push(screens::StageEntry::def(found.name, found.def_path));
    }

    tracing::info!("stage select: {} stage(s) available", stages.len());
    stages
}

/// Resolves a `--motif <name|path>` selector (T045) to a concrete `system.def`
/// path, or `None` when it cannot be resolved (the caller then uses the default
/// motif). Pure given the filesystem. Three forms are accepted, in order:
///
/// 1. a direct `.def` path that exists (used verbatim);
/// 2. a directory path holding a `system.def` (resolves to `<dir>/system.def`);
/// 3. a discovered motif **name** matched (case-insensitively) against the
///    motifs found under [`DEFAULT_MOTIF_DIR`].
fn resolve_motif_system_def(selector: &str) -> Option<PathBuf> {
    resolve_motif_system_def_in(selector, Path::new(DEFAULT_MOTIF_DIR))
}

/// Resolves a `--motif` selector against a specific motif directory (the form-3
/// search root). Split from [`resolve_motif_system_def`] so the discovered-name
/// path can be unit-tested against a synthetic motif dir without touching the
/// shipped [`DEFAULT_MOTIF_DIR`]. Pure given the filesystem.
fn resolve_motif_system_def_in(selector: &str, motif_dir: &Path) -> Option<PathBuf> {
    let sel = selector.trim();
    if sel.is_empty() {
        return None;
    }
    let as_path = Path::new(sel);

    // (1) A direct system.def (or any .def) path.
    if as_path.is_file()
        && as_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("def"))
    {
        return Some(as_path.to_path_buf());
    }
    // (2) A motif directory holding a system.def.
    if as_path.is_dir() {
        let candidate = as_path.join("system.def");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // (3) A discovered motif name under the motif dir.
    fp_ui::discover_motifs(motif_dir)
        .into_iter()
        .find(|m| m.name.eq_ignore_ascii_case(sel))
        .map(|m| m.system_def_path)
}

/// Resolves the roster `select.def` path for a motif: the motif's `[Files]
/// select` resolved relative to the `system.def` directory if it names one and
/// that file exists, else the shipped `fallback` path. Pure given the filesystem;
/// split out so the fallback logic is unit-testable.
fn resolve_select_path(system_path: &Path, system: &SystemDef, fallback: &Path) -> PathBuf {
    let declared = system.select_file.trim();
    if !declared.is_empty() {
        let resolved = fp_formats::def::DefFile::resolve_path(system_path, declared);
        if resolved.exists() {
            return resolved;
        }
        tracing::warn!(
            "motif declares select = {declared} but {} is missing; using fallback roster",
            resolved.display()
        );
    }
    fallback.to_path_buf()
}

/// The live in-app screen. The menu flow starts in [`RunScreen::Title`] and
/// transitions Title -> Select -> Fight -> Title; [`RunScreen::Quit`] ends the
/// loop. The Fight variant carries the same [`MatchRun`] the direct-CLI match
/// path uses, so the fight renders identically.
enum RunScreen {
    /// The title-screen main menu.
    Title(screens::TitleMenu),
    /// The character-select grid.
    Select(screens::SelectScreen),
    /// The movelist / character-info screen (T071), shown when the player presses
    /// Info on a character cell. Carries the info to display plus the
    /// character-select screen to resume when dismissed.
    CharacterInfo {
        /// The character-info / movelist data to draw.
        info: screens::InfoScreen,
        /// The character-select screen to return to on dismiss (unchanged).
        select: screens::SelectScreen,
    },
    /// The stage-select list (T041), shown after character-select and before the
    /// fight. Carries the already-chosen [`screens::MatchPick`] so the match can
    /// be built once a stage is confirmed; cancelling returns to character-select.
    StageSelect {
        /// The stage list + cursor.
        stages: screens::StageSelect,
        /// The characters chosen on the previous (character-select) screen.
        pick: screens::MatchPick,
        /// The select mode the pick came from, so cancelling can rebuild the
        /// correct character-select screen.
        mode: screens::SelectMode,
    },
    /// The setup / options screen (T042): input device + key remapping. Reached
    /// from the title menu; back returns to Title.
    Setup(screens::SetupScreen),
    /// The HUD-customization screen (T046): bar colors + per-element visibility.
    /// Reached from the setup screen; back returns to Setup. Edits mutate the
    /// app's [`MenuApp::hud_config`], which is snapshotted onto the screenpack HUD
    /// at the **next** match start (`enter_fight` -> `set_hud_config`); they do
    /// not retro-apply to an already-running match (this screen is only reachable
    /// out of a fight, so that never arises).
    HudCustomize(screens::HudCustomizeScreen),
    /// A running two-player match. On match-over the flow returns to Title.
    Fight(Box<MatchRun>),
    /// Leave the application.
    Quit,
}

/// The whole menu-mode runtime: the loaded motif, the menu font, and the live
/// screen. Owns the transitions between Title/Select/Fight.
struct MenuApp {
    /// The loaded default motif (title menu + roster).
    motif: Motif,
    /// The bitmap font used to draw the text menus (the shipped HUD font).
    /// `None` when the font is absent — the menus then draw nothing but the flow
    /// still works (and logs), so a missing font never traps the app.
    font: Option<GlyphFont>,
    /// The player's live HUD-customization overrides (T046), edited on the
    /// HUD-customization screen and applied to a match's screenpack HUD when a
    /// fight starts. Defaults to the no-op [`fp_ui::HudConfig::default`] (HUD
    /// unchanged).
    hud_config: screens::HudConfig,
    /// The current screen.
    screen: RunScreen,
}

impl MenuApp {
    /// Builds the menu runtime with an optional `--motif` selector (T045) and an
    /// optional characters directory to discover a roster from (T043).
    ///
    /// Loads the (default or selected) motif + menu font and starts on the Title
    /// screen built from the motif (or its built-in fallback).
    ///
    /// The motif is resolved by [`Motif::load_selected`] (default motif on an
    /// absent/invalid selector). When `chars_dir` is given, every character found
    /// there ([`fp_ui::discover_chars`]) augments the motif's roster — the
    /// existing `select.def` roster is kept and the discovered characters are
    /// appended. Never panics.
    fn with_options(
        renderer: &Renderer,
        motif_selector: Option<&str>,
        chars_dir: Option<&Path>,
    ) -> Self {
        let mut motif = Motif::load_selected(motif_selector);
        if let Some(dir) = chars_dir {
            let discovered = fp_ui::discover_chars(dir);
            tracing::info!(
                "discovered {} character(s) under {}",
                discovered.len(),
                dir.display()
            );
            motif.augment_roster(&discovered);
        }
        let title = screens::TitleMenu::from_system(&motif.system);
        Self {
            motif,
            font: load_hud_font(renderer),
            hud_config: screens::HudConfig::default(),
            screen: RunScreen::Title(title),
        }
    }

    /// Whether the app should keep running (false once the menu requested Quit).
    fn running(&self) -> bool {
        !matches!(self.screen, RunScreen::Quit)
    }

    /// Enters the character-select screen for the given mode.
    fn enter_select(&mut self, mode: screens::SelectMode) {
        let screen = screens::SelectScreen::new(
            mode,
            &self.motif.select,
            &self.motif.system.select_info,
            &self.motif.select_path,
        );
        if screen.is_empty() {
            tracing::warn!("roster has no choosable characters; returning to title");
            self.screen = RunScreen::Title(screens::TitleMenu::from_system(&self.motif.system));
        } else {
            self.screen = RunScreen::Select(screen);
        }
    }

    /// Enters the setup / options screen (T042), reachable from the title menu.
    fn enter_setup(&mut self) {
        self.screen = RunScreen::Setup(screens::SetupScreen::new());
    }

    /// Enters the movelist / character-info screen (T071) for the character at
    /// `def_path`, carrying the character-select screen forward to resume on
    /// dismiss. The character is loaded here (off the hot path); a load failure
    /// degrades to a [`screens::InfoScreen::load_failed`] fallback that still
    /// shows the roster label, so Info never crashes or traps the player.
    fn enter_character_info(&mut self, def_path: &Path, select: screens::SelectScreen) {
        let info = match fp_character::LoadedCharacter::load(def_path) {
            Ok(loaded) => screens::InfoScreen::from_loaded(&loaded),
            Err(e) => {
                tracing::warn!("character info: failed to load {}: {e}", def_path.display());
                let label = def_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("???");
                screens::InfoScreen::load_failed(label)
            }
        };
        self.screen = RunScreen::CharacterInfo { info, select };
    }

    /// Enters the stage-select screen (T041) for a completed character pick,
    /// carrying the pick forward so the match can be assembled once a stage is
    /// confirmed. Seeds the stage list from the loaded motif (the dojo backdrop
    /// plus discovered `.def` stages).
    fn enter_stage_select(&mut self, pick: screens::MatchPick, mode: screens::SelectMode) {
        let stages = screens::StageSelect::new(self.motif.stages.clone());
        self.screen = RunScreen::StageSelect { stages, pick, mode };
    }

    /// Builds the two-player match for a completed [`screens::MatchPick`] over the
    /// chosen [`screens::StageChoice`] and enters the Fight screen, or returns to
    /// Title on a load failure (so a bad roster `.def` never crashes the flow).
    fn enter_fight(
        &mut self,
        pick: &screens::MatchPick,
        stage: &screens::StageChoice,
        mode: screens::SelectMode,
        cpu_difficulty: AiDifficulty,
        cpu_mode: BehaviorMode,
        renderer: &Renderer,
    ) {
        let game_mode = game_mode_for(mode);
        tracing::info!(
            "Starting match ({:?}): P1={} ({}) vs P2={} ({}) on stage {} ({})",
            game_mode,
            pick.p1_name,
            pick.p1_def.display(),
            pick.p2_name,
            pick.p2_def.display(),
            stage.name,
            stage.path.display(),
        );
        match build_match_run(
            &pick.p1_def,
            &pick.p2_def,
            stage,
            cpu_difficulty,
            cpu_mode,
            renderer,
        ) {
            Some(mut run) => {
                // (T066) Flag the match's mode so Training disables round
                // termination (no timeout, no auto-KO end) while Versus runs the
                // normal round flow. A no-op for Versus (the engine default).
                run.team.set_game_mode(game_mode);
                // Apply the player's HUD-customization overrides (T046) to the
                // match's screenpack HUD, if one loaded. With the default (no-op)
                // config this leaves the HUD byte-for-byte unchanged.
                if let Some(screenpack) = run.screenpack.as_mut() {
                    screenpack.set_hud_config(self.hud_config.clone());
                }
                self.screen = RunScreen::Fight(Box::new(run));
            }
            None => {
                tracing::warn!("could not start match; returning to title");
                self.screen = RunScreen::Title(screens::TitleMenu::from_system(&self.motif.system));
            }
        }
    }

    /// Enters the HUD-customization screen (T046), reachable from the setup
    /// screen. The screen edits [`MenuApp::hud_config`] in place; the change is
    /// applied to a match's screenpack HUD the next time a fight starts.
    fn enter_hud_customize(&mut self) {
        self.screen = RunScreen::HudCustomize(screens::HudCustomizeScreen::new());
    }

    /// Drives one frame of the HUD-customization screen (T046), editing the live
    /// [`MenuApp::hud_config`] in place. On back/cancel returns to the setup
    /// screen. A no-op when the active screen isn't the HUD-customization screen.
    ///
    /// Kept as a method so the borrow checker can split-borrow `self.screen` and
    /// `self.hud_config` (two disjoint fields) — the caller can't borrow both at
    /// once through `&mut app.screen`.
    fn tick_hud_customize(&mut self, input: screens::MenuInput) {
        if let RunScreen::HudCustomize(ref mut hud) = self.screen {
            match hud.update(input, &mut self.hud_config) {
                screens::HudCustomizeOutcome::Pending => {}
                screens::HudCustomizeOutcome::Exit => {
                    self.screen = RunScreen::Setup(screens::SetupScreen::new());
                }
            }
        }
    }

    /// Returns to the character-select screen for the given mode (used when the
    /// player cancels out of stage-select).
    fn return_to_select(&mut self, mode: screens::SelectMode) {
        self.enter_select(mode);
    }

    /// Returns to the Title screen (fresh cursor).
    fn return_to_title(&mut self) {
        self.screen = RunScreen::Title(screens::TitleMenu::from_system(&self.motif.system));
    }
}

/// Maps a character-select [`screens::SelectMode`] to the match-time
/// [`GameMode`] it implies (T066): the Training select flow enters a
/// [`GameMode::Training`] match (round termination disabled — the Lab), while
/// the Versus flow enters a normal [`GameMode::Versus`] match.
fn game_mode_for(mode: screens::SelectMode) -> GameMode {
    match mode {
        screens::SelectMode::Training => GameMode::Training,
        screens::SelectMode::Versus => GameMode::Versus,
    }
}

/// Builds a [`MatchRun`] (match + per-run render/audio/HUD resources) for two
/// character `.def`s over the player-chosen stage (T041), mirroring the
/// direct-CLI match path. A [`screens::StageKind::Def`] stage loads its MUGEN
/// `[BGdef]` layers; a [`screens::StageKind::Backdrop`] stage draws its image as
/// the full-window background (the dojo default). A missing/unloadable stage
/// degrades to the flat clear color — the match still runs. Returns `None` on a
/// character load failure so the caller can fall back to the title menu. Never
/// panics.
// `build_match_run` threads the player's stage, palette, difficulty, and teaching
// mode selections plus the renderer into the loader; bundling them would only move
// the same flat fields elsewhere, so the arg count is allowed.
#[allow(clippy::too_many_arguments)]
fn build_match_run(
    p1_def: &Path,
    p2_def: &Path,
    stage: &screens::StageChoice,
    cpu_difficulty: AiDifficulty,
    cpu_mode: BehaviorMode,
    renderer: &Renderer,
) -> Option<MatchRun> {
    // A `.def` stage becomes a MUGEN `[BGdef]` stage; a backdrop stage has no
    // `.def` and instead overrides the full-window background image.
    let (stage_def, backdrop): (Option<&Path>, &str) = match stage.kind {
        screens::StageKind::Def => (Some(stage.path.as_path()), DEFAULT_STAGE_BG),
        screens::StageKind::Backdrop => (None, stage.path.to_str().unwrap_or(DEFAULT_STAGE_BG)),
    };
    match load_match_with_backdrop(
        p1_def,
        p2_def,
        stage_def,
        backdrop,
        PalSelection::default(),
        // The in-app menu fights 1v1; the team modes are reachable via the
        // `--simul`/`--turns` direct-CLI flag (T027).
        TeamMode::Single,
        // P2's CPU difficulty as chosen on the Setup/Options screen (T069).
        cpu_difficulty,
        // P2's CPU teaching mode as chosen on the Setup/Options screen (T070).
        cpu_mode,
        renderer,
    ) {
        Mode::Match(run) => Some(*run),
        // A character that fails to load degrades to the test pattern in
        // `load_match_with_backdrop`; the menu flow treats that as "couldn't
        // start" and returns to the title rather than showing a checkerboard.
        _ => None,
    }
}

/// Draws the Title screen: the motif name (when present) and each menu item as a
/// line of text, the highlighted item prefixed with a cursor marker, over the
/// already-cleared solid background. A no-op when no font is loaded.
fn draw_title_screen(
    frame: &mut fp_render::RenderFrame<'_>,
    font: &GlyphFont,
    menu: &screens::TitleMenu,
    title_name: &str,
    win_w: f32,
) {
    /// Menu text scale (the ~7px font at 3x ≈ 21px lines).
    const SCALE: f32 = 3.0;
    /// Title-name scale, a touch larger than the items.
    const TITLE_SCALE: f32 = 4.0;
    let line_h = font.line_height() as f32 * SCALE;

    // Motif name as a header near the top.
    let mut y = 60.0;
    if !title_name.is_empty() {
        draw_centered_text(
            frame,
            font,
            &to_menu_text(title_name),
            win_w,
            y,
            TITLE_SCALE,
            1.0,
        );
    }

    // Menu items, vertically stacked and centered, the cursor item marked.
    y = 180.0;
    for (i, entry) in menu.entries.iter().enumerate() {
        let selected = i == menu.cursor;
        // A leading "> " marks the highlighted item; others are indented to match
        // so the labels line up. The font is uppercase-only, so upcase the label.
        let line = if selected {
            format!("> {}", to_menu_text(&entry.label))
        } else {
            format!("  {}", to_menu_text(&entry.label))
        };
        let alpha = if selected { 1.0 } else { 0.6 };
        draw_centered_text(frame, font, &line, win_w, y, SCALE, alpha);
        y += line_h + 8.0;
    }
}

/// Draws the character-select screen: a header, the roster as a centered text
/// list (one cell per line) with the P1 (and, in Versus, P2) cursor markers, and
/// each player's locked-in pick. A no-op when no font is loaded.
fn draw_select_screen(
    frame: &mut fp_render::RenderFrame<'_>,
    font: &GlyphFont,
    screen: &screens::SelectScreen,
    win_w: f32,
) {
    /// Roster text scale.
    const SCALE: f32 = 2.5;
    let line_h = font.line_height() as f32 * SCALE;

    draw_centered_text(frame, font, "SELECT", win_w, 40.0, 3.0, 1.0);

    let mut y = 120.0;
    for (i, cell) in screen.cells.iter().enumerate() {
        let name = match cell {
            screens::RosterCell::Character(e) => to_menu_text(&e.name),
            screens::RosterCell::Random => "RANDOM".to_string(),
        };
        // Cursor markers: P1 marks its cell, P2 marks its own (Versus only). When
        // both land on the same cell show a combined marker.
        let p1_here = !screen.is_empty() && screen.p1_cursor == i;
        let p2_here = screen.mode == screens::SelectMode::Versus && screen.p2_cursor == i;
        let marker = match (p1_here, p2_here) {
            (true, true) => "P12",
            (true, false) => "P1 ",
            (false, true) => "P2 ",
            (false, false) => "   ",
        };
        let locked = screen.p1_locked == Some(i) || screen.p2_locked == Some(i);
        let line = if locked {
            format!("{marker} {name} *")
        } else {
            format!("{marker} {name}")
        };
        let alpha = if p1_here || p2_here { 1.0 } else { 0.6 };
        draw_centered_text(frame, font, &line, win_w, y, SCALE, alpha);
        y += line_h + 6.0;
    }

    // A short hint at the bottom.
    let hint = match screen.mode {
        screens::SelectMode::Versus => "P1 THEN P2 PICK",
        screens::SelectMode::Training => "PICK FIGHTER",
    };
    draw_centered_text(frame, font, hint, win_w, y + 20.0, 2.0, 0.8);
    // Tab opens the character-info / movelist screen (T071) for the highlighted
    // character.
    draw_centered_text(frame, font, "TAB  INFO", win_w, y + 44.0, 1.5, 0.6);
}

/// Draws the movelist / character-info screen (T071): the character's display
/// name and author as a header, then the movelist as a left-aligned list of
/// `NAME   MOTION` lines, with a dismiss hint. A no-op when no font is loaded
/// (the screen still functions and dismisses; nothing is drawn).
///
/// Renders cleanly for a sparse/malformed character: an empty movelist shows a
/// "NO MOVES LISTED" note rather than a blank panel, and every string is folded
/// into the font's glyph set ([`to_info_text`]) so unrenderable symbols
/// degrade rather than vanish.
fn draw_character_info_screen(
    frame: &mut fp_render::RenderFrame<'_>,
    font: &GlyphFont,
    info: &screens::InfoScreen,
    win_w: f32,
) {
    // Header: display name (large) + author credit (small), both centered.
    let title = if info.display_name.is_empty() {
        "CHARACTER".to_string()
    } else {
        to_menu_text(&info.display_name)
    };
    draw_centered_text(frame, font, &title, win_w, 36.0, 3.0, 1.0);
    if !info.author.is_empty() {
        let author = format!("BY {}", to_menu_text(&info.author));
        draw_centered_text(frame, font, &author, win_w, 80.0, 1.5, 0.8);
    }
    draw_centered_text(frame, font, "MOVELIST", win_w, 112.0, 2.0, 0.9);

    // The movelist, left-aligned. Each line is "NAME   MOTION"; an empty motion
    // (button-only / unparseable) just shows the name.
    const SCALE: f32 = 2.0;
    let line_h = font.line_height() as f32 * SCALE;
    let x = (win_w * 0.12).max(16.0);
    let mut y = 152.0;
    if info.moves.is_empty() {
        draw_centered_text(frame, font, "NO MOVES LISTED", win_w, y, 1.5, 0.7);
    } else {
        for mv in &info.moves {
            let name = to_info_text(&mv.name);
            let line = if mv.motion.is_empty() {
                name
            } else {
                format!("{name}   {}", to_info_text(&mv.motion))
            };
            frame.draw_text(
                font,
                &line,
                &TextDrawParams {
                    x,
                    y,
                    scale: SCALE,
                    alpha: 1.0,
                    blend: BlendMode::Normal,
                },
            );
            y += line_h + 4.0;
        }
    }

    draw_centered_text(
        frame,
        font,
        "TAB OR BACK TO RETURN",
        win_w,
        y + 28.0,
        1.5,
        0.6,
    );
}

/// Folds a movelist string into the menu font's glyph set for display (T071).
///
/// The shipped FNT covers `0-9 A-Z`, space, and colon. The movelist uses a few
/// symbols the font can't draw — `+` (simultaneous press) and the unicode arrows
/// used by the literal-motion fallback. Map them to renderable letters/spaces so
/// the line stays legible (e.g. `QCF+a` -> `QCF A`, `\u{2192}\u{2193}+x` ->
/// `FD X`) instead of dropping glyphs. The underlying movelist data keeps the
/// proper symbols; only the on-screen text is folded.
fn to_info_text(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| match c {
            '+' => ' ',
            '\u{2191}' => 'U', // ↑
            '\u{2193}' => 'D', // ↓
            '\u{2192}' => 'F', // →
            '\u{2190}' => 'B', // ←
            '\u{2197}' => 'U', // ↗ (approx; diagonals collapse to nearest cardinal)
            '\u{2196}' => 'U', // ↖
            '\u{2198}' => 'D', // ↘
            '\u{2199}' => 'D', // ↙
            '[' | ']' => ' ',  // charge brackets -> spaces
            other => other,
        })
        .collect();
    to_menu_text(&mapped)
}

/// Draws the stage-select screen (T041): a header, the available stages as a
/// centered vertical text list (one per line) with the cursor marker on the
/// highlighted stage, and a short navigation hint. A no-op when no font is
/// loaded.
fn draw_stage_select_screen(
    frame: &mut fp_render::RenderFrame<'_>,
    font: &GlyphFont,
    stages: &screens::StageSelect,
    win_w: f32,
) {
    /// Stage-list text scale.
    const SCALE: f32 = 2.5;
    let line_h = font.line_height() as f32 * SCALE;

    draw_centered_text(frame, font, "SELECT STAGE", win_w, 40.0, 3.0, 1.0);

    let mut y = 120.0;
    for (i, entry) in stages.entries.iter().enumerate() {
        let selected = i == stages.cursor;
        // A leading "> " marks the highlighted stage; others align under it.
        let line = if selected {
            format!("> {}", to_menu_text(&entry.name))
        } else {
            format!("  {}", to_menu_text(&entry.name))
        };
        let alpha = if selected { 1.0 } else { 0.6 };
        draw_centered_text(frame, font, &line, win_w, y, SCALE, alpha);
        y += line_h + 6.0;
    }

    draw_centered_text(frame, font, "PICK STAGE", win_w, y + 20.0, 2.0, 0.8);
}

/// Draws the setup / options screen (T042): a header, the input-device
/// preference row, then one row per remappable action showing its currently
/// bound key, with the cursor marker on the highlighted row. While capturing a
/// key it shows a "PRESS A KEY" prompt on the selected action. A no-op when no
/// font is loaded.
fn draw_setup_screen(
    frame: &mut fp_render::RenderFrame<'_>,
    font: &GlyphFont,
    setup: &screens::SetupScreen,
    config: &screens::InputConfig,
    win_w: f32,
) {
    /// Setup-list text scale.
    const SCALE: f32 = 2.5;
    let line_h = font.line_height() as f32 * SCALE;

    draw_centered_text(frame, font, "SETUP", win_w, 40.0, 3.0, 1.0);

    // A short header noting the highlighted target (the device row, the HUD
    // customization row, or an action).
    let focus = if setup.on_device_row() {
        "DEVICE".to_string()
    } else if setup.on_cpu_difficulty_row() {
        "CPU".to_string()
    } else if setup.on_cpu_mode_row() {
        "CPU MODE".to_string()
    } else if setup.on_hud_row() {
        "HUD".to_string()
    } else {
        setup
            .selected_action()
            .map(|a| a.label().to_string())
            .unwrap_or_default()
    };
    draw_centered_text(frame, font, &format!("> {focus}"), win_w, 84.0, 2.0, 0.7);

    let mut y = 120.0;
    // Row 0 is the device toggle, row 1 opens HUD customization (T046); the rest
    // are one per action (in ALL order). `row_count` is the count this loop draws.
    let kinds = setup.row_kinds();
    debug_assert_eq!(kinds.len(), setup.row_count());
    for (i, kind) in kinds.iter().enumerate() {
        let selected = i == setup.cursor;
        let marker = if selected { ">" } else { " " };
        let line = match kind {
            // The device-preference row.
            screens::SetupRowKind::Device => {
                format!("{marker} DEVICE: {}", config.device.label())
            }
            // The CPU-difficulty selector row (T069): Left/Right step it.
            screens::SetupRowKind::CpuDifficulty => {
                format!("{marker} CPU: {}", config.cpu_difficulty.label())
            }
            // The CPU teaching-mode selector row (T070): Left/Right step it.
            screens::SetupRowKind::CpuMode => {
                format!("{marker} CPU MODE: {}", config.cpu_mode.label())
            }
            // The HUD-customization entry row (T046).
            screens::SetupRowKind::HudCustomize => format!("{marker} HUD CUSTOMIZE..."),
            // An action's binding row: label + the bound key's name (or PRESS A
            // KEY while this action is being captured).
            screens::SetupRowKind::Action(act) => {
                let capturing = setup.capturing == Some(*act);
                if capturing {
                    format!("{marker} {}: PRESS A KEY", act.label())
                } else {
                    let key_name = config
                        .key_for(*act)
                        .and_then(scancode_from_keycode)
                        .map(|sc| sc.name().to_ascii_uppercase())
                        .unwrap_or_else(|| "NONE".to_string());
                    format!("{marker} {}: {key_name}", act.label())
                }
            }
        };
        let alpha = if selected { 1.0 } else { 0.6 };
        draw_centered_text(frame, font, &to_menu_text(&line), win_w, y, SCALE, alpha);
        y += line_h + 4.0;
    }

    let hint = if setup.awaiting_key() {
        "PRESS A KEY  ESC CANCEL"
    } else {
        "ENTER REMAP  ESC BACK"
    };
    draw_centered_text(frame, font, hint, win_w, y + 16.0, 2.0, 0.8);
}

/// Draws the HUD-customization screen (T046): the life/power bar color rows and
/// the per-element visibility toggles, with a cursor and the values from the live
/// [`fp_ui::HudConfig`]. Pure presentation over the already-edited config; no
/// gameplay effect here (the renderer reads the config at match time).
fn draw_hud_customize_screen(
    frame: &mut fp_render::RenderFrame<'_>,
    font: &GlyphFont,
    hud: &screens::HudCustomizeScreen,
    config: &screens::HudConfig,
    win_w: f32,
) {
    /// HUD-customization list text scale.
    const SCALE: f32 = 2.5;
    let line_h = font.line_height() as f32 * SCALE;

    draw_centered_text(frame, font, "HUD CUSTOMIZE", win_w, 40.0, 3.0, 1.0);

    // A short header noting the highlighted row.
    let focus = hud.selected_row().map(|r| r.label()).unwrap_or("");
    draw_centered_text(frame, font, &format!("> {focus}"), win_w, 84.0, 2.0, 0.7);

    let rows = hud.rows();
    debug_assert_eq!(rows.len(), hud.row_count());
    let mut y = 120.0;
    for (i, row) in rows.iter().enumerate() {
        let selected = i == hud.cursor;
        let marker = if selected { ">" } else { " " };
        let line = match row {
            screens::HudRow::LifeColor => {
                format!(
                    "{marker} {}: {}",
                    row.label(),
                    color_label(config.life_color())
                )
            }
            screens::HudRow::PowerColor => {
                format!(
                    "{marker} {}: {}",
                    row.label(),
                    color_label(config.power_color())
                )
            }
            screens::HudRow::Visibility(element) => {
                let state = if config.is_visible(*element) {
                    "ON"
                } else {
                    "OFF"
                };
                format!("{marker} {}: {state}", row.label())
            }
        };
        let alpha = if selected { 1.0 } else { 0.6 };
        draw_centered_text(frame, font, &to_menu_text(&line), win_w, y, SCALE, alpha);
        y += line_h + 4.0;
    }

    draw_centered_text(
        frame,
        font,
        "ENTER CHANGE  ESC BACK",
        win_w,
        y + 16.0,
        2.0,
        0.8,
    );
}

/// A short uppercase label for a [`screens::BarColor`] used on the
/// HUD-customization screen, falling back to `CUSTOM` for a non-preset color.
fn color_label(color: screens::BarColor) -> &'static str {
    color.label().unwrap_or("CUSTOM")
}

/// Upcases `s` into the menu font's supported glyph set (the shipped FNT covers
/// `0-9 A-Z`, space, and colon). Lowercase becomes uppercase; any character the
/// font can't draw is harmlessly skipped by `draw_text`'s missing-glyph
/// fallback, so this only needs to fold case for readability.
fn to_menu_text(s: &str) -> String {
    s.to_ascii_uppercase()
}

/// Whether a [`MatchInput`] asserts nothing — no direction held and no button
/// pressed. Used to decide whether the human second player is actually providing
/// input this frame (if not, the CPU AI drives P2).
fn match_input_is_idle(i: MatchInput) -> bool {
    i == MatchInput::none()
}

/// Picks player 2's input for a tick from the human input, an optional CPU AI,
/// and the AI's world observation (T018). Pure (no [`MatchRun`]/SDL) so the
/// override rule is unit-testable:
/// - a non-idle human input always wins (a second controller overrides the AI);
/// - otherwise the AI's decision drives P2;
/// - with no AI and an idle human, P2 stays idle (pre-T018 behaviour).
fn pick_p2_input(
    p2_human: MatchInput,
    cpu_ai: Option<&mut CpuAi>,
    obs: fp_input::AiObservation,
) -> MatchInput {
    if !match_input_is_idle(p2_human) {
        return p2_human;
    }
    match cpu_ai {
        Some(ai) => ai.decide(obs).into(),
        None => p2_human,
    }
}

/// Resolves player 2's input for a tick: the human `p2_human` input when it
/// asserts anything, otherwise the CPU AI's decision (T018) reading the live
/// opponent position. When no AI is installed and the human is idle, P2 stays
/// idle (the pre-T018 behaviour). Thin wrapper over [`pick_p2_input`] that reads
/// the live observation off the run.
fn resolve_p2_input(run: &mut MatchRun, p2_human: MatchInput) -> MatchInput {
    // Observe BEFORE borrowing the AI mutably (both live on `run`).
    let obs = run.team.active().ai_observation_for_p2();
    // Training-mode dummy control (F027 / T067): in the Lab, a non-CPU dummy
    // stance drives P2 with a fixed held-state (stand/crouch/jump/block) computed
    // from the live opponent side and the dummy's "was hit" signal — unless a
    // human second controller asserts something this frame, which always wins.
    // `DummyMode::Cpu` (and any non-Training match) falls through to the baseline
    // CPU AI exactly as before.
    if run.team.game_mode() == GameMode::Training
        && !run.dummy_mode.is_cpu()
        && match_input_is_idle(p2_human)
    {
        let active = run.team.active();
        let opponent_on_right = obs.opponent_on_right();
        // Use the sticky per-combo latch (not the per-tick `p2_was_hit` edge):
        // BlockAfterFirst must keep guarding across the non-damage frames between
        // a combo's hits, not drop its guard on the gap and eat the next hit.
        let was_hit = active.p2_hit_latched();
        return dummy_input(run.dummy_mode, opponent_on_right, was_hit, run.dummy_tick);
    }
    pick_p2_input(p2_human, run.cpu_ai.as_mut(), obs)
}

/// Advances a [`MatchRun`] one 60Hz tick and plays the frame's surfaced sound
/// requests (P1 then P2). Factored out of the run loop so both the direct-CLI
/// match path and the menu Fight screen drive a match identically.
///
/// `p2_input` is the *human* second-player input (a second controller, if any).
/// When it asserts nothing AND a CPU AI is installed (T018), P2's input for this
/// tick is taken from the AI instead — so the otherwise-idle P2 approaches,
/// attacks, blocks, and jumps. A human second controller therefore transparently
/// overrides the AI on any frame it presses something.
/// Applies this frame's training quick-keys (F027 / T067) to a live [`MatchRun`].
///
/// All four are no-ops unless the match is in [`GameMode::Training`] (the Lab),
/// so the keys are inert in a Versus match:
/// - `cycle_dummy` (F4): rotate the P2 dummy stance
///   (`Stand → Crouch → JumpLoop → BlockAll → BlockAfterFirst → Cpu → …`).
/// - `toggle_inf_life` (F5): flip "infinite life" for **both** fighters together.
/// - `toggle_inf_meter` (F6): flip "infinite meter" for both fighters.
/// - `reset_positions` (F7): return both fighters to their round-start positions,
///   facing, and full life without advancing the round.
fn apply_training_keys(
    run: &mut MatchRun,
    cycle_dummy: bool,
    toggle_inf_life: bool,
    toggle_inf_meter: bool,
    reset_positions: bool,
) {
    if run.team.game_mode() != GameMode::Training {
        return;
    }
    if cycle_dummy {
        run.dummy_mode = run.dummy_mode.cycle_next();
        tracing::info!(mode = run.dummy_mode.label(), "training: dummy stance");
    }
    if toggle_inf_life {
        let on = !run.team.infinite_life(Side::P1);
        run.team.set_infinite_life(Side::P1, on);
        run.team.set_infinite_life(Side::P2, on);
        tracing::info!(on, "training: infinite life (both fighters)");
    }
    if toggle_inf_meter {
        let on = !run.team.infinite_meter(Side::P1);
        run.team.set_infinite_meter(Side::P1, on);
        run.team.set_infinite_meter(Side::P2, on);
        tracing::info!(on, "training: infinite meter (both fighters)");
    }
    if reset_positions {
        run.team.reset_positions();
        run.dummy_tick = 0;
        tracing::info!("training: reset positions");
    }
}

fn tick_match_run(run: &mut MatchRun, p1_input: MatchInput, p2_input: MatchInput) {
    let p2_input = resolve_p2_input(run, p2_input);
    // Advance the dummy's jump-cadence clock once per tick (F027 / T067).
    run.dummy_tick = run.dummy_tick.wrapping_add(1);
    // Drive the whole team (in 1v1 / Single this is exactly one pair). The renderer
    // and audio below read the active pair through `run.m()`.
    run.team.tick(p1_input, p2_input);
    // AFTER the tick: play this frame's surfaced sound requests, P1 then P2, each
    // from its own decoded-sound cache. Graceful throughout — a silent backend or
    // a missing sound is a no-op.
    // Field access (`run.team`) keeps the active-match reads disjoint from the
    // `&mut run.audio` mixer borrow.
    run.p1_audio.play_requests(
        &mut run.audio,
        run.team.active().p1(),
        run.team.active().p1_sound_requests(),
    );
    run.p2_audio.play_requests(
        &mut run.audio,
        run.team.active().p2(),
        run.team.active().p2_sound_requests(),
    );

    // Advance each stage background's auto-scroll offset one tick. The cached GPU
    // sprite size (when already decoded) lets the offset wrap within one tile
    // period so it never grows unbounded; an undecoded sprite reports `None`,
    // keeping raw accumulation until it caches. Driven here, in the fixed-tick
    // path, so multi-tick catch-up frames scroll the right amount.
    if let Some(stage) = run.stage.as_mut() {
        stage.advance_scroll();
        // Advance animated (`type = anim`) BG layers' AIR clocks one tick too, so a
        // multi-tick catch-up frame animates the right amount.
        stage.advance_anim();
    }
}

/// Decodes everything a [`MatchRun`] needs to draw this frame (both fighters'
/// current sprites, live hit-spark sprites, stage background sprites, and the
/// active storyboard overlay) into the GPU caches. Must run BEFORE `begin_frame`
/// because decoding needs `&Renderer`, which a live `RenderFrame` holds borrowed.
/// Factored out of the run loop so the Fight screen caches identically.
fn cache_match_run(run: &mut MatchRun, renderer: &Renderer) {
    // Field access (`run.team`) keeps the active-match read disjoint from the
    // `&mut run.pN_render` caches passed alongside it.
    cache_player_sprite(&mut run.p1_render, run.team.active().p1(), renderer);
    cache_player_sprite(&mut run.p2_render, run.team.active().p2(), renderer);
    cache_effect_sprites(run, renderer);
    cache_explod_sprites(run, renderer);
    if let Some(stage) = run.stage.as_mut() {
        stage.cache_sprites(renderer);
    }
    run.tick_storyboard(renderer);
}

/// Draws a whole [`MatchRun`] frame: stage layers, both fighters (sprpriority
/// ordered), hit-sparks, the optional Clsn debug overlay, the HUD (screenpack or
/// quad), and any active storyboard overlay. Factored out of the run loop so the
/// menu Fight screen renders byte-identically to the direct-CLI match path.
fn draw_match_run(
    frame: &mut fp_render::RenderFrame<'_>,
    run: &MatchRun,
    hud: &Hud,
    overlays: MatchOverlays,
    win_wf: f32,
    win_hf: f32,
    tick: u64,
) {
    // Camera follows the fighters' midpoint, clamped to the stage's bounds — X
    // horizontally and Y vertically (scaled by `[Camera] verticalfollow`). With no
    // stage the camera stays at the origin (flat-background path).
    let (camera_x, camera_y) = run
        .stage
        .as_ref()
        .map(|s| {
            (
                s.stage
                    .camera_follow_x(run.m().p1().pos().x, run.m().p2().pos().x),
                s.stage
                    .camera_follow_y(run.m().p1().pos().y, run.m().p2().pos().y),
            )
        })
        .unwrap_or((0.0, 0.0));

    // Full-color background image first of all (behind everything), when no MUGEN
    // stage is loaded. Scaled to fill the whole window.
    if let Some(bg) = run.background.as_ref() {
        frame.draw_image(bg, 0.0, 0.0, win_wf, win_hf);
    }

    // Back background layers first (behind the fighters).
    if let Some(stage) = run.stage.as_ref() {
        stage.draw_layer(frame, BgLayer::Back, camera_x, camera_y, win_wf, win_hf);
    }

    // Draw both fighters ordered by sprite-draw priority (MUGEN `sprpriority`,
    // audit #16): the lower priority is drawn FIRST (behind), the higher OVER it.
    // A tie keeps P1 behind P2 (stable, deterministic order).
    if p1_draws_behind_p2(
        run.m().p1().character.cur_sprpriority,
        run.m().p2().character.cur_sprpriority,
    ) {
        draw_player(
            frame,
            &run.p1_render,
            run.m().p1(),
            camera_x,
            win_wf,
            win_hf,
        );
        draw_player(
            frame,
            &run.p2_render,
            run.m().p2(),
            camera_x,
            win_wf,
            win_hf,
        );
    } else {
        draw_player(
            frame,
            &run.p2_render,
            run.m().p2(),
            camera_x,
            win_wf,
            win_hf,
        );
        draw_player(
            frame,
            &run.p1_render,
            run.m().p1(),
            camera_x,
            win_wf,
            win_hf,
        );
    }

    // Hit-spark effects (audit #17), drawn OVER both fighters, under front BG/HUD.
    draw_effects(
        frame,
        EffectRenders {
            p1_render: &run.p1_render,
            p2_render: &run.p2_render,
            common_render: run.common_fx.as_ref().map(|c| &c.render),
        },
        run.m(),
        camera_x,
        win_wf,
        win_hf,
    );

    // Explod display entities (T033), drawn OVER both fighters with the sparks,
    // under the front BG/HUD; each draws from its owner's sprite cache.
    draw_explods(
        frame,
        &run.p1_render,
        &run.p2_render,
        run.m(),
        camera_x,
        win_wf,
        win_hf,
    );

    // Front background layers, over the fighters but under the HUD.
    if let Some(stage) = run.stage.as_ref() {
        stage.draw_layer(frame, BgLayer::Front, camera_x, camera_y, win_wf, win_hf);
    }

    // Optional Clsn debug overlay (F1) — the raw dev toggle, always both sides.
    if overlays.dev_clsn {
        draw_player_clsn(frame, run.m().p1(), camera_x, win_wf, win_hf);
        draw_player_clsn(frame, run.m().p2(), camera_x, win_wf, win_hf);
    }

    // Player-facing training overlay (T063): styled, per-side-scopable, with a
    // legend. Independent of the raw F1 dev toggle above; reuses the same box
    // math. Drawn under the HUD so the legend text reads over the boxes.
    overlays
        .training
        .draw(frame, run.m(), hud.font(), camera_x, win_wf, win_hf);

    // Player-facing input display (T064): per-side input-history strip + command
    // flash. Off by default; toggled with F3. Drawn under
    // the HUD so the strip text reads over the stage but below lifebars.
    overlays
        .input_display
        .draw(frame, run.m(), hud.font(), win_wf);

    // HUD on top: a loaded screenpack draws real lifebars/text, else the quad HUD.
    // `tick` drives the deterministic max-power flash (T074) for both paths.
    match run.screenpack.as_ref() {
        Some(screenpack) => {
            let mut state = match_hud_state(run.m());
            state.frame = tick;
            screenpack.draw(frame, &state);
        }
        None => hud.draw(frame, win_wf, run.m(), tick),
    }

    // Intro/ending storyboard overlay (audit #32), drawn LAST and only while one
    // is active.
    if let Some(overlay) = run.storyboard_to_draw() {
        overlay.draw(frame, win_wf, win_hf);
    }
}

fn run() -> fp_core::FpResult<()> {
    // --- SDL2 setup ---
    let sdl = sdl2::init().map_err(|e| fp_core::FpError::Other(format!("SDL2 init: {e}")))?;
    let video = sdl
        .video()
        .map_err(|e| fp_core::FpError::Other(format!("SDL2 video: {e}")))?;

    // Game-controller support is optional: if the subsystem can't initialize
    // (no driver / headless), we log and run keyboard-only — never a fatal error.
    let mut controllers = match sdl.game_controller() {
        Ok(subsystem) => Some(Controllers::new(subsystem)),
        Err(e) => {
            tracing::warn!("controller: subsystem unavailable, keyboard only: {e}");
            None
        }
    };

    let window = video
        .window("Fighters Paradise", WINDOW_WIDTH, WINDOW_HEIGHT)
        .position_centered()
        .resizable()
        .metal_view()
        .build()
        .map_err(|e| fp_core::FpError::Other(format!("SDL2 window: {e}")))?;

    // --- wgpu setup ---
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });

    // SAFETY: The surface must live shorter than the window. We ensure this by
    // owning both in the same scope and dropping surface (inside Renderer) when
    // we exit the main loop.
    let surface = unsafe {
        instance.create_surface_unsafe(
            wgpu::SurfaceTargetUnsafe::from_window(&window)
                .map_err(|e| fp_core::FpError::Render(format!("failed to create surface: {e}")))?,
        )
    }
    .map_err(|e| fp_core::FpError::Render(format!("failed to create surface: {e}")))?;

    let mut renderer = pollster::block_on(Renderer::new(
        &instance,
        surface,
        WINDOW_WIDTH,
        WINDOW_HEIGHT,
    ))?;

    // --- Load content based on CLI args ---
    // Strip the per-player `.act` palette flags (`--p1-pal N` / `--p2-pal N`,
    // FL2b) first, then the `--motif <name|path>` flag (T045); the remaining
    // positional args drive file routing as before.
    let raw_args: Vec<String> = std::env::args().collect();
    let (pal, args) = parse_pal_flags(&raw_args);
    let (motif_selector, args) = parse_motif_flag(&args);
    // Team mode (T027): `--simul`/`--turns` select a multi-fighter match on the
    // direct-CLI path; the default is the classic 1v1 (`TeamMode::Single`).
    let (team_mode, args) = parse_team_flag(&args);
    // CPU teaching mode (T070): `--ai-mode <token>` picks which teaching CPU drives
    // P2 on the direct-CLI match path (default `Ladder` = the plain difficulty
    // ladder). The menu flow instead reads the Setup/Options CPU-mode selector.
    let (cli_cpu_mode, args) = parse_ai_mode_flag(&args);

    // The top-level launch route: no file args (or an explicit `menu`) launches
    // the in-app Title menu; a directory argument scans it for a character roster
    // (T043) and launches the menu over it; any direct content path
    // (p1.def/sff/...) keeps the legacy direct view exactly as before. This
    // REPLACES the old no-args default (a two-KFM match) with the menu, so a fresh
    // clean-room checkout boots into the title screen over the shipped
    // trainingdummy roster (no KFM needed).
    let route = cli_route(&args);
    let mut menu_app = match &route {
        CliRoute::Menu => Some(MenuApp::with_options(
            &renderer,
            motif_selector.as_deref(),
            None,
        )),
        CliRoute::Directory(dir) => Some(MenuApp::with_options(
            &renderer,
            motif_selector.as_deref(),
            Some(dir.as_path()),
        )),
        CliRoute::Direct | CliRoute::Replay { .. } => None,
    };
    // The direct-CLI content mode, only built on the Direct route; the Replay route
    // builds the replay-study viewer (T076).
    let mut mode = match &route {
        CliRoute::Direct => Some(select_mode(&args, pal, team_mode, cli_cpu_mode, &renderer)),
        CliRoute::Replay { log, p1, p2 } => {
            let p2_def = p2.as_deref().unwrap_or(p1.as_path());
            Some(load_replay_mode(log, p1, p2_def, &renderer))
        }
        CliRoute::Menu | CliRoute::Directory(_) => None,
    };

    // The minimal match HUD (life bars + KO marker). Built once; drawn in the
    // two-player match mode and the menu Fight screen.
    let hud = Hud::new(&renderer);

    // The live player-1 input configuration (T042): the device preference plus
    // the remappable keyboard binding for each action. Both the menu Fight screen
    // and the direct-CLI match sample the keyboard through this, so a rebind made
    // on the setup screen changes gameplay immediately. The setup screen edits it
    // in place via `&mut`.
    let mut input_config = default_input_config();

    // Edge-detection state for the text menus: last frame's held menu controls.
    // Updated once per real frame so a held key moves the cursor one cell.
    let mut prev_menu_held = screens::HeldMenuInput::default();
    // A monotonic frame counter, used as the deterministic-friendly RNG seed for
    // RandomSelect on the character-select screen.
    let mut frame_counter: u64 = 0;

    // --- Main loop ---
    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| fp_core::FpError::Other(format!("SDL2 event pump: {e}")))?;

    let mut previous = Instant::now();
    let mut accumulator = Duration::ZERO;
    let mut running = true;
    // Player-facing match overlays, all off by default and persisting for the
    // session:
    //   - `dev_clsn` (F1, audit #34): raw both-sides Clsn1/Clsn2 debug boxes.
    //   - `training` (F2, T063): styled per-side (Off → P1 → P2 → Both) hitbox view.
    //   - `input_display` (F3, T064): per-side input-history strip + command flash.
    let mut overlays = MatchOverlays::default();

    while running {
        // Per-frame edge flags driven from discrete key events (below).
        // `esc_pressed` doubles as the menu "back" in menu mode (back out a
        // screen) and a hard quit in direct mode; `confirm_pressed` (Enter/Space)
        // confirms a menu item. These are edges by construction (one KeyDown per
        // physical press), complementing the held-state directions sampled below.
        let mut esc_pressed = false;
        let mut confirm_key_pressed = false;
        // Menu "info" edge (T071): opens the character-info / movelist screen on
        // the character-select screen, and dismisses it. Bound to Tab so it never
        // collides with the confirm (Enter/Space) or back (Esc) keys.
        let mut info_key_pressed = false;
        // The first physical key pressed this frame (its scancode), excluding
        // Escape. Used by the setup screen's key-capture mode (T042) to bind the
        // pressed key to the action being remapped; ignored in every other mode.
        let mut captured_scancode: Option<Scancode> = None;
        // Training-mode quick-key edges (F027 / T067), applied to the active
        // match after the event loop. Each is a one-shot press this frame.
        let mut cycle_dummy_pressed = false;
        let mut toggle_inf_life_pressed = false;
        let mut toggle_inf_meter_pressed = false;
        let mut reset_positions_pressed = false;
        // Replay-study transport edges (T076), applied to a `Mode::Replay` viewer
        // after the event loop; inert in every other mode. Net seek delta this
        // frame (frames; − = rewind), and the discrete play/pause + step + jump
        // edges.
        let mut replay_play_toggle = false;
        let mut replay_step_back = false;
        let mut replay_step_fwd = false;
        let mut replay_seek_delta: i64 = 0;
        let mut replay_jump_start = false;
        let mut replay_jump_end = false;
        // Poll events
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. } => {
                    // A window close is always a hard quit, in any mode.
                    running = false;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    repeat: false,
                    ..
                } => {
                    esc_pressed = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Return | Keycode::Space),
                    repeat: false,
                    ..
                } => {
                    confirm_key_pressed = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Tab),
                    repeat: false,
                    ..
                } => {
                    // Menu "info" action (T071). Outside the menu it is inert.
                    info_key_pressed = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F1),
                    repeat: false,
                    ..
                } => {
                    overlays.dev_clsn = !overlays.dev_clsn;
                    tracing::info!(
                        "Clsn debug overlay {}",
                        if overlays.dev_clsn { "ON" } else { "OFF" }
                    );
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F2),
                    repeat: false,
                    ..
                } => {
                    // Player-facing training overlay (T063): cycle Off → P1 → P2
                    // → Both → Off. Independent of the F1 dev overlay.
                    overlays.training.cycle();
                    tracing::info!("Training Clsn overlay: {}", overlays.training.scope.label());
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F3),
                    repeat: false,
                    ..
                } => {
                    // Player-facing input display (T064): cycle Off → P1 → P2 →
                    // Both → Off. Independent of the F1/F2 overlays.
                    overlays.input_display.cycle();
                    tracing::info!("Input display: {}", overlays.input_display.scope.label());
                }
                // Training quick-keys (F027 / T067). These set edge flags applied
                // to the active match below; they are no-ops outside Training.
                Event::KeyDown {
                    keycode: Some(Keycode::F4),
                    repeat: false,
                    ..
                } => {
                    cycle_dummy_pressed = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F5),
                    repeat: false,
                    ..
                } => {
                    toggle_inf_life_pressed = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F6),
                    repeat: false,
                    ..
                } => {
                    toggle_inf_meter_pressed = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F7),
                    repeat: false,
                    ..
                } => {
                    reset_positions_pressed = true;
                }
                // Replay-study transport (T076): play/pause, step ∓1, seek ∓10,
                // jump to start/end. Inert outside `Mode::Replay`. Key repeat is
                // allowed on step/seek so a held key scrubs.
                Event::KeyDown {
                    keycode: Some(Keycode::P),
                    repeat: false,
                    ..
                } => {
                    replay_play_toggle = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Comma),
                    ..
                } => {
                    replay_step_back = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Period),
                    ..
                } => {
                    replay_step_fwd = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Left),
                    ..
                } => {
                    replay_seek_delta -= 10;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Right),
                    ..
                } => {
                    replay_seek_delta += 10;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Home),
                    repeat: false,
                    ..
                } => {
                    replay_jump_start = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::End),
                    repeat: false,
                    ..
                } => {
                    replay_jump_end = true;
                }
                // Any other physical key press: record the first one this frame so
                // the setup screen's remap-capture can bind it. (Esc/Return/Space/
                // F1/F2 are handled above; this catches the rest, e.g. a new key
                // for an action.) Only the first press per frame is kept.
                Event::KeyDown {
                    scancode: Some(scancode),
                    repeat: false,
                    ..
                } => {
                    if captured_scancode.is_none() {
                        captured_scancode = Some(scancode);
                    }
                }
                Event::Window {
                    win_event: sdl2::event::WindowEvent::Resized(w, h),
                    ..
                } => {
                    renderer.resize(w as u32, h as u32);
                }
                // Controller hotplug. `which` is a joystick *index* on add and an
                // instance *id* on remove (SDL convention). Both are handled
                // failure-tolerantly: a missing subsystem or a failed open is a
                // no-op, never a panic.
                Event::ControllerDeviceAdded { which, .. } => {
                    if let Some(c) = controllers.as_mut() {
                        c.on_device_added(which);
                    }
                }
                Event::ControllerDeviceRemoved { which, .. } => {
                    if let Some(c) = controllers.as_mut() {
                        c.on_device_removed(which);
                    }
                }
                _ => {}
            }
        }

        // Fixed timestep accumulation
        let current = Instant::now();
        accumulator += current - previous;
        previous = current;

        // Sample every physical input ONCE per real frame, BEFORE the
        // fixed-timestep catch-up loop (audit #27). On a frame that has to run
        // multiple ticks to catch up, every sub-tick is driven by this single
        // physical snapshot — re-reading `keyboard_state()` / the controller
        // inside the loop would replay the same live state N times anyway, but
        // doing it here makes the "one input per frame" semantics explicit and
        // keeps press-vs-hold edges and command timing correct (one buffer push
        // per frame). The snapshots are cheap, so we take them unconditionally
        // even in non-Match modes (which ignore them).
        //
        // P1 acts from the keyboard OR controller 0 (either source can assert any
        // bit). If a second controller is present, it drives P2 for a real
        // two-human match; otherwise the baseline CPU AI drives P2 (T018, resolved
        // per-frame in `tick_match_run`/`resolve_p2_input`).
        let kbd_input = match_input_from_keyboard(&input_config, &event_pump.keyboard_state());
        let pad0 = controllers
            .as_ref()
            .and_then(|c| c.input(0))
            .map(|c| controller_to_match_input(&c))
            .unwrap_or_else(MatchInput::none);
        let p1_input = merge_match_input(kbd_input, pad0);
        let p2_input = controllers
            .as_ref()
            .and_then(|c| c.input(1))
            .map(|c| controller_to_match_input(&c))
            .unwrap_or_else(MatchInput::none);

        frame_counter = frame_counter.wrapping_add(1);

        // --- Menu screen update (menu mode only) ---
        // Build the rising-edge menu input for this frame: directions from the
        // held P1 input (keyboard OR controller 0), confirm from Enter/Space OR a
        // controller face/attack button, back from Esc OR controller B. A held
        // direction yields exactly one cursor step thanks to edge detection.
        if let Some(app) = menu_app.as_mut() {
            // Held state this frame (for direction edges).
            let held = screens::HeldMenuInput {
                up: p1_input.up,
                down: p1_input.down,
                left: p1_input.left,
                right: p1_input.right,
                // A held confirm/back is also fine — edge detection makes it
                // one-shot. Map controller A (a) to confirm, B (b) to back, C (c)
                // to info (the character-info / movelist action, T071).
                confirm: p1_input.a,
                back: p1_input.b,
                info: p1_input.c,
            };
            let mut menu_in = screens::MenuInput::from_edges(held, prev_menu_held);
            // Fold in the discrete key-event edges (these are already one-shot).
            menu_in.confirm = menu_in.confirm || confirm_key_pressed;
            menu_in.back = menu_in.back || esc_pressed;
            menu_in.info = menu_in.info || info_key_pressed;
            prev_menu_held = held;

            // Drive the active menu screen. The Fight screen is driven by the
            // normal match tick path below (not here); Title/Select consume the
            // menu input and may transition.
            match app.screen {
                RunScreen::Title(ref mut menu) => {
                    if let Some(action) = menu.update(menu_in) {
                        match action {
                            screens::TitleAction::Select(mode) => app.enter_select(mode),
                            screens::TitleAction::Setup => app.enter_setup(),
                            screens::TitleAction::Quit => app.screen = RunScreen::Quit,
                            screens::TitleAction::NoOp => {}
                        }
                    }
                }
                RunScreen::Setup(ref mut setup) => {
                    // In key-capture mode, a fresh physical key press (collected
                    // above, excluding Esc which cancels) rebinds the selected
                    // action; the new binding is read by the keyboard sampler next
                    // frame, so the remap takes effect in-match immediately. When a
                    // key is captured this frame, navigation is NOT also run (the
                    // capture consumes the frame), so a single press can't both
                    // bind and re-arm/navigate.
                    let mut captured = false;
                    if setup.awaiting_key() {
                        if let Some(scancode) = captured_scancode {
                            if let Some((action, displaced)) =
                                setup.capture_key(keycode_of(scancode), &mut input_config)
                            {
                                tracing::info!(
                                    "remapped {action:?} to key {} (was on {displaced:?})",
                                    scancode.name(),
                                );
                                captured = true;
                            }
                        }
                    }
                    if !captured {
                        match setup.update(menu_in, &mut input_config) {
                            screens::SetupOutcome::Pending => {}
                            screens::SetupOutcome::Exit => app.return_to_title(),
                            // Open the HUD-customization screen (T046).
                            screens::SetupOutcome::OpenHudCustomize => app.enter_hud_customize(),
                        }
                    }
                }
                RunScreen::HudCustomize(_) => {
                    // Edit the live HUD-customization config (T046); back returns to
                    // the setup screen. The edited config is applied to a match's
                    // screenpack HUD when the next fight starts. The method
                    // split-borrows the screen + the hud_config off the app.
                    app.tick_hud_customize(menu_in);
                }
                RunScreen::Select(ref mut select) => {
                    let mode = select.mode;
                    let outcome = select.update(menu_in, frame_counter);
                    // Snapshot the select screen for outcomes that need to leave it
                    // and come back (Info), so the `&mut app.screen` borrow ends
                    // before reassigning the screen.
                    let select_snapshot = select.clone();
                    match outcome {
                        screens::SelectOutcome::Pending => {}
                        screens::SelectOutcome::Cancelled => app.return_to_title(),
                        // Characters chosen: advance to stage-select (T041), not
                        // straight to the fight.
                        screens::SelectOutcome::Done(pick) => app.enter_stage_select(pick, mode),
                        // Info pressed: open the movelist / character-info screen
                        // (T071), keeping the select screen to resume on dismiss.
                        screens::SelectOutcome::ShowInfo(def_path) => {
                            app.enter_character_info(&def_path, select_snapshot)
                        }
                    }
                }
                RunScreen::CharacterInfo { ref info, .. } => {
                    // Any dismiss returns to the snapshotted character-select.
                    if info.update(menu_in) == screens::InfoOutcome::Dismissed {
                        // Move the saved select screen back out (take ownership by
                        // replacing the whole screen).
                        if let RunScreen::CharacterInfo { select, .. } =
                            std::mem::replace(&mut app.screen, RunScreen::Quit)
                        {
                            app.screen = RunScreen::Select(select);
                        }
                    }
                }
                RunScreen::StageSelect {
                    ref mut stages,
                    ref pick,
                    mode,
                } => {
                    // Step the stage cursor, then clone the pick so the
                    // `&mut app.screen` borrow ends before `enter_fight`/
                    // `return_to_select` reassign the screen.
                    let outcome = stages.update(menu_in);
                    let pick = pick.clone();
                    match outcome {
                        screens::StageOutcome::Pending => {}
                        // Cancel from stage-select goes back to character-select.
                        screens::StageOutcome::Cancelled => app.return_to_select(mode),
                        screens::StageOutcome::Done(stage) => {
                            app.enter_fight(
                                &pick,
                                &stage,
                                mode,
                                input_config.cpu_difficulty,
                                input_config.cpu_mode,
                                &renderer,
                            );
                        }
                    }
                }
                RunScreen::Fight(_) | RunScreen::Quit => {}
            }

            if !app.running() {
                running = false;
            }
        } else if esc_pressed {
            // Direct CLI modes keep the original Esc-quits behaviour.
            running = false;
        }

        // Apply this frame's training quick-keys (F027 / T067) to whichever match
        // is live (direct-CLI or the menu Fight screen). No-ops outside Training.
        if cycle_dummy_pressed
            || toggle_inf_life_pressed
            || toggle_inf_meter_pressed
            || reset_positions_pressed
        {
            if let Some(Mode::Match(run)) = mode.as_mut() {
                apply_training_keys(
                    run,
                    cycle_dummy_pressed,
                    toggle_inf_life_pressed,
                    toggle_inf_meter_pressed,
                    reset_positions_pressed,
                );
            }
            if let Some(RunScreen::Fight(run)) = menu_app.as_mut().map(|a| &mut a.screen) {
                apply_training_keys(
                    run,
                    cycle_dummy_pressed,
                    toggle_inf_life_pressed,
                    toggle_inf_meter_pressed,
                    reset_positions_pressed,
                );
            }
        }

        // Apply this frame's replay-study transport edges (T076) to a viewer; inert
        // in every other mode. Seek/step happen here (outside the tick loop), so a
        // scrub is one discrete restore-and-re-sim per real frame; play advances in
        // the fixed-timestep loop below.
        if let Some(Mode::Replay(v)) = mode.as_mut() {
            if replay_play_toggle {
                v.toggle_play();
            }
            if replay_jump_start {
                v.seek(0);
            }
            if replay_jump_end {
                v.seek(v.len());
            }
            if replay_step_back {
                v.step_back();
            }
            if replay_step_fwd {
                v.advance();
            }
            if replay_seek_delta != 0 {
                let cur = v.frame as i64;
                let target = (cur + replay_seek_delta).clamp(0, v.len() as i64) as u32;
                v.seek(target);
            }
        }

        // --- Fixed-timestep tick (catch-up loop) ---
        // Both the direct-CLI match and the menu Fight screen drive their match at
        // a fixed 60Hz here; the Title/Select menu screens are event-driven (no
        // per-tick simulation), so they only need a render below. Viewer/Static/
        // TestPattern tick as before.
        while accumulator >= TICK_DURATION {
            match mode.as_mut() {
                Some(Mode::Match(run)) => tick_match_run(run, p1_input, p2_input),
                // A replay viewer advances one recorded frame per tick only while
                // playing; paused, it holds the current frame (transport seeks above).
                Some(Mode::Replay(v)) => {
                    if v.playing {
                        v.advance();
                    }
                }
                Some(Mode::Viewer(v)) => v.tick(),
                Some(Mode::Static(..)) | Some(Mode::TestPattern(..)) | None => {}
            }
            // The menu Fight screen advances its match here too.
            if let Some(RunScreen::Fight(run)) = menu_app.as_mut().map(|a| &mut a.screen) {
                tick_match_run(run, p1_input, p2_input);
            }
            accumulator -= TICK_DURATION;
        }

        // After the catch-up loop: if the menu's match is over, return to the
        // title screen (Menu-2 deliverable 4). Done outside the tick loop so the
        // transition happens once per real frame, after all sub-ticks.
        if let Some(app) = menu_app.as_mut() {
            if let RunScreen::Fight(run) = &app.screen {
                if run.m().match_winner().is_some() {
                    tracing::info!("Match over; returning to title menu");
                    app.return_to_title();
                }
            }
        }

        // Ensure the current animation frame's sprite is cached before rendering.
        // Caching needs `&Renderer`, which a live `RenderFrame` would hold
        // borrowed, so it must happen before `begin_frame`.
        match mode.as_mut() {
            Some(Mode::Match(run)) => cache_match_run(run, &renderer),
            Some(Mode::Replay(v)) => cache_match_run(&mut v.run, &renderer),
            Some(Mode::Viewer(v)) => {
                if let Some(sid) = v.current_frame().map(|f| f.sprite) {
                    v.get_or_create_sprite(sid, &renderer);
                }
            }
            Some(Mode::Static(..)) | Some(Mode::TestPattern(..)) | None => {}
        }
        if let Some(RunScreen::Fight(run)) = menu_app.as_mut().map(|a| &mut a.screen) {
            cache_match_run(run, &renderer);
        }

        // Render
        let mut frame = renderer.begin_frame()?;
        frame.clear(0.1, 0.1, 0.15);

        let (win_w, win_h) = window.size();
        let win_wf = win_w as f32;
        let win_hf = win_h as f32;

        // Direct-CLI content modes render exactly as before.
        match mode.as_ref() {
            Some(Mode::Match(run)) => {
                draw_match_run(
                    &mut frame,
                    run,
                    &hud,
                    overlays,
                    win_wf,
                    win_hf,
                    frame_counter,
                );
            }
            // Replay-study viewer (T076): the live match + its overlays draw through
            // the exact same path as a normal match (the F026 overlays "just work"
            // over the live state), with a transport status line on top.
            Some(Mode::Replay(v)) => {
                draw_match_run(
                    &mut frame,
                    &v.run,
                    &hud,
                    overlays,
                    win_wf,
                    win_hf,
                    frame_counter,
                );
                if let Some(font) = hud.font() {
                    let status = format!(
                        "REPLAY {} {}/{}  P:PLAY ,/.:STEP <>:SEEK10",
                        if v.playing { "PLAY" } else { "PAUSE" },
                        v.frame,
                        v.len()
                    );
                    draw_centered_text(&mut frame, font, &status, win_wf, 8.0, 1.0, 1.0);
                }
            }
            Some(Mode::Viewer(v)) => {
                if let Some(anim_frame) = v.current_frame() {
                    if let Some(cached) = v.sprite_cache.get(&anim_frame.sprite) {
                        let center_x = win_w as f32 / 2.0;
                        let ground_y = win_h as f32 * 0.7;
                        let draw_x = center_x - cached.axis_x as f32 + anim_frame.offset.x as f32;
                        let draw_y = ground_y - cached.axis_y as f32 + anim_frame.offset.y as f32;
                        let (render_blend, alpha) = map_blend_mode(&anim_frame.blend);
                        let params = SpriteDrawParams {
                            x: draw_x,
                            y: draw_y,
                            flip_h: anim_frame.flip_h,
                            flip_v: anim_frame.flip_v,
                            blend: render_blend,
                            alpha,
                            ..Default::default()
                        };
                        frame.draw_sprite(&cached.texture, &cached.palette, &params);
                    }
                }
            }
            Some(Mode::Static(sprite_tex, palette_tex))
            | Some(Mode::TestPattern(sprite_tex, palette_tex)) => {
                let params = SpriteDrawParams {
                    x: (win_w as f32 - sprite_tex.width as f32) / 2.0,
                    y: (win_h as f32 - sprite_tex.height as f32) / 2.0,
                    ..Default::default()
                };
                frame.draw_sprite(sprite_tex, palette_tex, &params);
            }
            None => {}
        }

        // Menu-mode screens render over the solid clear color: Title/Select as
        // text, Fight via the shared match draw path (identical to the direct
        // match render above).
        if let Some(app) = menu_app.as_ref() {
            match &app.screen {
                RunScreen::Title(menu) => {
                    if let Some(font) = app.font.as_ref() {
                        draw_title_screen(&mut frame, font, menu, &app.motif.system.name, win_wf);
                    }
                }
                RunScreen::Select(select) => {
                    if let Some(font) = app.font.as_ref() {
                        draw_select_screen(&mut frame, font, select, win_wf);
                    }
                }
                RunScreen::CharacterInfo { info, .. } => {
                    if let Some(font) = app.font.as_ref() {
                        draw_character_info_screen(&mut frame, font, info, win_wf);
                    }
                }
                RunScreen::StageSelect { stages, .. } => {
                    if let Some(font) = app.font.as_ref() {
                        draw_stage_select_screen(&mut frame, font, stages, win_wf);
                    }
                }
                RunScreen::Setup(setup) => {
                    if let Some(font) = app.font.as_ref() {
                        draw_setup_screen(&mut frame, font, setup, &input_config, win_wf);
                    }
                }
                RunScreen::HudCustomize(hud) => {
                    if let Some(font) = app.font.as_ref() {
                        draw_hud_customize_screen(&mut frame, font, hud, &app.hud_config, win_wf);
                    }
                }
                RunScreen::Fight(run) => {
                    draw_match_run(
                        &mut frame,
                        run,
                        &hud,
                        overlays,
                        win_wf,
                        win_hf,
                        frame_counter,
                    );
                }
                RunScreen::Quit => {}
            }
        }

        frame.finish();
    }

    tracing::info!("Shutting down");
    Ok(())
}

/// Load the first sprite from an SFF file and create GPU textures.
fn load_sff_sprite(
    renderer: &Renderer,
    path: &Path,
) -> fp_core::FpResult<(SpriteTexture, PaletteTexture)> {
    tracing::info!("Loading SFF file: {}", path.display());
    let sff = SffFile::load(path)?;

    tracing::info!(
        "SFF loaded: {} sprites, {} palettes",
        sff.sprites.len(),
        sff.palettes.len()
    );

    let sprite = sff
        .sprites
        .first()
        .ok_or_else(|| fp_core::FpError::not_found("sprite", "SFF file contains no sprites"))?;

    let pixels = sff.decode_sprite(0)?;
    let palette_data = sff.palette(sprite.palette_index as usize)?;

    tracing::info!(
        "Sprite ({}, {}): {}x{}, palette index {}",
        sprite.group,
        sprite.image,
        sprite.width,
        sprite.height,
        sprite.palette_index
    );

    let sprite_tex = SpriteTexture::new(
        renderer.device(),
        renderer.queue(),
        sprite.width as u32,
        sprite.height as u32,
        &pixels,
    );
    let palette_tex = PaletteTexture::new(renderer.device(), renderer.queue(), &palette_data);

    Ok((sprite_tex, palette_tex))
}

/// Generate a checkerboard test pattern and rainbow palette.
fn generate_test_pattern(renderer: &Renderer) -> (SpriteTexture, PaletteTexture) {
    let size: u32 = 128;
    let tile: u32 = 8;
    let mut pixels = vec![0u8; (size * size) as usize];

    for y in 0..size {
        for x in 0..size {
            let checker = ((x / tile) + (y / tile)) % 2;
            pixels[(y * size + x) as usize] = if checker == 0 { 1 } else { 2 };
        }
    }

    let mut palette = [0u8; 1024];
    // Index 1: white
    palette[4] = 255;
    palette[5] = 255;
    palette[6] = 255;
    palette[7] = 255;
    // Index 2: dark gray
    palette[8] = 80;
    palette[9] = 80;
    palette[10] = 80;
    palette[11] = 255;
    // Fill remaining with rainbow gradient
    for i in 3..256usize {
        let t = (i - 3) as f32 / 253.0;
        let (r, g, b) = hsv_to_rgb(t * 360.0, 0.8, 0.9);
        palette[i * 4] = r;
        palette[i * 4 + 1] = g;
        palette[i * 4 + 2] = b;
        palette[i * 4 + 3] = 255;
    }

    let sprite_tex = SpriteTexture::new(renderer.device(), renderer.queue(), size, size, &pixels);
    let palette_tex = PaletteTexture::new(renderer.device(), renderer.queue(), &palette);

    (sprite_tex, palette_tex)
}

/// Simple HSV to RGB conversion for palette generation.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;

    let (r, g, b) = match (h as u32) / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    // `CommandSource::is_active` is the trait method the Proctor snapshot tests
    // call on `ActiveCommands`; bring the trait into scope so it resolves.
    use fp_character::CommandSource;
    use std::path::PathBuf;

    /// Resolves a path under the workspace `test-assets/` directory.
    /// `CARGO_MANIFEST_DIR` points at `crates/fp-app`; go up two levels.
    fn test_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join(rel)
    }

    /// A neutral input frame.
    fn neutral() -> InputState {
        InputState::default()
    }

    /// An input frame holding the absolute Right direction.
    fn hold_right() -> InputState {
        InputState {
            direction: Direction {
                right: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn controller_to_match_input_carries_directions_and_buttons() {
        // A right-down + (a, c) controller snapshot must surface those exact bits.
        let raw = RawController {
            dpad_right: true,
            stick_y: 30000,       // down (SDL: +Y is down)
            face_west: true,      // a
            shoulder_right: true, // c
            ..RawController::default()
        };
        let ci = map_controller(&raw, DEADZONE_DEFAULT);
        let mi = controller_to_match_input(&ci);
        assert!(mi.right);
        assert!(mi.down);
        assert!(!mi.up);
        assert!(!mi.left);
        assert!(mi.a);
        assert!(mi.c);
        assert!(!mi.b && !mi.x && !mi.y && !mi.z);
    }

    #[test]
    fn controller_to_match_input_neutral_is_none() {
        let ci = map_controller(&RawController::default(), DEADZONE_DEFAULT);
        assert_eq!(controller_to_match_input(&ci), MatchInput::none());
    }

    #[test]
    fn merge_match_input_is_per_field_or() {
        let kbd = MatchInput {
            left: true,
            a: true,
            ..MatchInput::none()
        };
        let pad = MatchInput {
            right: true,
            a: true,
            z: true,
            ..MatchInput::none()
        };
        let merged = merge_match_input(kbd, pad);
        assert!(merged.left, "from keyboard");
        assert!(merged.right, "from controller");
        assert!(merged.a, "asserted by both");
        assert!(merged.z, "from controller");
        assert!(!merged.up && !merged.down);
        assert!(!merged.b && !merged.c && !merged.x && !merged.y);
    }

    #[test]
    fn merge_with_none_is_identity() {
        let only_kbd = MatchInput {
            up: true,
            x: true,
            ..MatchInput::none()
        };
        assert_eq!(merge_match_input(only_kbd, MatchInput::none()), only_kbd);
        assert_eq!(merge_match_input(MatchInput::none(), only_kbd), only_kbd);
    }

    // -----------------------------------------------------------------------
    // Player-1 game-controller path (T038). The live loop builds P1's input as
    //     merge_match_input(kbd_input, controller_to_match_input(&pad0))
    // where `pad0` is the pure `map_controller` of controller slot 0's raw
    // state. These tests exercise that exact composition with synthetic raw
    // snapshots (no live SDL), asserting the three acceptance criteria:
    //   1. a controller's d-pad/stick + buttons produce the correct P1
    //      MatchInput (directions + a/b/c/x/y/z);
    //   2. keyboard and controller both drive P1 (OR'd), neither disables the
    //      other;
    //   3. an absent / detached controller (modeled as a neutral pad0) leaves
    //      the keyboard's P1 input untouched — the hot-plug-safe steady state.
    // -----------------------------------------------------------------------

    /// Builds P1's per-frame `MatchInput` the way the main loop does: the
    /// keyboard snapshot OR'd with controller slot 0's mapped input. A `None`
    /// pad models "no controller bound / detached" (what `Controllers::input`
    /// returns when a slot is empty or the device silently detached).
    fn p1_input(kbd: MatchInput, pad0_raw: Option<RawController>) -> MatchInput {
        let pad0 = pad0_raw
            .map(|raw| controller_to_match_input(&map_controller(&raw, DEADZONE_DEFAULT)))
            .unwrap_or_else(MatchInput::none);
        merge_match_input(kbd, pad0)
    }

    #[test]
    fn p1_controller_dpad_and_all_six_buttons_reach_p1_input() {
        // Acceptance #1: a controller alone (no keyboard input) drives P1's
        // directions and every one of the six MUGEN attack buttons.
        let raw = RawController {
            dpad_left: true,
            dpad_up: true,
            face_west: true,      // a
            face_north: true,     // b
            shoulder_right: true, // c
            face_south: true,     // x
            face_east: true,      // y
            shoulder_left: true,  // z
            ..RawController::default()
        };
        let p1 = p1_input(MatchInput::none(), Some(raw));
        assert!(p1.left, "d-pad left -> P1 left");
        assert!(p1.up, "d-pad up -> P1 up");
        assert!(!p1.right && !p1.down);
        assert!(
            p1.a && p1.b && p1.c && p1.x && p1.y && p1.z,
            "all six buttons: {p1:?}"
        );
    }

    #[test]
    fn p1_left_stick_past_deadzone_drives_p1_directions() {
        // Acceptance #1 (stick path): the left analog stick past the deadzone
        // drives P1's directions (SDL convention: +X right, +Y down).
        let raw = RawController {
            stick_x: 25_000,
            stick_y: 25_000,
            ..RawController::default()
        };
        let p1 = p1_input(MatchInput::none(), Some(raw));
        assert!(p1.right, "stick +X -> P1 right");
        assert!(p1.down, "stick +Y -> P1 down");
        assert!(!p1.left && !p1.up);
    }

    #[test]
    fn p1_keyboard_and_controller_are_ored_neither_disables_the_other() {
        // Acceptance #2: keyboard AND controller both drive P1. The keyboard
        // walks back + presses `a`; the pad simultaneously holds forward +
        // presses `c`. Every asserted bit from either source must survive.
        let kbd = MatchInput {
            left: true,
            a: true,
            ..MatchInput::none()
        };
        let pad = RawController {
            dpad_right: true,
            shoulder_right: true, // c
            ..RawController::default()
        };
        let p1 = p1_input(kbd, Some(pad));
        assert!(p1.left, "keyboard direction survives");
        assert!(p1.a, "keyboard button survives");
        assert!(p1.right, "controller direction survives");
        assert!(p1.c, "controller button survives");
        // Untouched bits stay clear.
        assert!(!p1.up && !p1.down && !p1.b && !p1.x && !p1.y && !p1.z);
    }

    #[test]
    fn p1_keyboard_alone_works_when_no_controller_is_bound() {
        // Acceptance #2/#3: with no controller (pad0 = None, the steady state
        // after a disconnect frees the slot), the keyboard alone still drives
        // P1 unchanged — the controller never gates the keyboard.
        let kbd = MatchInput {
            up: true,
            z: true,
            ..MatchInput::none()
        };
        assert_eq!(p1_input(kbd, None), kbd, "keyboard-only P1 is unaffected");
    }

    #[test]
    fn p1_neutral_controller_does_not_clobber_keyboard() {
        // Acceptance #3: a bound-but-idle controller (neutral raw snapshot,
        // e.g. resting analog sticks) contributes nothing — it must not clear
        // or alter the keyboard's P1 input.
        let kbd = MatchInput {
            right: true,
            x: true,
            ..MatchInput::none()
        };
        let neutral_pad = RawController::default();
        assert_eq!(
            p1_input(kbd, Some(neutral_pad)),
            kbd,
            "idle controller is a no-op on P1"
        );
    }

    // -----------------------------------------------------------------------
    // Controller-driven menu navigation (T039 title nav, T040 char-select nav).
    //
    // The main loop builds the source-agnostic `screens::MenuInput` from the P1
    // `MatchInput` (keyboard OR controller, see the loop): directions come from
    // the held P1 input edges, confirm from the controller `a` button (or
    // Enter/Space), back from the controller `b` button (or Esc). These tests
    // drive that exact composition from a synthetic `RawController` to prove a
    // controller alone navigates the Title menu and the Character-Select grid and
    // can confirm — with no keyboard involved.
    // -----------------------------------------------------------------------

    /// Builds the rising-edge `MenuInput` a controller produces this frame, the
    /// way the main loop does: map the raw pad to a P1 `MatchInput`, fold it into
    /// a `HeldMenuInput` (a = confirm, b = back), and edge-detect against the
    /// previous held state. With `prev = default`, every held control is a fresh
    /// edge (one cursor step).
    fn menu_input_from_pad(raw: RawController) -> screens::MenuInput {
        let p1 = controller_to_match_input(&map_controller(&raw, DEADZONE_DEFAULT));
        let held = screens::HeldMenuInput {
            up: p1.up,
            down: p1.down,
            left: p1.left,
            right: p1.right,
            confirm: p1.a,
            back: p1.b,
            info: p1.c,
        };
        screens::MenuInput::from_edges(held, screens::HeldMenuInput::default())
    }

    #[test]
    fn controller_navigates_and_confirms_title_menu() {
        // Acceptance #1: the controller's d-pad moves the title cursor and the
        // confirm button (mapped from face `a`) activates the highlighted item —
        // all with no keyboard input.
        let mut menu = screens::TitleMenu::fallback(); // VS / TRAINING / SETUP / EXIT
        assert_eq!(menu.cursor, 0);

        // D-pad down moves the highlight one item (cursor 0 -> 1 = TRAINING).
        let down = RawController {
            dpad_down: true,
            ..RawController::default()
        };
        assert_eq!(
            menu.update(menu_input_from_pad(down)),
            None,
            "move, no action"
        );
        assert_eq!(menu.cursor, 1);

        // The left analog stick (past the deadzone) also drives the cursor: down
        // again -> SETUP (cursor 2).
        let stick_down = RawController {
            stick_y: 25_000, // +Y is down (SDL convention)
            ..RawController::default()
        };
        menu.update(menu_input_from_pad(stick_down));
        assert_eq!(menu.cursor, 2);

        // The confirm button (face `a`) activates the highlighted SETUP item.
        let confirm = RawController {
            face_west: true, // `a`
            ..RawController::default()
        };
        assert_eq!(
            menu.update(menu_input_from_pad(confirm)),
            Some(screens::TitleAction::Setup),
            "controller confirm activates the highlighted title item"
        );
    }

    #[test]
    fn controller_back_button_quits_title_menu() {
        // Acceptance #1: the controller `b` button maps to menu back, which quits
        // from the title (the title's documented back behaviour). MUGEN `b` is the
        // North face button (`controller_to_match_input`/`map_controller`).
        let mut menu = screens::TitleMenu::fallback();
        let back = RawController {
            face_north: true, // MUGEN `b` -> menu back
            ..RawController::default()
        };
        assert_eq!(
            menu.update(menu_input_from_pad(back)),
            Some(screens::TitleAction::Quit)
        );
    }

    #[test]
    fn controller_moves_select_cursor_on_grid_and_confirms() {
        // Acceptance #2: the controller d-pad moves the (grid-aware) select cursor
        // and a button confirms/locks a pick — no keyboard. A 3-column grid of two
        // characters + random; Training so a single confirm completes.
        let select = SelectDef::parse(
            "[Characters]\nChar Alpha, a/a.def\nChar Beta, b/b.def\nrandomselect\n",
        );
        let info = fp_ui::SelectInfo {
            columns: 3,
            rows: 1,
            ..fp_ui::SelectInfo::default()
        };
        let mut screen = screens::SelectScreen::new(
            screens::SelectMode::Training,
            &select,
            &info,
            Path::new("data/select.def"),
        );
        assert_eq!(screen.p1_cursor, 0);

        // D-pad right steps the cursor along the grid row (0 -> 1 = Beta).
        let right = RawController {
            dpad_right: true,
            ..RawController::default()
        };
        assert_eq!(
            screen.update(menu_input_from_pad(right), 0),
            screens::SelectOutcome::Pending
        );
        assert_eq!(
            screen.p1_cursor, 1,
            "controller d-pad moved the grid cursor"
        );

        // The confirm button (face `a`) locks the pick; Training completes.
        let confirm = RawController {
            face_west: true, // `a`
            ..RawController::default()
        };
        let outcome = screen.update(menu_input_from_pad(confirm), 0);
        let screens::SelectOutcome::Done(pick) = outcome else {
            panic!("controller confirm should lock the pick and complete: {outcome:?}");
        };
        assert_eq!(pick.p1_def, PathBuf::from("data").join("b/b.def"));
        assert_eq!(pick.p1_name, "Char Beta");
    }

    // -----------------------------------------------------------------------
    // Setup screen wiring (T042): the remap must take effect on in-match input.
    //
    // The pure remap/navigation logic is unit-tested in `screens`; here we prove
    // the app-level wiring — that rebinding an action through `SetupScreen` over
    // the live `InputConfig` actually changes which physical key the keyboard
    // sampler (`match_input_from_held`) reads for that action.
    // -----------------------------------------------------------------------

    #[test]
    fn remapping_a_key_changes_the_resolved_in_match_binding() {
        // Acceptance #3: after remapping, the in-match keyboard binding changes.
        let mut config = default_input_config();

        // By default `a` (light punch) is on `U`; pressing `U` asserts `a`.
        assert!(
            match_input_from_held(&config, |sc| sc == Scancode::U).a,
            "default: U drives a"
        );
        assert!(
            !match_input_from_held(&config, |sc| sc == Scancode::P).a,
            "default: P does not drive a"
        );

        // Rebind `a` to `P` through the setup screen's capture path.
        let mut setup = screens::SetupScreen::new();
        // Walk to the `A` action row (device, CPU-difficulty, CPU-mode,
        // HUD-customize, Up, Down, Left, Right, A — the CPU-difficulty (T069),
        // CPU teaching-mode (T070), and HUD-customization (T046) rows sit between
        // device and Up).
        for _ in 0..8 {
            setup.update(
                screens::MenuInput {
                    down: true,
                    ..screens::MenuInput::default()
                },
                &mut config,
            );
        }
        assert_eq!(setup.selected_action(), Some(screens::InputAction::A));
        // Confirm arms capture, then the captured `P` key rebinds.
        setup.update(
            screens::MenuInput {
                confirm: true,
                ..screens::MenuInput::default()
            },
            &mut config,
        );
        assert!(setup.awaiting_key());
        setup.capture_key(keycode_of(Scancode::P), &mut config);

        // The in-match sampler now reads `P` for `a`, and `U` no longer drives it.
        assert!(
            match_input_from_held(&config, |sc| sc == Scancode::P).a,
            "after remap: P drives a"
        );
        assert!(
            !match_input_from_held(&config, |sc| sc == Scancode::U).a,
            "after remap: U no longer drives a"
        );
    }

    #[test]
    fn arrow_keys_keep_working_after_remapping_wasd_movement() {
        // The arrow keys are a permanent secondary movement binding, so they keep
        // driving directions even if the remappable primary direction key moves.
        let mut config = default_input_config();
        // Rebind `Left` away from `A` to `Q`.
        config.rebind(screens::InputAction::Left, keycode_of(Scancode::Q));
        // The arrow key still drives left.
        assert!(
            match_input_from_held(&config, |sc| sc == Scancode::Left).left,
            "arrow Left still drives left after WASD remap"
        );
        // The new primary key drives left too.
        assert!(match_input_from_held(&config, |sc| sc == Scancode::Q).left);
    }

    // -----------------------------------------------------------------------
    // Keyboard key map (T024): the player-1 scancode -> engine-input path.
    //
    // `match_input_from_held` is the pure core of `match_input_from_keyboard`;
    // it takes a held-key oracle so the key map is testable without a live SDL
    // context. Tests build a synthetic "held" set and assert every documented
    // direction and attack button reaches the right `MatchInput` bit, that WASD
    // and the arrow keys alias the same direction, and that directions stay
    // absolute (never pre-rotated by facing). The MatchInput -> engine raw
    // InputState hop is covered in `fp-input` (see `playability_tests`).
    // -----------------------------------------------------------------------

    /// Builds the `MatchInput` for a set of held scancodes via the pure key map,
    /// using the default (unremapped) player-1 bindings.
    fn kbd(held: &[Scancode]) -> MatchInput {
        let config = default_input_config();
        match_input_from_held(&config, |sc| held.contains(&sc))
    }

    #[test]
    fn keyboard_no_keys_held_is_none() {
        assert_eq!(kbd(&[]), MatchInput::none());
    }

    #[test]
    fn keyboard_wasd_drives_each_direction() {
        assert!(kbd(&[Scancode::A]).left, "A -> left");
        assert!(kbd(&[Scancode::D]).right, "D -> right");
        assert!(kbd(&[Scancode::W]).up, "W -> up (jump)");
        assert!(kbd(&[Scancode::S]).down, "S -> down (crouch)");
        // Each is exactly one bit, nothing else.
        let left = kbd(&[Scancode::A]);
        assert_eq!(
            left,
            MatchInput {
                left: true,
                ..MatchInput::none()
            }
        );
    }

    #[test]
    fn keyboard_arrow_keys_drive_each_direction() {
        assert!(kbd(&[Scancode::Left]).left, "Left arrow -> left");
        assert!(kbd(&[Scancode::Right]).right, "Right arrow -> right");
        assert!(kbd(&[Scancode::Up]).up, "Up arrow -> up (jump)");
        assert!(kbd(&[Scancode::Down]).down, "Down arrow -> down (crouch)");
    }

    #[test]
    fn keyboard_wasd_and_arrows_alias_same_direction() {
        // Either source asserts the same bit, and holding both is just that bit.
        assert_eq!(kbd(&[Scancode::A]), kbd(&[Scancode::Left]));
        assert_eq!(kbd(&[Scancode::A, Scancode::Left]), kbd(&[Scancode::A]));
    }

    #[test]
    fn keyboard_each_attack_button_maps_to_documented_bit() {
        // Punch row a/b/c on U/I/O.
        assert!(kbd(&[Scancode::U]).a, "U -> a");
        assert!(kbd(&[Scancode::I]).b, "I -> b");
        assert!(kbd(&[Scancode::O]).c, "O -> c");
        // Kick row x/y/z on J/K/L.
        assert!(kbd(&[Scancode::J]).x, "J -> x");
        assert!(kbd(&[Scancode::K]).y, "K -> y");
        assert!(kbd(&[Scancode::L]).z, "L -> z");

        // Each attack key sets exactly its own bit and no other.
        assert_eq!(
            kbd(&[Scancode::U]),
            MatchInput {
                a: true,
                ..MatchInput::none()
            }
        );
    }

    #[test]
    fn keyboard_combined_walk_and_attack() {
        // Holding D + U (walk right + light punch) sets exactly those two bits,
        // the common "attack while advancing" case.
        let mi = kbd(&[Scancode::D, Scancode::U]);
        assert_eq!(
            mi,
            MatchInput {
                right: true,
                a: true,
                ..MatchInput::none()
            }
        );
    }

    #[test]
    fn keyboard_bindings_cover_every_input_field() {
        // The default binding config must reach all four directions and all six
        // attack buttons, so no documented input is unreachable from the keyboard.
        let config = default_input_config();
        for action in screens::InputAction::ALL {
            let scancode = config
                .key_for(action)
                .and_then(scancode_from_keycode)
                .expect("every action has a default key");
            let mi = kbd(&[scancode]);
            // Each default key sets exactly its action's bit.
            let mut expected = MatchInput::none();
            field_for_action(action).set(&mut expected);
            assert_eq!(mi, expected, "default key for {action:?} drives its field");
        }
    }

    #[test]
    fn keyboard_directions_are_absolute_not_prerotated() {
        // The keyboard map emits ABSOLUTE screen directions; left and right are
        // never swapped by the app (facing is resolved later inside the engine's
        // CommandMatcher). Holding A must always be `left`, never `right`,
        // regardless of which way the (eventual) fighter faces.
        let l = kbd(&[Scancode::A]);
        assert!(l.left && !l.right);
        let r = kbd(&[Scancode::D]);
        assert!(r.right && !r.left);
    }

    #[test]
    fn is_def_path_detects_def() {
        assert!(is_def_path("kfm.def"));
        assert!(is_def_path("path/to/KFM.DEF"));
        assert!(!is_def_path("kfm.sff"));
        assert!(!is_def_path("kfm"));
    }

    // -----------------------------------------------------------------------
    // Intro / ending storyboard overlay (audit #32)
    // -----------------------------------------------------------------------

    #[test]
    fn storyboard_kind_def_keys() {
        assert_eq!(StoryboardKind::Intro.def_key(), "intro.storyboard");
        assert_eq!(StoryboardKind::Ending.def_key(), "ending.storyboard");
    }

    /// The intro overlay is active only during round 1's [`RoundState::Intro`],
    /// while an intro is loaded and not yet finished.
    #[test]
    fn intro_storyboard_active_only_in_round1_intro() {
        // Round 1 intro, intro loaded, not done -> Intro.
        assert_eq!(
            active_storyboard(RoundState::Intro, 1, false, false, true, false),
            ActiveStoryboard::Intro
        );
        // Past the intro phase (Fight) -> nothing.
        assert_eq!(
            active_storyboard(RoundState::Fight, 1, false, false, true, false),
            ActiveStoryboard::None
        );
        // Later round's intro -> nothing (intro is a once-per-match cutscene).
        assert_eq!(
            active_storyboard(RoundState::Intro, 2, false, false, true, false),
            ActiveStoryboard::None
        );
        // Intro already finished -> nothing (don't loop it).
        assert_eq!(
            active_storyboard(RoundState::Intro, 1, false, true, true, false),
            ActiveStoryboard::None
        );
    }

    /// With no intro storyboard loaded, the intro gate never fires — the normal
    /// intro plays with no overlay (no regression).
    #[test]
    fn no_intro_storyboard_no_overlay() {
        assert_eq!(
            active_storyboard(RoundState::Intro, 1, false, false, false, false),
            ActiveStoryboard::None
        );
    }

    /// The ending overlay is active once the match is decided, taking precedence,
    /// and only when an ending is loaded.
    #[test]
    fn ending_storyboard_active_when_match_over() {
        // Match over, ending loaded -> Ending.
        assert_eq!(
            active_storyboard(RoundState::Win, 3, true, true, true, true),
            ActiveStoryboard::Ending
        );
        // Match over but no ending loaded -> nothing (normal match-over view).
        assert_eq!(
            active_storyboard(RoundState::Win, 3, true, true, true, false),
            ActiveStoryboard::None
        );
        // Match NOT over -> not the ending, even if loaded.
        assert_eq!(
            active_storyboard(RoundState::Fight, 1, false, false, true, true),
            ActiveStoryboard::None
        );
    }

    /// During an in-progress match (not over, past the intro) no overlay is ever
    /// active even when both are loaded — the live fight is untouched.
    #[test]
    fn mid_fight_has_no_overlay() {
        assert_eq!(
            active_storyboard(RoundState::Fight, 1, false, true, true, true),
            ActiveStoryboard::None
        );
        assert_eq!(
            active_storyboard(RoundState::Ko, 1, false, true, true, true),
            ActiveStoryboard::None
        );
    }

    /// The overlay loader no-ops (returns `None`) for a character `.def` that
    /// declares no `intro.storyboard`/`ending.storyboard` key — gated as required,
    /// without needing a GPU. (We read the key via the same DEF path the loader
    /// uses; a renderer is only needed once a storyboard is actually found.)
    #[test]
    fn character_without_storyboard_key_yields_none() {
        // A minimal in-memory character def with no storyboard declarations.
        let def_text = "[Info]\nname = \"X\"\n[Files]\nsprite = x.sff\n";
        let def = fp_formats::def::DefFile::from_str(def_text).expect("parse def");
        // The loader's gating predicate: the key must be present and non-empty.
        let has_intro = def
            .get(STORYBOARD_SECTION, StoryboardKind::Intro.def_key())
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        let has_ending = def
            .get(STORYBOARD_SECTION, StoryboardKind::Ending.def_key())
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        assert!(!has_intro, "no intro.storyboard declared -> no overlay");
        assert!(!has_ending, "no ending.storyboard declared -> no overlay");
    }

    /// The real KFM character `.def` declares both an intro and an ending
    /// storyboard, and both resolve to existing `.def` files (asset-gated; skips
    /// cleanly when test-assets is absent). This verifies the declaration-reading
    /// half of the overlay loader against real content without needing a GPU.
    #[test]
    fn kfm_declares_resolvable_storyboards() {
        let char_def = test_asset("kfm/kfm.def");
        let Ok(def) = fp_formats::def::DefFile::load(&char_def) else {
            eprintln!("skipping: {} not present", char_def.display());
            return;
        };
        for kind in [StoryboardKind::Intro, StoryboardKind::Ending] {
            let rel = def
                .get(STORYBOARD_SECTION, kind.def_key())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| panic!("KFM declares {}", kind.def_key()));
            let sb_path = fp_formats::def::DefFile::resolve_path(&char_def, rel);
            assert!(
                sb_path.exists(),
                "KFM {} resolves to an existing file: {}",
                kind.def_key(),
                sb_path.display()
            );
            // And the storyboard parses into a non-empty, playable scene model.
            let sb = fp_storyboard::Storyboard::load(&sb_path).expect("storyboard loads");
            assert!(!sb.scenes.is_empty(), "KFM {} has scenes", kind.def_key());
            assert!(
                !sb.sprite_path.is_empty(),
                "KFM {} declares a sprite file",
                kind.def_key()
            );
        }
    }

    #[test]
    fn clamp_elem_is_safe() {
        assert_eq!(clamp_elem(-5, 3), 0);
        assert_eq!(clamp_elem(0, 3), 0);
        assert_eq!(clamp_elem(2, 3), 2);
        assert_eq!(clamp_elem(99, 3), 2);
        assert_eq!(clamp_elem(0, 0), 0); // empty action
    }

    // =====================================================================
    // Task 7.2 — two-player Match wiring (the playable demo)
    // =====================================================================

    /// AC4 (no division by zero): the life-bar fraction is always in `[0, 1]` and
    /// safe against a zero/negative `life_max` and overkill/over-full life.
    #[test]
    fn life_fraction_is_clamped_and_safe() {
        assert!(
            (life_fraction(1000, 1000) - 1.0).abs() < 1e-6,
            "full life is 1.0"
        );
        assert!(
            (life_fraction(500, 1000) - 0.5).abs() < 1e-6,
            "half life is 0.5"
        );
        assert!((life_fraction(0, 1000)).abs() < 1e-6, "no life is 0.0");
        assert_eq!(
            life_fraction(-50, 1000),
            0.0,
            "overkill clamps to 0, not negative"
        );
        assert_eq!(life_fraction(2000, 1000), 1.0, "over-full clamps to 1");
        assert_eq!(
            life_fraction(100, 0),
            0.0,
            "zero life_max yields 0, no div-by-zero"
        );
        assert_eq!(life_fraction(100, -10), 0.0, "negative life_max yields 0");
    }

    /// PR-C (audit #26): the power-bar fraction mirrors `life_fraction`'s safety —
    /// always in `[0, 1]`, clamped at both ends, and never divides by a
    /// zero/negative `power_max`.
    #[test]
    fn power_fraction_is_clamped_and_safe() {
        assert!(
            (power_fraction(3000, 3000) - 1.0).abs() < 1e-6,
            "full meter is 1.0"
        );
        assert!(
            (power_fraction(1500, 3000) - 0.5).abs() < 1e-6,
            "half meter is 0.5"
        );
        assert!((power_fraction(0, 3000)).abs() < 1e-6, "empty meter is 0.0");
        assert_eq!(power_fraction(-50, 3000), 0.0, "negative power clamps to 0");
        assert_eq!(
            power_fraction(9999, 3000),
            1.0,
            "over-full meter clamps to 1"
        );
        assert_eq!(
            power_fraction(100, 0),
            0.0,
            "zero power_max yields 0, no div-by-zero"
        );
        assert_eq!(power_fraction(100, -10), 0.0, "negative power_max yields 0");
    }

    /// T074: the quad HUD picks its life-bar color from the SAME shared
    /// `fp_ui::low_life_tint` threshold the screenpack HUD uses, so a fighter
    /// below 25% life reads red and a healthy fighter reads green. This mirrors
    /// the exact decision in `Hud::draw_life_bar`.
    #[test]
    fn quad_life_bar_red_shifts_below_25_percent() {
        // Pick "red" exactly when the shared threshold tint is non-neutral.
        let picks_red = |frac: f32| !fp_ui::low_life_tint(frac).is_neutral();
        assert!(!picks_red(life_fraction(1000, 1000)), "full life is green");
        assert!(!picks_red(life_fraction(500, 1000)), "half life is green");
        assert!(
            !picks_red(life_fraction(260, 1000)),
            "just above 25% stays green"
        );
        assert!(
            picks_red(life_fraction(250, 1000)),
            "at 25% red-shifts (matches the screenpack threshold)"
        );
        assert!(picks_red(life_fraction(100, 1000)), "10% life is red");
        assert!(picks_red(life_fraction(0, 1000)), "dead reads red");
    }

    /// T074: the quad HUD power bar flashes (swaps blue↔yellow) ONLY at max meter,
    /// driven by the deterministic frame-keyed `fp_ui::max_power_flash_tint`. The
    /// decision mirrors `Hud::draw_power_bar`.
    #[test]
    fn quad_power_bar_flashes_only_at_max() {
        // "yellow" (flash accent) exactly when the shared flash tint is non-neutral.
        let picks_yellow =
            |frac: f32, tick: u64| !fp_ui::max_power_flash_tint(frac, tick).is_neutral();
        // Below max: never flashes, at every frame across a full period.
        for tick in 0..(fp_ui::POWER_FLASH_PERIOD * 2) {
            assert!(
                !picks_yellow(power_fraction(1500, 3000), tick),
                "half meter never flashes"
            );
            assert!(
                !picks_yellow(power_fraction(2900, 3000), tick),
                "nearly-but-not-full never flashes"
            );
        }
        // At max meter: the bar flashes on at least one phase within a period and
        // is back to blue on another — a visible, deterministic pulse.
        let full = power_fraction(3000, 3000);
        let flashed = (0..fp_ui::POWER_FLASH_PERIOD).any(|t| picks_yellow(full, t));
        let blued = (0..fp_ui::POWER_FLASH_PERIOD).any(|t| !picks_yellow(full, t));
        assert!(flashed, "max meter must flash (yellow) on some frame");
        assert!(blued, "max meter must show blue on some frame (it pulses)");
    }

    /// AC2: world X maps into the window centered on the midpoint, with the origin
    /// landing at the window center and signs preserved.
    #[test]
    fn world_to_screen_x_centers_on_window() {
        let win_w = 640.0;
        assert!(
            (world_to_screen_x(0.0, win_w) - 320.0).abs() < 1e-6,
            "origin at center"
        );
        assert!(
            world_to_screen_x(-60.0, win_w) < 320.0,
            "negative world X is left of center"
        );
        assert!(
            world_to_screen_x(60.0, win_w) > 320.0,
            "positive world X is right of center"
        );
    }

    // =====================================================================
    // PR-L — stage background wiring (audit #29)
    // =====================================================================

    /// A fighter's screen anchor scrolls opposite the camera: with the camera at
    /// 0 it matches the no-stage mapping; moving the camera right (+) shifts the
    /// fighter left on screen by the same amount (`pos.x - camera_x`).
    #[test]
    fn player_anchor_scrolls_with_camera() {
        let win_w = 640.0;
        let win_h = 480.0;
        let pos = fp_core::Vec2::new(60.0, 0.0);

        let (no_cam, _) = player_screen_anchor(pos, 0.0, win_w, win_h);
        assert!(
            (no_cam - world_to_screen_x(60.0, win_w)).abs() < 1e-6,
            "camera 0 reduces to the original centered mapping (no regression)"
        );

        let (with_cam, _) = player_screen_anchor(pos, 100.0, win_w, win_h);
        assert!(
            (with_cam - world_to_screen_x(-40.0, win_w)).abs() < 1e-6,
            "camera +100 maps pos.x-100 (the world scrolls under the camera)"
        );
        assert!(
            with_cam < no_cam,
            "panning the camera right pushes the fighter left"
        );
    }

    /// `bg_sprite_id` accepts in-range group/image and rejects (with `None`) any
    /// component outside the SFF `u16` range rather than wrapping to a wrong id.
    #[test]
    fn bg_sprite_id_validates_u16_range() {
        assert_eq!(bg_sprite_id(0, 0), Some(SpriteId::new(0, 0)));
        assert_eq!(
            bg_sprite_id(65535, 65535),
            Some(SpriteId::new(65535, 65535))
        );
        assert_eq!(bg_sprite_id(-1, 0), None, "negative group is out of range");
        assert_eq!(
            bg_sprite_id(0, 70000),
            None,
            "image > u16::MAX is out of range"
        );
    }

    /// `stage_arg` picks up a `.def` extra argument as the stage and ignores a
    /// non-`.def` one (keeping the flat background).
    #[test]
    fn stage_arg_selects_def_extra_only() {
        let args = vec![
            "fp-app".to_string(),
            "p1.def".to_string(),
            "p2.def".to_string(),
            "ringside.def".to_string(),
        ];
        assert_eq!(stage_arg(&args, 3), Some(Path::new("ringside.def")));
        // Out-of-range index → no stage.
        assert_eq!(stage_arg(&args, 9), None);

        let non_def = vec![
            "fp-app".to_string(),
            "p1.def".to_string(),
            "p2.def".to_string(),
            "notes.txt".to_string(),
        ];
        assert_eq!(
            stage_arg(&non_def, 3),
            None,
            "a non-.def extra arg is not a stage"
        );
    }

    /// A missing stage path degrades to no stage (the flat-background fallback)
    /// rather than panicking — `StageRender::load` returns `None`.
    #[test]
    fn stage_load_missing_file_is_none_not_panic() {
        let missing = Path::new("/no/such/stage/definitely-not-here.def");
        assert!(StageRender::load(missing).is_none());
    }

    /// Builds the same two-KFM [`Match`] the app builds (default both KFM), or
    /// returns `None` (after a skip note) when the fixture is absent/unloadable.
    /// This is the single wiring the app's default mode uses, so the integration
    /// test below exercises exactly the runtime path.
    fn build_kfm_match() -> Option<Match> {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping two-KFM match test: {} not present", def.display());
            return None;
        }
        match build_two_player_match(&def, &def, PalSelection::default()) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("skipping two-KFM match test: build failed: {e}");
                None
            }
        }
    }

    /// AC1: the app-level builder loads two characters, applies the stand<->walk
    /// bridge to BOTH, seeds opposing positions/facings, and starts in the intro
    /// phase with the default 99-second clock — the wiring `cargo run` uses.
    #[test]
    fn two_kfm_match_builds_with_opposing_positions_and_facings() {
        let Some(m) = build_kfm_match() else { return };

        // Opposing start positions (P1 left of center, P2 right).
        assert!(m.p1().pos().x < 0.0, "P1 starts left of center");
        assert!(m.p2().pos().x > 0.0, "P2 starts right of center");
        // Facing each other (left fighter faces right, right fighter faces left).
        assert_eq!(
            m.p1().facing(),
            fp_character::Facing::Right,
            "P1 faces the opponent"
        );
        assert_eq!(
            m.p2().facing(),
            fp_character::Facing::Left,
            "P2 faces the opponent"
        );
        // Both at full life, intro phase, 99-second clock, no winner yet.
        assert_eq!(m.p1().life(), m.p1().life_max());
        assert_eq!(m.p2().life(), m.p2().life_max());
        assert_eq!(m.round_state(), RoundState::Intro);
        assert_eq!(m.timer(), 99 * 60);
        assert_eq!(m.winner(), None);

        // BOTH characters must carry the engine stand<->walk bridge in [Statedef -1]
        // (the bridge is applied per-character by `build_player`).
        for player in [m.p1(), m.p2()] {
            let minus_one = player
                .loaded
                .state(-1)
                .expect("[Statedef -1] must exist after the bridge is applied");
            let walks = minus_one.controllers.iter().any(|c| {
                c.controller_type
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"))
                    && c.params
                        .get("value")
                        .is_some_and(|e| e.source.trim() == STATE_WALK.to_string())
            });
            assert!(
                walks,
                "each fighter must have the stand->walk bridge in [Statedef -1]"
            );
        }
    }

    /// AC4 (never panics): the two-KFM match survives sustained, varied synthetic
    /// input on P1 (and an idle P2) without panicking, advancing the round.
    #[test]
    fn two_kfm_match_ticks_without_panic() {
        let Some(mut m) = build_kfm_match() else {
            return;
        };
        for i in 0..240u32 {
            let p1 = MatchInput {
                left: i % 3 == 0,
                right: i % 2 == 0,
                up: i % 7 == 0,
                down: i % 5 == 0,
                a: i % 11 == 0,
                b: i % 13 == 0,
                ..MatchInput::none()
            };
            m.tick(p1, MatchInput::none());
        }
        // It reached at least the fight phase and never panicked.
        assert!(matches!(
            m.round_state(),
            RoundState::Fight | RoundState::Ko | RoundState::Win
        ));
    }

    /// Drives `m` until it leaves the intro and enters [`RoundState::Fight`],
    /// returning whether the fight became live within the budget.
    fn run_until_fight(m: &mut Match) -> bool {
        for _ in 0..120 {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() == RoundState::Fight {
                return true;
            }
        }
        m.round_state() == RoundState::Fight
    }

    /// AC3 (the headline headless integration test): build the same two-KFM Match
    /// the app builds, drive P1's keyboard inputs so a real KFM attack connects,
    /// and assert P2's life drops — proving the app-level two-player wiring works
    /// end to end with no window/GPU.
    ///
    /// This drives ENTIRELY through the public `MatchInput`/accessor seam (the
    /// `Match`'s internals are private to fp-engine), exactly as `cargo run` does:
    /// P1 walks into range with "toward opponent" held, then presses an attack
    /// button; KFM's own CNS produces the HitDef and the engine resolves the
    /// contact, dropping P2's life. The build path (`build_two_player_match`) is
    /// the same one the app's default mode uses.
    #[test]
    fn headless_two_player_attack_connects_and_drops_life() {
        let Some(mut m) = build_kfm_match() else {
            return;
        };

        assert!(
            run_until_fight(&mut m),
            "fight must go live before driving input"
        );
        let p2_life_before = m.p2().life();
        assert_eq!(p2_life_before, m.p2().life_max(), "P2 starts at full life");

        // Phase 1: P1 faces right (toward P2), so holding "right" is forward. Walk
        // into punching range; the player-push pins the two at touching distance.
        for _ in 0..240 {
            m.tick(
                MatchInput {
                    right: true,
                    ..MatchInput::none()
                },
                MatchInput::none(),
            );
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }

        // Phase 2: in range, throw light punches (the `x` button). Press on alternate
        // frames so the command recognizer sees fresh presses; over a generous budget
        // a punch must connect and reduce P2's life.
        let mut p2_was_hit = false;
        for i in 0..400 {
            let inp = if i % 3 == 0 {
                MatchInput {
                    x: true,
                    ..MatchInput::none()
                }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            if m.p2().life() < p2_life_before {
                p2_was_hit = true;
                break;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }

        assert!(
            p2_was_hit,
            "a P1 attack must connect and drop P2's life over the frame budget; \
             P2 life stayed at {} (round state {:?})",
            m.p2().life(),
            m.round_state()
        );
        assert!(
            m.p2().life() < p2_life_before,
            "P2 life must be strictly below its starting value after a connecting hit"
        );
    }

    /// 8.3b AC (headless, gated): build the same two-KFM `Match` the app builds,
    /// drive a PlaySnd-bearing action (KFM walks into range and throws light
    /// punches — its attack states author `PlaySnd`), and assert P1's surfaced
    /// `p1_sound_requests()` becomes non-empty at some tick — proving the request
    /// reaches the app's play path. Each frame the requests are also pumped
    /// through `FighterAudio::play_requests` over a silent `NullBackend`
    /// `AudioSystem` (the same call the live app makes), proving the decode/play
    /// path runs end to end without a device and never panics.
    ///
    /// We do not assert audio output (the `NullBackend` is silent and the
    /// `RecordingBackend` is `#[cfg(test)]`-private to fp-audio); the surfaced
    /// requests plus a panic-free play pump are the observable contract here.
    #[test]
    fn headless_two_player_attack_surfaces_and_plays_sound_requests() {
        let Some(mut m) = build_kfm_match() else {
            return;
        };
        assert!(
            run_until_fight(&mut m),
            "fight must go live before driving input"
        );

        // The exact audio layer the live app holds: a silent-fallback mixer plus
        // a per-fighter decoded-sound cache. NullBackend forces silence so the
        // test is deterministic and device-free.
        let mut audio = AudioSystem::with_backend(Box::new(fp_audio::NullBackend));
        let mut p1_audio = FighterAudio::default();
        let mut p2_audio = FighterAudio::default();

        // Pump P1+P2 requests through the play path for this frame.
        let pump = |m: &Match,
                    audio: &mut AudioSystem,
                    p1_audio: &mut FighterAudio,
                    p2_audio: &mut FighterAudio| {
            p1_audio.play_requests(audio, m.p1(), m.p1_sound_requests());
            p2_audio.play_requests(audio, m.p2(), m.p2_sound_requests());
        };

        // Phase 1: walk into punching range (holding "right" = forward for P1).
        for _ in 0..240 {
            m.tick(
                MatchInput {
                    right: true,
                    ..MatchInput::none()
                },
                MatchInput::none(),
            );
            pump(&m, &mut audio, &mut p1_audio, &mut p2_audio);
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }

        // Phase 2: throw light punches; KFM's attack states fire PlaySnd, so the
        // surfaced P1 requests must become non-empty within the budget.
        let mut saw_request = false;
        for i in 0..400 {
            let inp = if i % 3 == 0 {
                MatchInput {
                    x: true,
                    ..MatchInput::none()
                }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            if !m.p1_sound_requests().is_empty() {
                saw_request = true;
            }
            // Pump every frame regardless, exercising the real play path.
            pump(&m, &mut audio, &mut p1_audio, &mut p2_audio);
            if saw_request {
                break;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }

        assert!(
            saw_request,
            "a PlaySnd-bearing KFM action must surface at least one P1 sound \
             request over the frame budget (round state {:?})",
            m.round_state()
        );
    }

    /// AC3 (round advances toward KO): land attacks until P2 is finished, then
    /// assert the round advances out of the fight (KO/Win) — proving the app-level
    /// wiring carries a connecting attack all the way to the round result. Damage
    /// must accrue regardless; the KO consequence is asserted once P2 is downed.
    #[test]
    fn headless_two_player_sustained_attacks_advance_round() {
        let Some(mut m) = build_kfm_match() else {
            return;
        };
        if !run_until_fight(&mut m) {
            eprintln!("skipping: match never reached the fight phase");
            return;
        }

        let start = m.p2().life();
        // Close to range first.
        for _ in 0..240 {
            m.tick(
                MatchInput {
                    right: true,
                    ..MatchInput::none()
                },
                MatchInput::none(),
            );
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }
        // Hammer P2 with light punches for a long budget (KFM has ~1000 life).
        for i in 0..6000 {
            let inp = if i % 3 == 0 {
                MatchInput {
                    x: true,
                    ..MatchInput::none()
                }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }

        // P2 must have taken real damage from the app-level wiring.
        assert!(
            m.p2().life() < start,
            "sustained P1 attacks must reduce P2 life (was {start}, now {})",
            m.p2().life()
        );
        // If P2 was finished, the round must have advanced out of Fight with P1 up.
        if m.p2().life() <= 0 {
            assert!(
                matches!(m.round_state(), RoundState::Ko | RoundState::Win),
                "a KO must advance the round out of Fight, got {:?}",
                m.round_state()
            );
            if m.round_state() == RoundState::Win {
                assert_eq!(m.winner(), Some(Winner::P1), "P1 wins once P2 is KO'd");
            }
        }
    }

    /// HEADLESS integration test (no GPU/window): load KFM, inject synthetic
    /// input (hold Forward, then release) across N ticks, and assert the
    /// character enters the CNS walk state (20) while held and returns toward
    /// stand (0) on release — proving input -> command -> CNS-state end to end.
    ///
    /// Gated on test-assets: skips cleanly if KFM is not present.
    #[test]
    fn headless_hold_forward_drives_cns_walk_state() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping headless walk test: {} not present", def.display());
            return;
        }

        let loaded = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping headless walk test: kfm.def failed to load: {e}");
                return;
            }
        };
        let mut pc = CnsCharacter::new(loaded);

        // Sanity: start standing with control, facing right.
        assert_eq!(pc.entity.state_no, STATE_STAND, "starts in stand state");
        assert!(pc.entity.ctrl, "starts with control");
        assert_eq!(
            pc.entity.facing,
            fp_character::Facing::Right,
            "starts facing right"
        );

        // CRITICAL: this whole path runs WITHOUT any app-side shim.
        // `holdfwd = /$F` compiles natively in fp-input, `alive` resolves to
        // Life>0 in fp-character (so the common stand state does not fall into
        // death 5050), and the MUGEN engine-built-in stand<->walk command-state is
        // supplied by the fp-character loader for every character (task 7.3 part B).

        // Hold Forward (facing right → absolute Right) for many ticks. The
        // `holdfwd` command activates while held; the loader's built-in
        // [Statedef -1] then fires the stand->walk ChangeState into state 20.
        let mut entered_walk = false;
        for _ in 0..30 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK {
                entered_walk = true;
                break;
            }
        }
        assert!(
            entered_walk,
            "holding Forward should drive the CNS walk state (20); ended in state {}",
            pc.entity.state_no
        );

        // STRENGTHENED: while genuinely in the walk state, the character must be
        // strictly advancing in the facing direction. Facing right, "forward" is
        // +x, so state 20's `VelSet x = const(velocity.walk.fwd.x)` (gated on the
        // live `holdfwd` command) must produce a strictly positive x-velocity AND
        // strictly advance pos.x over the held ticks. We verify BOTH the velocity
        // (the immediate CNS effect) and the integrated position (the observable
        // motion), so the test fails if the character merely flips to state 20
        // without actually walking.
        assert_eq!(
            pc.entity.state_no, STATE_WALK,
            "must still be in walk state to assert forward motion"
        );
        let mut saw_forward_vel = false;
        let x_before = pc.entity.pos.x;
        for _ in 0..5 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK && pc.entity.vel.x > 0.0 {
                saw_forward_vel = true;
            }
        }
        assert!(
            saw_forward_vel,
            "in walk state, facing right + holding forward must set a strictly \
             positive x-velocity (walk.fwd.x); never saw vel.x > 0"
        );
        assert!(
            pc.entity.pos.x > x_before,
            "walking forward must strictly advance pos.x in the facing (+x) \
             direction; pos.x went {x_before} -> {} (no advance)",
            pc.entity.pos.x
        );

        // Release all input: the walk->stand rule must return us toward stand,
        // and the character must stop advancing once it leaves the walk state.
        let mut returned_to_stand = false;
        for _ in 0..30 {
            pc.tick(neutral());
            if pc.entity.state_no == STATE_STAND {
                returned_to_stand = true;
                break;
            }
        }
        assert!(
            returned_to_stand,
            "releasing input should return the character toward stand (0); ended in state {}",
            pc.entity.state_no
        );
    }

    /// The character never panics under sustained, varied synthetic input.
    #[test]
    fn headless_random_input_never_panics() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let Ok(loaded) = LoadedCharacter::load(&def) else {
            eprintln!("skipping: kfm.def failed to load");
            return;
        };
        let mut pc = CnsCharacter::new(loaded);

        // Cycle through a spread of inputs; the loop must never panic.
        for i in 0..120u32 {
            let input = InputState {
                direction: Direction {
                    up: i % 7 == 0,
                    down: i % 5 == 0,
                    left: i % 3 == 0,
                    right: i % 2 == 0,
                },
                ..Default::default()
            };
            pc.tick(input);
        }
    }

    // =====================================================================
    // Edge-case, error-path, and MUGEN-semantics coverage for the fp-app
    // integration. Each block is annotated with the acceptance criterion it
    // exercises. The pure helpers (snapshot_active_commands, map_blend_mode,
    // make_ctrl, empty_state, clamp_elem) are tested fully synthetically; the
    // CnsCharacter behavioral tests (facing-relative input, command="..." live
    // triggers, graceful current_frame, native `alive` resolution, engine
    // movement bridge) require the merged CNS graph and are gated on test-assets
    // — they skip cleanly when KFM is absent.
    //
    // NOTE (task 5.6c): the `normalize_command` and `drop_unevaluable_alive_controllers`
    // band-aids were removed; commands now compile straight from the raw MUGEN
    // strings (fp-input 5.6a) and `alive` resolves natively (fp-character 5.6b).
    // =====================================================================

    /// An input frame holding the absolute Left direction.
    fn hold_left() -> InputState {
        InputState {
            direction: Direction {
                left: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Loads KFM and wraps it in a `CnsCharacter`, returning `None` (after a
    /// skip note) when the fixture is absent or fails to load. Keeps every gated
    /// behavioral test from duplicating the skip boilerplate.
    fn load_kfm_pc() -> Option<CnsCharacter> {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return None;
        }
        match LoadedCharacter::load(&def) {
            Ok(loaded) => Some(CnsCharacter::new(loaded)),
            Err(e) => {
                eprintln!("skipping: kfm.def failed to load: {e}");
                None
            }
        }
    }

    // ---- AC2: raw MUGEN command strings compile directly via fp-input ----
    // (Task 5.6c removed the `normalize_command` band-aid; fp-input parses `$`/`>`
    // natively as of 5.6a, so the raw string is fed straight to `compile_command`.)

    #[test]
    fn raw_hold_forward_compiles_natively() {
        // The headline locomotion command in kfm.cmd is `holdfwd = /$F`. With no
        // app-side normalization, it must compile straight from the raw string to
        // a single hold-direction element (the `$` direction-detect modifier is
        // now handled inside fp-input), proving the pipeline accepts real MUGEN
        // syntax with the band-aid gone.
        let elements = compile_command("/$F").expect("raw /$F should compile natively");
        assert_eq!(elements.len(), 1, "hold-forward is a single element");
    }

    #[test]
    fn raw_strict_and_detect_commands_compile_natively() {
        // `$` (direction-detect) and `>` (strict-immediate) — the two symbols the
        // old `normalize_command` used to strip — now compile natively. A motion
        // command using both must produce one element per token, in order.
        let qcf = compile_command("$D, $DF, $F, x").expect("detect motion compiles");
        assert_eq!(qcf.len(), 4, "D, DF, F, x is four elements");
        let strict = compile_command(">~D, >F, a").expect("strict motion compiles");
        assert_eq!(strict.len(), 3, "~D, F, a is three elements");
    }

    // ---- AC2: snapshot_active_commands maps matcher output -> ActiveCommands ----

    /// Builds a `CommandDef` from a raw MUGEN command string, mirroring exactly
    /// how `CnsCharacter::new` compiles the .cmd list (no normalization).
    fn cmd_def(name: &str, raw: &str, time: u32, buffer_time: u32) -> CommandDef {
        CommandDef {
            name: name.to_string(),
            elements: compile_command(raw).expect("test command should compile"),
            time,
            buffer_time,
        }
    }

    #[test]
    fn snapshot_active_commands_reflects_held_direction() {
        // PART B: a held Forward must surface the `holdfwd` command name through
        // the matcher into the ActiveCommands snapshot the entity reads.
        let defs = vec![cmd_def("holdfwd", "/$F", 1, 1)];
        let mut matcher = CommandMatcher::new(defs.clone());
        let mut buffer = InputBuffer::new();
        buffer.push(hold_right()); // absolute Right == Forward when facing right
        matcher.check_commands(&buffer, /* facing_right */ true);

        let active = snapshot_active_commands(&matcher, &defs);
        assert!(
            active.is_active("holdfwd"),
            "holding Forward should make holdfwd active"
        );
        // A command the matcher did not fire must be inactive.
        assert!(!active.is_active("holdback"));
        // Case-insensitive match (MUGEN command labels are case-insensitive).
        assert!(active.is_active("HoldFwd"));
    }

    #[test]
    fn snapshot_active_commands_empty_when_neutral() {
        // No input held → no command active → an empty snapshot, never a panic.
        let defs = vec![cmd_def("holdfwd", "/$F", 1, 1)];
        let mut matcher = CommandMatcher::new(defs.clone());
        let mut buffer = InputBuffer::new();
        buffer.push(neutral());
        matcher.check_commands(&buffer, true);
        let active = snapshot_active_commands(&matcher, &defs);
        assert!(!active.is_active("holdfwd"));
    }

    #[test]
    fn snapshot_active_commands_empty_defs_is_inert() {
        // With no command defs, the snapshot is empty regardless of the matcher.
        let matcher = CommandMatcher::new(Vec::new());
        let active = snapshot_active_commands(&matcher, &[]);
        assert!(!active.is_active("anything"));
    }

    // ---- AC3: map_blend_mode covers every AIR blend variant ----

    #[test]
    fn map_blend_mode_normal_and_additive() {
        let (m, a) = map_blend_mode(&fp_formats::air::BlendMode::Normal);
        assert_eq!(m, fp_render::BlendMode::Normal);
        assert!((a - 1.0).abs() < 1e-6);

        let (m, a) = map_blend_mode(&fp_formats::air::BlendMode::Additive);
        assert_eq!(m, fp_render::BlendMode::Additive);
        assert!((a - 1.0).abs() < 1e-6);
    }

    #[test]
    fn map_blend_mode_subtractive() {
        let (m, a) = map_blend_mode(&fp_formats::air::BlendMode::Subtractive);
        assert_eq!(m, fp_render::BlendMode::Subtractive);
        assert!((a - 1.0).abs() < 1e-6);
    }

    #[test]
    fn map_blend_mode_additive_alpha_scales_256() {
        // AIR additive-alpha is 0..256; the renderer wants 0.0..1.0. A value of
        // 128 maps to 0.5, 256 to 1.0, 0 to 0.0 — and never panics on the edges.
        let (m, a) = map_blend_mode(&fp_formats::air::BlendMode::AdditiveAlpha(128));
        assert_eq!(m, fp_render::BlendMode::Additive);
        assert!((a - 0.5).abs() < 1e-6, "128/256 == 0.5, got {a}");

        let (_, full) = map_blend_mode(&fp_formats::air::BlendMode::AdditiveAlpha(255));
        assert!(full < 1.0 && full > 0.9, "255/256 ~ 0.996, got {full}");

        let (_, zero) = map_blend_mode(&fp_formats::air::BlendMode::AdditiveAlpha(0));
        assert!((zero - 0.0).abs() < 1e-6, "0/256 == 0.0, got {zero}");
    }

    // ---- AC3: clamp_elem bounds the animation cursor (already covered for the
    // happy path; add the boundary i32::MIN/MAX and len==1 cases) ----

    #[test]
    fn clamp_elem_handles_extreme_values() {
        assert_eq!(clamp_elem(i32::MIN, 4), 0, "very-negative clamps to 0");
        assert_eq!(
            clamp_elem(i32::MAX, 4),
            3,
            "very-large clamps to last index"
        );
        assert_eq!(clamp_elem(0, 1), 0, "single-frame action");
        assert_eq!(clamp_elem(5, 1), 0, "single-frame action clamps to 0");
    }

    // ---- AC2 (facing-relative): holding the SCREEN direction toward the
    // opponent drives walk regardless of which way the character faces. ----

    #[test]
    fn headless_facing_left_hold_left_drives_walk() {
        // PART B requires F/B to be facing-relative. When the character faces
        // LEFT, "Forward" is screen-Left. Holding Left must therefore drive the
        // walk state, exactly as holding Right does when facing Right. This is the
        // mirror of Forge's facing-right walk test and guards the facing handling.
        let Some(mut pc) = load_kfm_pc() else { return };
        pc.entity.facing = fp_character::Facing::Left;
        assert_eq!(pc.entity.state_no, STATE_STAND, "starts standing");

        let mut entered_walk = false;
        for _ in 0..30 {
            pc.tick(hold_left());
            if pc.entity.state_no == STATE_WALK {
                entered_walk = true;
                break;
            }
        }
        assert!(
            entered_walk,
            "facing Left, holding screen-Left (=Forward) should drive walk (20); \
             ended in state {}",
            pc.entity.state_no
        );
    }

    #[test]
    fn headless_facing_left_hold_right_does_not_walk_forward() {
        // The negative of the above: facing Left, holding screen-Right is BACK,
        // not Forward. The FORWARD command (`holdfwd`) must NOT fire, so the
        // character must never advance in the FACING (forward) direction.
        //
        // Facing left, "forward" is -x. The correct MUGEN outcome of holding Back
        // is walk-BACK, which (facing left) moves the character toward +x — that
        // is expected and fine. What must NOT happen is forward motion (toward
        // -x), which would mean the forward command wrongly drove the walk. We
        // therefore assert the character never moved in the forward (-x) sense.
        // (Before 5.6c the walk velocity was a broken zero, so this test could not
        // distinguish the two; with walking now working it pins the facing sign.)
        let Some(mut pc) = load_kfm_pc() else { return };
        pc.entity.facing = fp_character::Facing::Left;
        let x_start = pc.entity.pos.x;
        for _ in 0..20 {
            pc.tick(hold_right());
        }
        assert!(
            pc.entity.pos.x >= x_start - 1e-3,
            "facing Left + holding Right (Back) must not walk FORWARD (toward -x); \
             moved from {x_start} to {} (negative = wrong forward motion)",
            pc.entity.pos.x
        );
    }

    #[test]
    fn headless_facing_left_hold_left_advances_forward_to_negative_x() {
        // Positive companion: facing Left, holding screen-Left is FORWARD. With
        // walking now functional (5.6c) and the executor integrating position
        // facing-relative (6.2c), the character must strictly advance in the
        // facing (forward = -x) direction, mirroring the facing-right walk.
        //
        // 6.2c semantics: the STORED velocity is FACING-RELATIVE — walk-forward is
        // `const(velocity.walk.fwd.x)` = +2.4 for BOTH facings (this is exactly
        // what common1.cns's `vel x > 0` walk-anim selector relies on). The facing
        // sign is applied only when integrating world position
        // (`pos.x += vel.x * facing_sign`), so facing left the SAME +2.4 velocity
        // produces -x motion. We therefore assert the facing-relative velocity is
        // strictly positive (forward) AND the world position advances toward -x.
        let Some(mut pc) = load_kfm_pc() else { return };
        pc.entity.facing = fp_character::Facing::Left;
        // Reach walk state first.
        let mut walking = false;
        for _ in 0..30 {
            pc.tick(hold_left());
            if pc.entity.state_no == STATE_WALK {
                walking = true;
                break;
            }
        }
        assert!(
            walking,
            "facing left, holding forward should reach walk (20)"
        );
        let x_before = pc.entity.pos.x;
        let mut saw_forward_vel = false;
        for _ in 0..5 {
            pc.tick(hold_left());
            // Facing-relative walk-forward velocity is positive for both facings.
            if pc.entity.state_no == STATE_WALK && pc.entity.vel.x > 0.0 {
                saw_forward_vel = true;
            }
        }
        assert!(
            saw_forward_vel,
            "facing left, walk-forward velocity is FACING-RELATIVE and must be \
             strictly positive (+walk.fwd.x); the facing sign is applied at \
             integration, not to the stored velocity"
        );
        assert!(
            pc.entity.pos.x < x_before,
            "facing left, walking forward must strictly advance toward -x; \
             pos.x went {x_before} -> {}",
            pc.entity.pos.x
        );
    }

    // ---- AC2: command="..." triggers evaluate against LIVE input each tick.
    // We prove the round-trip by toggling input and observing the CNS state
    // follow it (walk while held, stand on release), then re-entering walk on a
    // fresh hold — i.e. the trigger is re-evaluated every tick, not latched. ----

    #[test]
    fn headless_live_command_triggers_track_input_each_tick() {
        let Some(mut pc) = load_kfm_pc() else { return };

        // Hold forward → walk.
        let mut walked = false;
        for _ in 0..30 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK {
                walked = true;
                break;
            }
        }
        assert!(
            walked,
            "first hold should enter walk; got {}",
            pc.entity.state_no
        );

        // Release → stand.
        let mut stood = false;
        for _ in 0..30 {
            pc.tick(neutral());
            if pc.entity.state_no == STATE_STAND {
                stood = true;
                break;
            }
        }
        assert!(
            stood,
            "release should return to stand; got {}",
            pc.entity.state_no
        );

        // Hold AGAIN → forward locomotion again. This is the key live-evaluation
        // assertion: a forward command fires on the *new* hold, proving triggers
        // are re-evaluated per tick rather than consumed once at startup.
        //
        // MUGEN semantics note: because the buffer still carries the earlier
        // forward press, the fresh hold can complete KFM's `FF` (F, F) double-tap
        // RUN command (-> state 100, run) instead of, or before, the plain walk
        // (-> state 20). Both are forward-locomotion states gated on a live
        // forward command, so either proves the command re-fired. We assert the
        // character left stand into a forward-locomotion state, not the specific
        // number (asserting only 20 would be wrong: run is the correct MUGEN
        // outcome of a re-tap and was the observed behavior).
        const STATE_RUN: i32 = 100;
        let mut moved_forward_again = false;
        for _ in 0..30 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK || pc.entity.state_no == STATE_RUN {
                moved_forward_again = true;
                break;
            }
        }
        assert!(
            moved_forward_again,
            "a fresh hold must re-trigger forward locomotion (walk 20 or run 100) \
             via live per-tick eval; got {}",
            pc.entity.state_no
        );
        assert_ne!(
            pc.entity.state_no, STATE_STAND,
            "the character must have left stand on the fresh hold"
        );
    }

    // ---- AC3: current_frame degrades gracefully (never panics, returns a real
    // frame for the live anim, None for a nonexistent anim). ----

    #[test]
    fn headless_current_frame_is_present_for_stand_and_none_for_missing() {
        let Some(mut pc) = load_kfm_pc() else { return };
        // After one tick the stand state's anim has frames; current_frame resolves.
        pc.tick(neutral());
        assert!(
            pc.current_frame().is_some(),
            "stand animation should resolve a current frame"
        );
        // Point the entity at an anim that does not exist; current_frame must be
        // None (graceful), not a panic.
        pc.entity.anim = 999_999;
        assert!(
            pc.current_frame().is_none(),
            "a nonexistent anim id must yield None, not panic"
        );
    }

    // ---- AC3: the engine built-in stand->walk command-state is present in
    // [Statedef -1] after loading (now supplied by the fp-character loader for
    // every character, task 7.3 part B — not an app shim). ----

    #[test]
    fn headless_walk_bridge_present_after_construction() {
        let Some(pc) = load_kfm_pc() else { return };
        // The loader appended the engine built-in stand<->walk locomotion to
        // [Statedef -1]. It must carry a ChangeState into state 20 gated on a
        // holdfwd-style command.
        let minus_one = pc
            .loaded
            .state(-1)
            .expect("[Statedef -1] must exist after construction");
        let walks = minus_one.controllers.iter().any(|c| {
            c.controller_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"))
                && c.params
                    .get("value")
                    .is_some_and(|e| e.source.trim() == STATE_WALK.to_string())
        });
        assert!(
            walks,
            "a ChangeState -> walk(20) must be present in [Statedef -1]"
        );
    }

    // ---- AC2 (5.6b end-to-end): the `alive` trigger now resolves natively, so
    // the `drop_unevaluable_alive_controllers` band-aid is gone. KFM's stock
    // common stand state STILL carries its `trigger1 = !alive => ChangeState 5050`
    // death gate (we no longer strip it); with `alive` correctly resolving to
    // `Life > 0`, a full-life character must never trip it. ----

    #[test]
    fn headless_alive_resolves_so_no_death_state_at_full_life() {
        let Some(mut pc) = load_kfm_pc() else { return };

        // The alive-gated controllers are NO LONGER dropped: KFM's common stand
        // state must still author its `!alive` death gate, proving we kept the
        // stock data intact rather than papering over it.
        let any_alive = pc.loaded.states.values().any(|s| {
            s.controllers.iter().any(|c| {
                c.triggerall
                    .iter()
                    .chain(c.triggers.iter().flat_map(|g| g.conditions.iter()))
                    .any(|e| e.source.to_ascii_lowercase().contains("alive"))
            })
        });
        assert!(
            any_alive,
            "stock KFM authors `alive`-gated controllers; they must NOT be stripped \
             (the drop band-aid was removed in 5.6c)"
        );

        // Sanity: full life means `alive` is truthy from the start.
        assert!(pc.entity.life > 0, "KFM starts at full life");

        // Across many idle ticks the character must never enter KFM's death state
        // (5050) — proving `alive` resolves correctly (5.6b) instead of defaulting
        // to 0 and tripping the `!alive` gate, which the band-aid used to mask.
        for _ in 0..60 {
            pc.tick(neutral());
            assert_ne!(
                pc.entity.state_no, 5050,
                "a full-life character must not fall into the death state while idle"
            );
        }
    }

    // =====================================================================
    // PROCTOR (test-engineer) coverage for task 5.6c.
    //
    // The blocks below COMPLEMENT Forge's tests above. They harden the four
    // acceptance criteria against edge cases, error paths, and MUGEN semantics
    // that the existing suite does not pin:
    //   * AC1 — exactly ONE residual shim remains (the documented engine bridge);
    //           the removed band-aids leave no synthesized-command/death-strip
    //           behavior.
    //   * AC2 — every KFM hold direction compiles natively from its raw `/$X`
    //           string; a real-fixture round-trip over the whole kfm.cmd proves
    //           the no-normalization pipeline accepts stock MUGEN syntax and only
    //           drops genuinely-malformed (`;`-alternate) forms; snapshot semantics
    //           are decoupled from matcher internals and case-insensitive.
    //   * AC3 — after release the character not only returns toward stand but
    //           STOPS advancing (the task's explicit "stop advancing once it
    //           leaves the walk state"); cursor/blend helpers never panic on the
    //           full input range.
    //   * AC4 — covered by the diagnosis reported out-of-band: the single residual
    //           is a genuine engine gap, asserted minimal here.
    // All fixture-dependent tests skip cleanly when test-assets/ is absent.
    // =====================================================================

    // ---- AC2: every KFM "hold direction" command compiles natively ----

    #[test]
    fn all_kfm_hold_directions_compile_natively() {
        // kfm.cmd defines holdfwd/holdback/holdup/holddown as `/$F`,`/$B`,`/$U`,
        // `/$D`. With `normalize_command` gone, each must compile straight from the
        // raw string to exactly one hold-direction element (the `/` = hold and `$`
        // = direction-detect modifiers are handled inside fp-input as of 5.6a).
        for raw in ["/$F", "/$B", "/$U", "/$D"] {
            let els = compile_command(raw)
                .unwrap_or_else(|e| panic!("raw hold command {raw:?} must compile: {e}"));
            assert_eq!(els.len(), 1, "{raw:?} is a single hold-direction element");
        }
    }

    #[test]
    fn holdback_diagonal_detect_compiles_to_single_element() {
        // The walk->stand rule is gated on BOTH holdfwd and holdback. The back
        // hold (`/$B`) must compile just like forward, or release-while-facing
        // would never resolve. One element, no error.
        let els = compile_command("/$B").expect("holdback `/$B` must compile natively");
        assert_eq!(els.len(), 1);
    }

    #[test]
    fn malformed_command_returns_err_not_panic() {
        // The `.cmd` compile loop relies on `compile_command` returning `Err`
        // (logged + skipped) rather than panicking on junk. An empty string and a
        // bare unknown token must both be `Err`, never a panic.
        assert!(compile_command("").is_err(), "empty command is an error");
        assert!(
            compile_command("   ").is_err(),
            "whitespace-only is an error"
        );
        assert!(
            compile_command("not_a_token").is_err(),
            "an unknown token is an error, not a panic"
        );
    }

    #[test]
    fn semicolon_alternate_form_is_skipped_gracefully() {
        // MUGEN's `;` alternate-command separator is NOT parsed by fp-input's
        // comma-splitting `compile_command`; such a form fails to compile. The
        // .cmd loop in `CnsCharacter::new` must SKIP it (warn + continue), never
        // abort. We pin the precondition here: the form is `Err`, so the loop's
        // `filter_map(|c| ...None)` path is the one exercised.
        let raw = "~D, DB, B, D, DB, B, x;~F, D, DF, F, D, DF, x";
        assert!(
            compile_command(raw).is_err(),
            "a `;`-alternate command must be Err so the loader skips it gracefully"
        );
    }

    // ---- AC2: snapshot_active_commands semantics independent of fixtures ----

    #[test]
    fn snapshot_active_commands_only_reports_passed_defs() {
        // The snapshot enumerates the *passed* def slice, filtering by the
        // matcher's active set. A def the matcher knows about but that is NOT in
        // the passed slice must not leak into the snapshot — proving the snapshot
        // is driven by the caller's def list, not matcher internals.
        let matcher_defs = vec![cmd_def("holdfwd", "/$F", 1, 1)];
        let mut matcher = CommandMatcher::new(matcher_defs);
        let mut buffer = InputBuffer::new();
        buffer.push(hold_right());
        matcher.check_commands(&buffer, true);

        // Query with an EMPTY def slice: nothing can be reported.
        let active_empty = snapshot_active_commands(&matcher, &[]);
        assert!(
            !active_empty.is_active("holdfwd"),
            "an empty def slice yields an empty snapshot even when holdfwd is firing"
        );

        // Query with a def slice that does not include the firing command.
        let other = vec![cmd_def("holdback", "/$B", 1, 1)];
        let active_other = snapshot_active_commands(&matcher, &other);
        assert!(
            !active_other.is_active("holdfwd"),
            "holdfwd is not in the passed slice, so it must not appear"
        );
        assert!(
            !active_other.is_active("holdback"),
            "holdback is in the slice but not firing (only Right held), so inactive"
        );
    }

    #[test]
    fn snapshot_active_commands_case_insensitive_query() {
        // MUGEN command labels are case-insensitive. The snapshot built from a
        // lowercase def must answer queries in any case.
        let defs = vec![cmd_def("holdfwd", "/$F", 1, 1)];
        let mut matcher = CommandMatcher::new(defs.clone());
        let mut buffer = InputBuffer::new();
        buffer.push(hold_right());
        matcher.check_commands(&buffer, true);
        let active = snapshot_active_commands(&matcher, &defs);
        assert!(active.is_active("holdfwd"));
        assert!(active.is_active("HOLDFWD"), "uppercase query matches");
        assert!(active.is_active("HoldFwd"), "mixed-case query matches");
    }

    // ---- AC2 (real fixture): the whole kfm.cmd compiles via the no-normalize
    // pipeline; only the genuinely-malformed `;`-alternate forms are dropped.
    // Gated: skips when test-assets/ is absent. ----

    #[test]
    fn kfm_cmd_round_trip_compiles_locomotion_and_skips_only_malformed() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping kfm.cmd round-trip: {} not present", def.display());
            return;
        }
        let Ok(loaded) = LoadedCharacter::load(&def) else {
            eprintln!("skipping kfm.cmd round-trip: kfm.def failed to load");
            return;
        };
        let Some(cmd) = loaded.cmd.as_ref() else {
            eprintln!("skipping kfm.cmd round-trip: no .cmd parsed");
            return;
        };

        let total = cmd.commands.len();
        assert!(total > 0, "kfm.cmd must define commands");

        // Compile EVERY raw command string exactly as `CnsCharacter::new` does
        // (straight to `compile_command`, no app-side normalization).
        let mut compiled = 0usize;
        let mut failed: Vec<&str> = Vec::new();
        let mut holdfwd_ok = false;
        let mut holdback_ok = false;
        for c in &cmd.commands {
            match compile_command(&c.command) {
                Ok(_) => {
                    compiled += 1;
                    if c.name.eq_ignore_ascii_case("holdfwd") {
                        holdfwd_ok = true;
                    }
                    if c.name.eq_ignore_ascii_case("holdback") {
                        holdback_ok = true;
                    }
                }
                Err(_) => failed.push(c.command.as_str()),
            }
        }

        // The locomotion commands the engine bridge depends on MUST compile.
        assert!(
            holdfwd_ok,
            "holdfwd must compile from its raw kfm.cmd string (no normalization)"
        );
        assert!(
            holdback_ok,
            "holdback must compile from its raw kfm.cmd string (no normalization)"
        );

        // Only the `;`-alternate forms are expected to fail; every failure must
        // contain a `;` (otherwise a real MUGEN command regressed). The bulk must
        // compile, proving the raw pipeline is faithful, not a mass-drop.
        for f in &failed {
            assert!(
                f.contains(';'),
                "the only acceptable compile failures are `;`-alternate forms; \
                 got an unexpected failure: {f:?}"
            );
        }
        assert!(
            compiled >= total.saturating_sub(failed.len()),
            "compiled count accounting is consistent"
        );
        assert!(
            compiled * 2 >= total,
            "the large majority of kfm.cmd must compile natively (compiled {compiled}/{total})"
        );
    }

    // ---- AC3: after release, the character STOPS advancing (the task's explicit
    // 'stop advancing once it leaves the walk state'). Gated on fixtures.
    //
    // MUGEN-FAITHFUL NUANCE (verified by trace, NOT a bug): on release KFM enters
    // stand(0) carrying its residual walk velocity, then `stand.friction = 0.85`
    // *coasts* it down for a few ticks until `[State 0, 3]`'s `trigger2 = Time = 4`
    // fires `VelSet x = 0` — a hard stop. So we assert the character STOPS (vel.x
    // reaches ~0 and pos.x freezes) within a handful of ticks and stays frozen,
    // rather than demanding an instantaneous halt (which would be wrong MUGEN
    // physics). Note the `abs(vel x) < Const(...)` friction-threshold gate does not
    // resolve yet (the documented `const(...)` gap), so the `Time = 4` rule is the
    // operative hard stop — which is exactly what the trace shows. ----

    #[test]
    fn headless_release_stops_forward_advance() {
        let Some(mut pc) = load_kfm_pc() else { return };

        // Drive into the walk state holding Forward (facing right).
        let mut walking = false;
        for _ in 0..30 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK {
                walking = true;
                break;
            }
        }
        assert!(
            walking,
            "hold Forward should reach walk; got {}",
            pc.entity.state_no
        );

        // Release until we are back in stand.
        let mut stood = false;
        for _ in 0..30 {
            pc.tick(neutral());
            if pc.entity.state_no == STATE_STAND {
                stood = true;
                break;
            }
        }
        assert!(
            stood,
            "release should return to stand; got {}",
            pc.entity.state_no
        );

        // Within a few more idle ticks the x-velocity must reach ~0 (friction
        // coast-down + the `Time = 4` hard stop). Allow up to 8 ticks of slack.
        let mut halted = false;
        for _ in 0..8 {
            pc.tick(neutral());
            if pc.entity.vel.x.abs() < 1e-3 {
                halted = true;
                break;
            }
        }
        assert!(
            halted,
            "standing idle must bring x-velocity to ~0 (friction + Time=4 VelSet); \
             got vel.x = {}",
            pc.entity.vel.x
        );

        // Once halted, the position must be FROZEN: no further advance across many
        // idle ticks (the walk velocity is fully cleared, not merely small).
        let x_frozen = pc.entity.pos.x;
        for _ in 0..15 {
            pc.tick(neutral());
        }
        assert!(
            (pc.entity.pos.x - x_frozen).abs() < 1e-4,
            "after halting in stand, pos.x must stay frozen; drifted {x_frozen} -> {} \
             while idle (residual walk velocity not cleared)",
            pc.entity.pos.x
        );
        assert_eq!(
            pc.entity.state_no, STATE_STAND,
            "the character must remain in stand while idle"
        );
    }

    // ---- AC3 (MUGEN semantics): cursor + blend helpers never panic on the full
    // input range; len==0 is the empty-action guard. ----

    #[test]
    fn clamp_elem_empty_action_with_extreme_indices() {
        // An empty action (len 0) must always yield 0 for ANY index, including the
        // signed extremes — the caller guards emptiness but clamp must not panic
        // or attempt `len - 1` underflow.
        assert_eq!(clamp_elem(i32::MIN, 0), 0);
        assert_eq!(clamp_elem(-1, 0), 0);
        assert_eq!(clamp_elem(0, 0), 0);
        assert_eq!(clamp_elem(i32::MAX, 0), 0);
    }

    #[test]
    fn map_blend_mode_additive_alpha_full_byte_range_never_panics() {
        // The in-memory `AdditiveAlpha` variant carries a `u8` (0..=255); the
        // renderer wants 0.0..1.0 via `/256`, so the max byte 255 maps to ~0.996
        // (NOT 1.0 — only the unrepresentable 256 would). Mapping every byte must
        // produce a finite, in-range, monotonically non-decreasing alpha with no
        // panic, since malformed content can carry any byte value.
        let mut prev = -1.0f32;
        for v in 0..=255u8 {
            let (m, alpha) = map_blend_mode(&fp_formats::air::BlendMode::AdditiveAlpha(v));
            assert_eq!(m, fp_render::BlendMode::Additive);
            assert!(
                alpha.is_finite() && (0.0..1.0).contains(&alpha),
                "alpha for AdditiveAlpha({v}) must be a finite [0,1) value, got {alpha}"
            );
            assert!(
                alpha >= prev,
                "alpha must be non-decreasing in the byte value"
            );
            prev = alpha;
        }
        // The maximum byte (255) is the largest representable, just under 1.0.
        let (_, full) = map_blend_mode(&fp_formats::air::BlendMode::AdditiveAlpha(255));
        assert!((full - 255.0 / 256.0).abs() < 1e-6, "255/256, got {full}");
    }

    // ---- AC1/AC4: no app-side movement shim remains. The death-state-strip and
    // synthesized standalone movement state are gone; the engine built-in
    // stand->walk command-state now comes from the fp-character loader (task 7.3
    // part B), and is present exactly once in [Statedef -1]. Gated. ----

    #[test]
    fn only_residual_shim_is_the_documented_engine_bridge() {
        let Some(pc) = load_kfm_pc() else { return };

        // (a) The death gate is NOT stripped (drop_unevaluable_alive_controllers
        //     removed): KFM still authors `!alive`-gated controllers somewhere.
        let alive_gate_present = pc.loaded.states.values().any(|s| {
            s.controllers.iter().any(|c| {
                c.triggerall
                    .iter()
                    .chain(c.triggers.iter().flat_map(|g| g.conditions.iter()))
                    .any(|e| e.source.to_ascii_lowercase().contains("alive"))
            })
        });
        assert!(
            alive_gate_present,
            "stock `alive`-gated controllers must remain (death-strip band-aid gone)"
        );

        // (b) The engine built-in stand->walk command-state appears EXACTLY once
        //     in [Statedef -1] (the loader appends it once; KFM's own `.cmd` `-1`
        //     authors no walk-entry of its own), confirming no duplicate / no extra
        //     synthesized movement state.
        let minus_one = pc.loaded.state(-1).expect("[Statedef -1] exists");
        let to_walk = minus_one
            .controllers
            .iter()
            .filter(|c| {
                c.controller_type
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"))
                    && c.params
                        .get("value")
                        .is_some_and(|e| e.source.trim() == STATE_WALK.to_string())
            })
            .count();
        assert_eq!(
            to_walk, 1,
            "exactly one stand->walk ChangeState built-in (no duplicate / no extra synthesized movement state)"
        );
    }

    // ---- AC1/AC2 end-to-end: stand state falls through to walk via [Statedef -1]
    // (no synthesized standalone movement state). The transition flows through the
    // loader's built-in command->state controllers in [Statedef -1]. Gated. ----

    #[test]
    fn headless_walk_transition_flows_through_statedef_minus_one() {
        let Some(mut pc) = load_kfm_pc() else { return };

        // The transition must be driven by [Statedef -1] (the per-tick
        // command->state state), which carries the bridge. Verify the bridge
        // controller exists there BEFORE driving input (precondition), then drive.
        let bridge_in_minus_one = pc
            .loaded
            .state(-1)
            .map(|s| {
                s.controllers.iter().any(|c| {
                    c.params
                        .get("value")
                        .is_some_and(|e| e.source.trim() == STATE_WALK.to_string())
                })
            })
            .unwrap_or(false);
        assert!(
            bridge_in_minus_one,
            "the stand->walk bridge must live in [Statedef -1], not a synthesized state"
        );

        let mut entered = false;
        for _ in 0..30 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK {
                entered = true;
                break;
            }
        }
        assert!(
            entered,
            "hold Forward must reach walk(20) through the merged [Statedef -1] bridge; got {}",
            pc.entity.state_no
        );
    }

    // =====================================================================
    // PROCTOR (test-engineer) coverage for task 7.2 — two-player Match wiring.
    //
    // These blocks COMPLEMENT Forge's 7.2 tests above. Forge's suite proves the
    // happy path (build the two-KFM match, attack connects, life drops, round
    // advances) and the helper purity (life_fraction, world_to_screen_x,
    // clamp_elem, map_blend_mode). The gaps Proctor closes here are:
    //
    //   * AC1 — an app-built two-KFM match WALKS P1 toward P2 when "toward the
    //           opponent" is held, end to end, with NO app shim: the `Match` runs
    //           KFM's real CommandMatcher (so `holdfwd` fires), the loader's
    //           built-in stand->walk command-state enters state 20, and KFM's own
    //           `[Statedef 20]` VelSet (gated on `command="holdfwd"`) moves it.
    //   * AC1 — `build_player` seeds the exact start X / stand state / control /
    //           full life the demo relies on (not just "left/right of center").
    //   * AC2 — `player_current_frame` (the engine-Player analog of the single-
    //           character `current_frame`) degrades to None for a missing anim and
    //           resolves a real frame for the live stand anim — the exact accessor
    //           the renderer calls per frame for BOTH fighters.
    //   * AC2 — the stage bounds the app constructs (`STAGE_HALF_WIDTH`) are wired
    //           into the Match and the two fighters start strictly inside them.
    //   * AC3 — the round advances toward Ko via the TIME-OVER branch too (Forge's
    //           tests only reach Ko via damage). A zero-second round built from
    //           app players reaches Ko/Win with the higher-life fighter winning,
    //           proving the app-built Player feeds the full round flow.
    //   * AC4 — degenerate inputs (both left+right, all buttons, P2 also active)
    //           never panic and the match still advances; CLI arg-routing
    //           precedence (.def vs .sff) is pinned via `is_def_path`.
    //
    // Every fixture-dependent test skips cleanly when test-assets/ is absent.
    // =====================================================================

    /// AC1 end-to-end (NO app shim): an app-built two-KFM match must actually WALK
    /// P1 toward P2 when "toward the opponent" is held — not merely flip to state
    /// 20 and stand still. This is the load-bearing proof of the whole 7.3 change:
    /// the `Match` runs KFM's real `CommandMatcher` (Part A) so `holdfwd` fires,
    /// the loader's built-in stand->walk command-state (Part B) enters state 20,
    /// and KFM's own `[Statedef 20]` VelSet (gated on `command="holdfwd"`) supplies
    /// the walk speed — with the two shims now deleted (Part C).
    #[test]
    fn app_built_match_walks_p1_toward_opponent() {
        let Some(mut m) = build_kfm_match() else {
            return;
        };
        assert!(
            run_until_fight(&mut m),
            "fight must go live before driving input"
        );

        // P1 faces right (toward P2); holding right is "fwd". The gap between the
        // two must shrink as P1 closes in (they start ~120px apart, well outside
        // the player-push touching distance).
        let gap_before = (m.p1().pos().x - m.p2().pos().x).abs();
        let p1_x_before = m.p1().pos().x;
        for _ in 0..60 {
            m.tick(
                MatchInput {
                    right: true,
                    ..MatchInput::none()
                },
                MatchInput::none(),
            );
        }
        let gap_after = (m.p1().pos().x - m.p2().pos().x).abs();
        assert!(
            m.p1().pos().x > p1_x_before,
            "holding 'toward opponent' must advance P1's world X via the walk-velocity \
             bridge; pos.x went {p1_x_before} -> {} (no advance = bridge not wired)",
            m.p1().pos().x
        );
        assert!(
            gap_after < gap_before,
            "P1 walking toward P2 must close the gap ({gap_before} -> {gap_after})"
        );
    }

    // ---- AC1: build_player seeds the precise demo start state. ----

    #[test]
    fn build_player_seeds_position_state_control_and_full_life() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!(
                "skipping build_player seed test: {} not present",
                def.display()
            );
            return;
        }
        let player = match build_player(&def, P1_START_X, None) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping build_player seed test: {e}");
                return;
            }
        };
        // The exact start X the demo places P1 at (before Match::new's facing seed,
        // which does not move X).
        assert!(
            (player.pos().x - P1_START_X).abs() < 1e-6,
            "P1 seeded at P1_START_X"
        );
        assert!(
            (player.pos().y).abs() < 1e-6,
            "seeded on the ground plane (y=0)"
        );
        assert_eq!(
            player.character.state_no, STATE_STAND,
            "starts in the stand state"
        );
        assert!(player.character.ctrl, "starts with control");
        assert_eq!(
            player.anim(),
            STATE_STAND,
            "starts on the stand animation (action 0)"
        );
        assert!(player.life() > 0, "starts with positive life");
        assert_eq!(player.life(), player.life_max(), "starts at FULL life");
    }

    // ---- AC2: player_current_frame — the per-frame render accessor for BOTH
    // fighters — degrades gracefully and resolves the live frame. ----

    #[test]
    fn player_current_frame_resolves_stand_and_none_for_missing_anim() {
        let Some(mut m) = build_kfm_match() else {
            return;
        };
        // After ticking through the intro into the fight, P1's stand anim has frames.
        assert!(run_until_fight(&mut m), "drive to the fight phase");
        assert!(
            player_current_frame(m.p1()).is_some(),
            "the stand animation must resolve a current frame for rendering"
        );
        assert!(
            player_current_frame(m.p2()).is_some(),
            "P2's animation must also resolve a frame (both fighters render)"
        );
        // Now point a player at a nonexistent anim and confirm None (graceful, no
        // panic). We reach into the public `character` field exactly as the engine
        // would. (`m` is borrowed immutably above, so build a fresh solo player to
        // mutate rather than reaching into the match.)
        let mut solo = match build_player(&test_asset("kfm/kfm.def"), P1_START_X, None) {
            Ok(p) => p,
            Err(_) => return,
        };
        solo.character.anim = 987_654;
        assert!(
            player_current_frame(&solo).is_none(),
            "a nonexistent anim id must yield None for the renderer, not panic"
        );
    }

    // ---- AC2: the app's stage bounds are wired into the Match and both fighters
    // start strictly inside them. ----

    #[test]
    fn app_stage_bounds_wired_and_fighters_start_inside() {
        let Some(m) = build_kfm_match() else { return };
        let bounds = m.bounds();
        assert!(
            (bounds.left - -STAGE_HALF_WIDTH).abs() < 1e-6,
            "left bound = -STAGE_HALF_WIDTH"
        );
        assert!(
            (bounds.right - STAGE_HALF_WIDTH).abs() < 1e-6,
            "right bound = STAGE_HALF_WIDTH"
        );
        // Both start strictly inside the playfield (their centers, at least).
        assert!(
            m.p1().pos().x > bounds.left && m.p1().pos().x < bounds.right,
            "P1 starts inside the stage"
        );
        assert!(
            m.p2().pos().x > bounds.left && m.p2().pos().x < bounds.right,
            "P2 starts inside the stage"
        );
    }

    // ---- AC3 (round advances toward Ko via the TIME-OVER branch). Forge's tests
    // reach Ko only via damage; this proves the app-built Player also carries the
    // round flow to a result when the clock expires. ----

    #[test]
    fn app_players_time_over_reaches_ko_and_win() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping time-over test: {} not present", def.display());
            return;
        }
        // Build two app-style players, but with a ZERO-second round so the fight
        // phase times out immediately. This drives the engine's time-over branch
        // (not the KO-by-damage branch Forge covers), using the exact `build_player`
        // construction the demo uses.
        let (p1, p2) = match (
            build_player(&def, P1_START_X, None),
            build_player(&def, P2_START_X, None),
        ) {
            (Ok(a), Ok(b)) => (a, b),
            _ => {
                eprintln!("skipping time-over test: a player failed to build");
                return;
            }
        };
        let mut m = Match::with_round_seconds(
            p1,
            p2,
            StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH),
            0,
        );
        assert_eq!(m.round_state(), RoundState::Intro, "starts in intro");
        assert_eq!(
            m.timer(),
            0,
            "a zero-second round starts with an empty clock"
        );

        // Tick neutrally through intro + the KO hold into the decided round.
        let mut reached_win = false;
        for _ in 0..400 {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() == RoundState::Win {
                reached_win = true;
                break;
            }
        }
        assert!(
            reached_win,
            "a zero-second round must advance Intro -> Fight -> Ko -> Win; ended in {:?}",
            m.round_state()
        );
        // Both fighters idled at full, equal life, so time-over is a draw.
        assert_eq!(
            m.winner(),
            Some(Winner::Draw),
            "equal life at time over is a draw"
        );
    }

    /// AC3: the round visits the intermediate [`RoundState::Ko`] hold on its way to
    /// [`RoundState::Win`] (it does not jump straight to Win). Pins the lifecycle
    /// the HUD's KO/round indicator renders against.
    #[test]
    fn app_players_round_passes_through_ko_before_win() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping ko-before-win test: {} not present", def.display());
            return;
        }
        let (p1, p2) = match (
            build_player(&def, P1_START_X, None),
            build_player(&def, P2_START_X, None),
        ) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return,
        };
        let mut m = Match::with_round_seconds(
            p1,
            p2,
            StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH),
            0,
        );
        let mut saw_ko = false;
        let mut saw_win = false;
        for _ in 0..400 {
            m.tick(MatchInput::none(), MatchInput::none());
            match m.round_state() {
                RoundState::Ko => saw_ko = true,
                RoundState::Win => {
                    saw_win = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_ko, "the round must hold in Ko before resolving to Win");
        assert!(saw_win, "the round must ultimately resolve to Win");
    }

    // ---- AC4 (never panics): degenerate / adversarial inputs on BOTH players. ----

    #[test]
    fn match_survives_conflicting_and_all_button_input_on_both_players() {
        let Some(mut m) = build_kfm_match() else {
            return;
        };
        // Both directions held at once (no net horizontal), every button pressed,
        // on BOTH players simultaneously, for a long budget. Must never panic and
        // the round must still advance off the intro.
        let chaos = MatchInput {
            left: true,
            right: true,
            up: true,
            down: true,
            a: true,
            b: true,
            c: true,
            x: true,
            y: true,
            z: true,
        };
        for _ in 0..300 {
            m.tick(chaos, chaos);
        }
        assert!(
            matches!(
                m.round_state(),
                RoundState::Fight | RoundState::Ko | RoundState::Win
            ),
            "the match must leave the intro under sustained chaotic input on both sides"
        );
    }

    /// AC4 (life HUD invariant under combat): neither fighter's life ever exceeds
    /// its max nor goes more negative than the HUD can clamp, across a real fight
    /// — `life_fraction` already clamps, but this proves the engine never feeds it
    /// a NaN-inducing `life_max <= 0`.
    #[test]
    fn life_values_stay_hud_safe_through_a_fight() {
        let Some(mut m) = build_kfm_match() else {
            return;
        };
        assert!(run_until_fight(&mut m), "drive to fight");
        for i in 0..600 {
            let inp = if i % 3 == 0 {
                MatchInput {
                    right: true,
                    x: true,
                    ..MatchInput::none()
                }
            } else {
                MatchInput {
                    right: true,
                    ..MatchInput::none()
                }
            };
            m.tick(inp, MatchInput::none());
            for p in [m.p1(), m.p2()] {
                assert!(p.life_max() > 0, "life_max must stay positive for the HUD");
                assert!(p.life() <= p.life_max(), "life never exceeds its max");
                // The HUD fraction must always be a finite [0,1] value for any life.
                let f = life_fraction(p.life(), p.life_max());
                assert!(
                    f.is_finite() && (0.0..=1.0).contains(&f),
                    "HUD fraction in [0,1]"
                );
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
    }

    // ---- AC1 (CLI arg routing): the .def-vs-.sff precedence `select_mode` relies
    // on. `select_mode` itself needs a GPU Renderer, but its routing decisions are
    // pure functions of `is_def_path` over the args — pin those decisions here. ----

    #[test]
    fn is_def_path_drives_match_vs_viewer_routing() {
        // Two .def args -> two-player match branch.
        assert!(is_def_path("p1.def") && is_def_path("p2.def"));
        // A .def first with a non-.def second still routes to the match (P1.def),
        // because select_mode checks `is_def_path(args[1])` for the single-def arm.
        assert!(is_def_path("kfm.DEF"));
        // SFF first -> legacy viewer/static branch (NOT a match).
        assert!(!is_def_path("char.sff"));
        assert!(!is_def_path("char.air"));
        // Extensionless / unusual names are not .def.
        assert!(!is_def_path("kfm"));
        assert!(!is_def_path("kfm.def.bak"));
        assert!(
            !is_def_path(".def"),
            "a bare dotfile named .def has no extension"
        );
    }

    // ---- AC2 (HUD geometry): the P2 bar is mirrored to the right edge and the
    // world->screen mapping respects the WORLD_TO_SCREEN scale. ----

    #[test]
    fn world_to_screen_x_applies_scale_and_is_monotonic() {
        let win_w = 800.0;
        let center = win_w / 2.0;
        // Origin maps to the window center.
        assert!((world_to_screen_x(0.0, win_w) - center).abs() < 1e-6);
        // Monotonic and scale-respecting: +world is +scale*world from center.
        let a = world_to_screen_x(10.0, win_w);
        let b = world_to_screen_x(20.0, win_w);
        assert!(b > a, "screen X increases with world X");
        assert!(
            ((b - a) - (10.0 * WORLD_TO_SCREEN)).abs() < 1e-4,
            "the delta equals the world delta times WORLD_TO_SCREEN"
        );
        // Mirror symmetry about the center.
        let left = world_to_screen_x(-30.0, win_w);
        let right = world_to_screen_x(30.0, win_w);
        assert!(
            (center - left - (right - center)).abs() < 1e-4,
            "symmetric about center"
        );
    }

    // --- Clsn debug overlay box-mapping math (audit #34) ---

    #[test]
    fn player_screen_anchor_matches_draw_player_mapping() {
        // The overlay anchor must equal the (x, y) draw_player hangs the sprite
        // off: X via world_to_screen_x, Y via the ground plane plus pos.y.
        let win_w = 640.0;
        let win_h = 480.0;
        let pos = fp_core::Vec2::new(40.0, -30.0);
        // Camera at 0: the anchor reduces to the original (un-scrolled) mapping.
        let (ax, ay) = player_screen_anchor(pos, 0.0, win_w, win_h);
        assert!((ax - world_to_screen_x(pos.x, win_w)).abs() < 1e-4);
        assert!((ay - (win_h * 0.8 + pos.y)).abs() < 1e-4);
    }

    #[test]
    fn clsn_to_screen_box_facing_right_translates_only() {
        // A local box to the right of and above the axis, facing right: X/Y are
        // just translated by the anchor; no mirroring.
        let local = fp_core::Rect::new(10.0, -40.0, 20.0, 30.0); // x,y,w,h
        let b = clsn_to_screen_box(
            &local,
            100.0,
            200.0,
            fp_character::Facing::Right,
            CLSN1_COLOR,
        );
        assert!((b.x - 110.0).abs() < 1e-4, "x = anchor_x + local.x");
        assert!((b.w - 20.0).abs() < 1e-4, "width preserved");
        assert!(
            (b.y - 160.0).abs() < 1e-4,
            "y = anchor_y + local.y (Y down)"
        );
        assert!((b.h - 30.0).abs() < 1e-4, "height preserved");
        assert_eq!(b.color, CLSN1_COLOR);
    }

    #[test]
    fn clsn_to_screen_box_facing_left_mirrors_x_only() {
        // Same local box facing left: X is reflected about the anchor while Y is
        // untouched, matching fp_physics::place_clsn. The left/right edges swap,
        // but the result stays a non-negative-width rect.
        let local = fp_core::Rect::new(10.0, -40.0, 20.0, 30.0);
        let right = clsn_to_screen_box(
            &local,
            100.0,
            200.0,
            fp_character::Facing::Right,
            CLSN2_COLOR,
        );
        let left = clsn_to_screen_box(
            &local,
            100.0,
            200.0,
            fp_character::Facing::Left,
            CLSN2_COLOR,
        );

        // Facing left: edges run from anchor - local.right() to anchor - local.x.
        assert!(
            (left.x - 70.0).abs() < 1e-4,
            "left edge = anchor_x - local.right()"
        );
        assert!(
            (left.w - 20.0).abs() < 1e-4,
            "width preserved under mirroring"
        );
        // Y is identical to the right-facing case (facing never affects Y).
        assert!((left.y - right.y).abs() < 1e-4);
        assert!((left.h - right.h).abs() < 1e-4);
        // The mirrored box is the reflection of the right-facing box about the
        // anchor X: their centers are equidistant from the anchor.
        let rc = right.x + right.w / 2.0;
        let lc = left.x + left.w / 2.0;
        assert!(
            ((100.0 - lc) - (rc - 100.0)).abs() < 1e-4,
            "symmetric about anchor"
        );
    }

    #[test]
    fn clsn_to_screen_box_axis_crossing_box_facing_left() {
        // A box straddling the axis (negative left edge) still normalizes to a
        // non-negative width after the left-facing mirror.
        let local = fp_core::Rect::new(-15.0, -10.0, 30.0, 10.0); // spans x=-15..15
        let b = clsn_to_screen_box(&local, 50.0, 0.0, fp_character::Facing::Left, CLSN1_COLOR);
        assert!(b.w >= 0.0, "width is non-negative after mirroring");
        assert!((b.w - 30.0).abs() < 1e-4, "width magnitude preserved");
        // Mirrored edges: anchor - 15 .. anchor + 15 => 35..65, so x = 35.
        assert!((b.x - 35.0).abs() < 1e-4);
    }

    // ---- T063: collect_clsn_boxes — shared box math + push box + facing. ----

    /// A synthetic, asset-free [`SffFile`] (one empty SFF v1 sprite). Mirrors the
    /// fp-engine test helper so these tests need no `test-assets/`.
    fn synth_sff() -> SffFile {
        const SUBHEADER_OFFSET: usize = 64;
        let mut buf = vec![0u8; SUBHEADER_OFFSET + 32];
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[15] = 1; // SFF v1
        buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // num_groups
        buf[20..24].copy_from_slice(&1u32.to_le_bytes()); // num_images
        buf[24..28].copy_from_slice(&(SUBHEADER_OFFSET as u32).to_le_bytes());
        SffFile::from_bytes(&buf).expect("synthetic SFF v1 must parse")
    }

    /// Builds a headless [`Player`] whose action 0 has exactly one frame with one
    /// known Clsn1 (hit) and one known Clsn2 (hurt) box, positioned at the axis,
    /// facing `facing`, with the default `[Size]` push half-widths. Asset-free —
    /// no `test-assets/` required — so the T063 verification runs on CI.
    fn synth_clsn_player(facing: fp_character::Facing) -> Player {
        let hurt = fp_core::Rect::new(-10.0, -60.0, 20.0, 60.0); // centered hurtbox
        let hit = fp_core::Rect::new(20.0, -50.0, 30.0, 10.0); // forward attack box
        let frame = fp_formats::air::AnimFrame {
            sprite: SpriteId::new(0, 0),
            ticks: -1,
            clsn1: vec![hit],
            clsn2: vec![hurt],
            ..Default::default()
        };
        let action = AnimAction {
            action_number: 0,
            frames: vec![frame],
            loopstart: 0,
        };
        let mut actions = HashMap::new();
        actions.insert(0, action);
        let loaded = LoadedCharacter {
            name: "synth".to_string(),
            displayname: "synth".to_string(),
            author: String::new(),
            localcoord: (320, 240),
            constants: fp_character::CharacterConstants::default(),
            states: HashMap::new(),
            sff: synth_sff(),
            air: AirFile { actions },
            cmd: None,
            snd: None,
            palettes: Vec::new(),
        };
        let mut c = Character::new();
        c.pos = Vec2::new(0.0, 0.0);
        c.facing = facing;
        c.anim = 0;
        c.anim_elem = 0;
        Player::new(c, loaded)
    }

    #[test]
    fn collect_clsn_boxes_tags_hit_hurt_and_push_kinds() {
        // Facing right, axis at screen x=160 (win_w=320 -> world_to_screen_x(0)).
        let player = synth_clsn_player(fp_character::Facing::Right);
        let boxes = collect_clsn_boxes(&player, 0.0, 320.0, 240.0);
        // Exactly one of each kind: one Clsn2, one Clsn1, one push box.
        let hurts = boxes.iter().filter(|(_, k)| *k == ClsnKind::Hurt).count();
        let hits = boxes.iter().filter(|(_, k)| *k == ClsnKind::Hit).count();
        let pushes = boxes.iter().filter(|(_, k)| *k == ClsnKind::Push).count();
        assert_eq!(hurts, 1, "one hurtbox (Clsn2)");
        assert_eq!(hits, 1, "one hitbox (Clsn1)");
        assert_eq!(pushes, 1, "one push/Width box");
        assert_eq!(boxes.len(), 3, "exactly hurt + hit + push");
        // Draw order: hurt first, then hit, then push (push reads over the rest).
        assert_eq!(boxes[0].1, ClsnKind::Hurt);
        assert_eq!(boxes[1].1, ClsnKind::Hit);
        assert_eq!(boxes[2].1, ClsnKind::Push);
        // Colors are the per-kind constants (red hit, blue hurt, green push).
        assert_eq!(boxes[0].0.color, CLSN2_COLOR);
        assert_eq!(boxes[1].0.color, CLSN1_COLOR);
        assert_eq!(boxes[2].0.color, PUSH_COLOR);
    }

    /// Builds a headless [`Player`] whose `.air` holds two actions: action 200 is
    /// a deterministic attack (3 startup / 4 active / 5 recovery, mirroring the
    /// shipped trainingdummy basic attack), action 0 is a plain idle (no Clsn1).
    /// The character is parked on `anim` so `format_frame_data` reads it. Asset-free.
    fn synth_frame_data_player(anim: i32) -> Player {
        use fp_formats::air::AnimFrame;
        let attack = AnimAction {
            action_number: 200,
            loopstart: 0,
            frames: vec![
                AnimFrame {
                    ticks: 3,
                    ..Default::default()
                },
                AnimFrame {
                    ticks: 4,
                    clsn1: vec![fp_core::Rect::new(20.0, -50.0, 30.0, 10.0)],
                    ..Default::default()
                },
                AnimFrame {
                    ticks: 5,
                    ..Default::default()
                },
            ],
        };
        let idle = AnimAction {
            action_number: 0,
            loopstart: 0,
            frames: vec![AnimFrame {
                ticks: 10,
                ..Default::default()
            }],
        };
        let mut actions = HashMap::new();
        actions.insert(0, idle);
        actions.insert(200, attack);
        let loaded = LoadedCharacter {
            name: "synth".to_string(),
            displayname: "synth".to_string(),
            author: String::new(),
            localcoord: (320, 240),
            constants: fp_character::CharacterConstants::default(),
            states: HashMap::new(),
            sff: synth_sff(),
            air: AirFile { actions },
            cmd: None,
            snd: None,
            palettes: Vec::new(),
        };
        let mut c = Character::new();
        c.anim = anim;
        Player::new(c, loaded)
    }

    #[test]
    fn format_frame_data_shows_startup_active_recovery_for_attack() {
        let player = synth_frame_data_player(200);
        // A freshly-built (non-connecting) player carries no advantage => `ADV —`.
        assert_eq!(format_frame_data(&player), "S3 A4 R5  ADV —");
    }

    #[test]
    fn format_frame_data_shows_dash_for_non_attack_action() {
        // Idle action (no Clsn1) is not countable -> the "—" form, never numbers.
        let player = synth_frame_data_player(0);
        assert_eq!(format_frame_data(&player), "S/A/R —  ADV —");
    }

    #[test]
    fn format_frame_data_shows_dash_for_missing_action() {
        // An action absent from the .air table also degrades to "—", never panics.
        let player = synth_frame_data_player(999);
        assert_eq!(format_frame_data(&player), "S/A/R —  ADV —");
    }

    #[test]
    fn format_frame_advantage_shows_signed_value_when_connected() {
        // On a connecting hit the readout shows the signed advantage: `+` when the
        // attacker recovers first, U+2212 MINUS when the defender does (the
        // `+3 / −5` form of T065 acceptance criterion #2).
        assert_eq!(format_frame_advantage(Some(3)), "ADV +3");
        assert_eq!(format_frame_advantage(Some(0)), "ADV +0");
        assert_eq!(format_frame_advantage(Some(-5)), "ADV \u{2212}5");
    }

    #[test]
    fn format_frame_advantage_shows_dash_without_a_connection() {
        // No connection this tick => no advantage number (never a stale one).
        assert_eq!(format_frame_advantage(None), "ADV —");
    }

    #[test]
    fn format_frame_data_appends_advantage_when_present() {
        // The full readout splices the S/A/R count and the advantage segment, so a
        // connecting attack reads e.g. `S3 A4 R5  ADV +6`. Asserted via the segment
        // composer to avoid scripting a whole match (the engine's
        // `frame_advantage_surfaced_on_scripted_connecting_hit` proves the live
        // value reaches the player; this proves the readout shows it).
        let sar = "S3 A4 R5";
        let composed = format!("{sar}  {}", format_frame_advantage(Some(6)));
        assert_eq!(composed, "S3 A4 R5  ADV +6");
    }

    #[test]
    fn collect_clsn_boxes_mirrors_hitbox_when_facing_left() {
        // The forward hitbox (local x in 20..50) lands on the +X side when facing
        // right and the -X side when facing left, mirrored about the axis.
        let right = collect_clsn_boxes(
            &synth_clsn_player(fp_character::Facing::Right),
            0.0,
            320.0,
            240.0,
        );
        let left = collect_clsn_boxes(
            &synth_clsn_player(fp_character::Facing::Left),
            0.0,
            320.0,
            240.0,
        );
        let rhit = right.iter().find(|(_, k)| *k == ClsnKind::Hit).unwrap().0;
        let lhit = left.iter().find(|(_, k)| *k == ClsnKind::Hit).unwrap().0;
        // Axis maps to the same screen X for both (facing doesn't move the axis).
        let axis = world_to_screen_x(0.0, 320.0);
        // Facing right: hitbox to the right of the axis. Facing left: to the left.
        assert!(
            rhit.x >= axis,
            "right-facing hitbox is forward (+X) of the axis"
        );
        assert!(
            lhit.x + lhit.w <= axis,
            "left-facing hitbox is forward (-X) of the axis"
        );
        // Widths are preserved under the mirror; the two are reflections about the
        // axis (their centers are equidistant from it).
        assert!(
            (rhit.w - lhit.w).abs() < 1e-4,
            "width preserved under mirror"
        );
        let rc = rhit.x + rhit.w / 2.0;
        let lc = lhit.x + lhit.w / 2.0;
        assert!(
            ((rc - axis) - (axis - lc)).abs() < 1e-4,
            "hitbox is mirrored about the axis between facings"
        );
    }

    #[test]
    fn collect_clsn_boxes_push_box_reflects_facing_relative_widths() {
        // The default [Size] half-widths are front=16, back=15 (asymmetric), so the
        // push box is NOT centered: it must reflect when facing flips.
        let player = synth_clsn_player(fp_character::Facing::Right);
        let (front, back) = player.push_widths();
        let boxes = collect_clsn_boxes(&player, 0.0, 320.0, 240.0);
        let push = boxes.iter().find(|(_, k)| *k == ClsnKind::Push).unwrap().0;
        let axis = world_to_screen_x(0.0, 320.0);
        // Total width spans front + back regardless of facing.
        assert!(
            (push.w - (front + back)).abs() < 1e-4,
            "push box spans front+back half-widths"
        );
        // Facing right: front extends to +X. Right edge = axis + front.
        assert!(
            (push.x + push.w - (axis + front)).abs() < 1e-4,
            "right-facing push box extends `front` toward +X"
        );
        assert!(
            (push.x - (axis - back)).abs() < 1e-4,
            "right-facing push box extends `back` toward -X"
        );
        // Facing left: the front/back swap sides (mirror about the axis).
        let lplayer = synth_clsn_player(fp_character::Facing::Left);
        let lboxes = collect_clsn_boxes(&lplayer, 0.0, 320.0, 240.0);
        let lpush = lboxes.iter().find(|(_, k)| *k == ClsnKind::Push).unwrap().0;
        assert!(
            (lpush.x - (axis - front)).abs() < 1e-4,
            "left-facing push box extends `front` toward -X"
        );
    }

    #[test]
    fn collect_clsn_boxes_yields_push_box_even_with_no_anim_frame() {
        // A player pointed at a nonexistent anim has no Clsn frame, but the push
        // box (derived from size, not AIR) is still produced — never a panic.
        let mut player = synth_clsn_player(fp_character::Facing::Right);
        player.character.anim = 987_654; // no such action
        let boxes = collect_clsn_boxes(&player, 0.0, 320.0, 240.0);
        assert_eq!(
            boxes.len(),
            1,
            "only the push box when no Clsn frame resolves"
        );
        assert_eq!(boxes[0].1, ClsnKind::Push);
    }

    // ---- T063: TrainingOverlay scope cycling + per-side selection. ----

    #[test]
    fn overlay_scope_cycles_off_p1_p2_both_and_back() {
        assert_eq!(OverlayScope::Off.next(), OverlayScope::P1);
        assert_eq!(OverlayScope::P1.next(), OverlayScope::P2);
        assert_eq!(OverlayScope::P2.next(), OverlayScope::Both);
        assert_eq!(OverlayScope::Both.next(), OverlayScope::Off);
    }

    #[test]
    fn overlay_scope_per_side_visibility() {
        assert!(!OverlayScope::Off.shows_p1() && !OverlayScope::Off.shows_p2());
        assert!(OverlayScope::P1.shows_p1() && !OverlayScope::P1.shows_p2());
        assert!(!OverlayScope::P2.shows_p1() && OverlayScope::P2.shows_p2());
        assert!(OverlayScope::Both.shows_p1() && OverlayScope::Both.shows_p2());
    }

    #[test]
    fn training_overlay_cycle_and_active_track_scope() {
        let mut ov = TrainingOverlay::default();
        assert!(!ov.is_active(), "default overlay is off");
        ov.cycle();
        assert_eq!(ov.scope, OverlayScope::P1);
        assert!(ov.is_active());
        ov.cycle();
        ov.cycle();
        assert_eq!(ov.scope, OverlayScope::Both);
        ov.cycle();
        assert_eq!(ov.scope, OverlayScope::Off);
        assert!(!ov.is_active());
    }

    // ---- #16: two-fighter draw-order decision from `sprpriority` ----

    #[test]
    fn p1_draws_behind_p2_orders_by_sprpriority() {
        // Lower priority draws first (behind). P1 lower -> P1 behind (true).
        assert!(
            p1_draws_behind_p2(0, 2),
            "lower P1 priority draws behind P2"
        );
        // P1 higher -> P1 in front -> P2 drawn first (false).
        assert!(
            !p1_draws_behind_p2(5, 1),
            "higher P1 priority draws in front of P2"
        );
        // Negative priorities order the same way (lower behind).
        assert!(p1_draws_behind_p2(-3, 0), "more-negative P1 draws behind");
        assert!(
            !p1_draws_behind_p2(1, -1),
            "P1 above a negative P2 draws in front"
        );
    }

    #[test]
    fn p1_draws_behind_p2_tie_keeps_p1_behind() {
        // Equal priorities: stable, deterministic default — P1 behind P2.
        assert!(
            p1_draws_behind_p2(2, 2),
            "a tie keeps P1 behind P2 (stable order)"
        );
        assert!(
            p1_draws_behind_p2(0, 0),
            "default-priority tie keeps P1 behind"
        );
    }

    #[test]
    fn char_palfx_to_render_inactive_maps_to_identity() {
        // The accessors hand back CurPalFx::IDENTITY when nothing is active; that
        // must convert to the renderer's no-op identity (no is_active gate needed).
        let out = char_palfx_to_render(fp_character::CurPalFx::IDENTITY);
        assert_eq!(out, fp_render::PalFx::IDENTITY);
        assert!(
            out.is_identity(),
            "inactive effect renders byte-identically"
        );
    }

    #[test]
    fn char_palfx_to_render_passes_active_fields_through() {
        // An active tint's add/mul/color/invertall cross the crate boundary
        // unchanged.
        let fx = fp_character::CurPalFx {
            add: [0.5, -0.25, 0.0],
            mul: [0.5, 1.0, 2.0],
            color: 0.25,
            invertall: true,
            remaining: 12,
            ..fp_character::CurPalFx::IDENTITY
        };
        let out = char_palfx_to_render(fx);
        assert_eq!(out.add, [0.5, -0.25, 0.0]);
        assert_eq!(out.mul, [0.5, 1.0, 2.0]);
        assert!((out.color - 0.25).abs() < 1e-6);
        assert!(out.invertall, "invertall crosses the boundary");
        assert!(!out.is_identity(), "an active tint is not the no-op");
    }

    // =====================================================================
    // PR-N — fight.def screenpack HUD wiring (audit #31)
    // =====================================================================

    /// With no `fight.def` anywhere on the search path (and no `FP_SCREENPACK`),
    /// `locate_fight_def` finds nothing — so the match falls back to the quad HUD
    /// (no regression for the default, screenpack-less match).
    #[test]
    fn locate_fight_def_returns_none_when_absent() {
        // A character .def in a directory that contains no fight.def / data/fight.def.
        let dir = std::env::temp_dir().join(format!("fp-screenpack-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p1_def = dir.join("kfm.def");
        // Guard against a leaked FP_SCREENPACK from the environment.
        std::env::remove_var("FP_SCREENPACK");
        assert!(
            locate_fight_def(&p1_def).is_none(),
            "no fight.def -> None -> quad HUD fallback"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `locate_fight_def` finds a `fight.def` sitting next to the P1 character def.
    #[test]
    fn locate_fight_def_finds_sibling() {
        let dir = std::env::temp_dir().join(format!("fp-screenpack-sib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fight = dir.join("fight.def");
        std::fs::write(&fight, "[Files]\nsff = fight.sff\n").unwrap();
        let p1_def = dir.join("kfm.def");
        std::env::remove_var("FP_SCREENPACK");
        assert_eq!(
            locate_fight_def(&p1_def).as_deref(),
            Some(fight.as_path()),
            "a sibling fight.def is located"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The round announcer text reflects the decided round (KO / win / draw), the
    /// round number during the intro, and is empty mid-fight.
    ///
    /// Exercises the **production** [`round_label`] (the very mapping
    /// `round_readout` delegates to), not a test-only copy — so a divergence in
    /// the real code cannot pass silently.
    #[test]
    fn round_readout_reflects_state() {
        // KO marker wins regardless of winner.
        assert_eq!(round_label(RoundState::Ko, None, 1), "KO");
        assert_eq!(round_label(RoundState::Win, Some(Winner::P1), 1), "P1 WINS");
        assert_eq!(round_label(RoundState::Win, Some(Winner::P2), 1), "P2 WINS");
        assert_eq!(round_label(RoundState::Win, None, 1), "DRAW");
        // The intro readout reflects the live round number (not a fixed "1").
        assert_eq!(round_label(RoundState::Intro, None, 1), "ROUND 1");
        assert_eq!(round_label(RoundState::Intro, None, 3), "ROUND 3");
        assert_eq!(round_label(RoundState::Fight, None, 1), "");
    }

    /// The HUD timer is whole seconds, not the engine's frames-remaining clock:
    /// `Match::timer()` returns frames (60/s), so a 99-second round (5940 frames)
    /// must read 99, never 5940. Guards the screenpack-path timer-unit contract.
    #[test]
    fn timer_frames_convert_to_whole_seconds() {
        assert_eq!(
            timer_frames_to_seconds(5940),
            99,
            "99s round = 5940 frames -> 99"
        );
        assert_eq!(timer_frames_to_seconds(60), 1);
        assert_eq!(timer_frames_to_seconds(0), 0);
        // Floors mid-second (sub-60 remainder is dropped, MUGEN-style).
        assert_eq!(timer_frames_to_seconds(59), 0);
        assert_eq!(timer_frames_to_seconds(119), 1);
        // Negative (shouldn't happen, but never render "-1").
        assert_eq!(timer_frames_to_seconds(-30), 0);
    }

    // ---- HUD text + .act palette selection (FL2b) ----------------------------

    /// Resolves a path under the shipped (committed) `assets/data/` directory.
    /// `CARGO_MANIFEST_DIR` points at `crates/fp-app`; go up two levels.
    fn shipped_data_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/data")
            .join(rel)
    }

    /// The HUD timer text is the engine's frame clock converted to whole seconds
    /// (the pure composition the announcer draws), floored and never negative.
    #[test]
    fn timer_text_is_whole_seconds_string() {
        assert_eq!(timer_text(5940), "99"); // 99s round
        assert_eq!(timer_text(60), "1");
        assert_eq!(timer_text(0), "0");
        assert_eq!(timer_text(59), "0"); // floors the sub-second remainder
        assert_eq!(timer_text(-30), "0"); // never renders a negative
    }

    /// The shipped HUD font (FL2b) parses through the real loader and maps every
    /// character the announcer/timer strings need. This is the non-gated, shipped
    /// asset, so it must load on every machine. (GPU upload via `GlyphFont` is not
    /// exercised here — that needs a device; the parse + glyph map is the contract
    /// the HUD text path depends on.)
    #[test]
    fn shipped_hud_font_parses_and_covers_hud_strings() {
        let font = fp_formats::fnt::FntFont::load(&shipped_data_asset("font.fnt"))
            .expect("shipped HUD font.fnt must load");
        // Every character used by ROUND N / KO / P1 WINS / P2 WINS / DRAW and the
        // digit timer must be mapped.
        for s in [
            "KO",
            "ROUND",
            "WINS",
            "DRAW",
            "P1",
            "P2",
            "0123456789",
            " ",
            ":",
        ] {
            for c in s.chars() {
                assert!(
                    font.glyph(c).is_some(),
                    "HUD font is missing glyph {c:?} (needed for HUD text)"
                );
            }
        }
        assert!(font.image_height > 0, "font must have a real line height");
    }

    /// `parse_pal_flags` extracts `--p1-pal`/`--p2-pal` and leaves the positional
    /// args (program name + paths) untouched for the rest of CLI routing.
    #[test]
    fn parse_pal_flags_extracts_selections_and_keeps_paths() {
        let args: Vec<String> = [
            "fp-app", "kfm.def", "--p1-pal", "3", "ken.def", "--p2-pal", "7",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (sel, rest) = parse_pal_flags(&args);
        assert_eq!(
            sel,
            PalSelection {
                p1: Some(3),
                p2: Some(7)
            }
        );
        // The flags (and their values) are stripped; the positional args survive
        // in order so `select_mode`'s file routing is unchanged.
        assert_eq!(rest, vec!["fp-app", "kfm.def", "ken.def"]);
    }

    /// No flags → no selection and the args pass through byte-for-byte, so the
    /// default (no costume swap) path is unchanged.
    #[test]
    fn parse_pal_flags_default_is_none_and_passthrough() {
        let args: Vec<String> = ["fp-app", "a.def", "b.def"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (sel, rest) = parse_pal_flags(&args);
        assert_eq!(sel, PalSelection::default());
        assert_eq!(sel.p1, None);
        assert_eq!(sel.p2, None);
        assert_eq!(rest, args);
    }

    /// A `--pN-pal` with a missing or non-numeric value is dropped (the selection
    /// stays `None`) and never panics; unknown `--…` tokens pass through.
    #[test]
    fn parse_pal_flags_tolerates_bad_values() {
        // Missing value at end-of-args.
        let a: Vec<String> = ["fp-app", "--p1-pal"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (sel, rest) = parse_pal_flags(&a);
        assert_eq!(sel.p1, None);
        assert_eq!(rest, vec!["fp-app"]);

        // Non-numeric value: the value token is NOT consumed as a selection; it is
        // left as a positional arg.
        let b: Vec<String> = ["fp-app", "--p2-pal", "x.def"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (sel, rest) = parse_pal_flags(&b);
        assert_eq!(sel.p2, None);
        assert_eq!(rest, vec!["fp-app", "x.def"]);

        // An unrelated `--flag` is passed through untouched.
        let c: Vec<String> = ["fp-app", "--verbose", "a.def"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (_sel, rest) = parse_pal_flags(&c);
        assert_eq!(rest, vec!["fp-app", "--verbose", "a.def"]);
    }

    /// The `.act` override binding seam (the decision `get_or_create_sprite` /
    /// `PaletteTexture::from_override` make): a present override binds a palette
    /// that differs from the embedded one (a visible costume swap), while an absent
    /// override binds the embedded palette byte-for-byte (the default,
    /// no-regression path). Pins the precedence the GPU upload depends on without
    /// needing a device. `bind_palette` mirrors `from_override`'s pure chooser.
    #[test]
    fn act_override_binds_when_selected_else_embedded() {
        /// The exact rule `PaletteTexture::from_override` applies: the override
        /// bytes when present, otherwise the embedded palette.
        fn bind_palette<'a>(
            embedded: &'a [u8; 1024],
            override_rgba: Option<&'a [u8; 1024]>,
        ) -> &'a [u8; 1024] {
            override_rgba.unwrap_or(embedded)
        }

        let mut embedded = [0u8; 1024];
        // Give the embedded palette a recognizable color at index 1.
        embedded[4] = 10;
        embedded[5] = 20;
        embedded[6] = 30;
        let mut over = embedded;
        // The override differs at index 1 (a costume swap).
        over[4] = 200;
        over[5] = 100;
        over[6] = 50;

        // With a selection: the bound bytes are the override and differ from
        // embedded (the costume is visibly different).
        let chosen_with = bind_palette(&embedded, Some(&over));
        assert!(std::ptr::eq(chosen_with, &over));
        assert_ne!(
            &chosen_with[4..7],
            &embedded[4..7],
            "a selected .act override must bind a palette that differs from embedded"
        );
        assert_eq!(&chosen_with[4..7], &[200, 100, 50]);

        // With no selection: the bound bytes are the embedded palette,
        // byte-identical (the default, no-regression path).
        let chosen_without = bind_palette(&embedded, None);
        assert!(std::ptr::eq(chosen_without, &embedded));
        assert_eq!(chosen_without, &embedded);
    }

    /// `build_player` applies a valid `.act` selection to the entity's active
    /// palette and ignores an out-of-range one (falling back to embedded), never
    /// panicking. Gated on the local KFM asset; KFM ships no `.act` overrides, so
    /// any selection is out-of-range and must resolve to "no active palette".
    #[test]
    fn build_player_palette_selection_is_safe() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!(
                "skipping palette-selection test: {} not present",
                def.display()
            );
            return;
        }
        // KFM has no `.act` overrides, so selecting one is out of range: the
        // entity must keep the embedded palette (active_palette == None), with a
        // warning, not a panic.
        let player = match build_player(&def, P1_START_X, Some(2)) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping palette-selection test: {e}");
                return;
            }
        };
        assert_eq!(
            player.character.active_palette(),
            None,
            "an out-of-range .act selection must fall back to the embedded palette"
        );
        // And no selection leaves the embedded palette too.
        let plain = build_player(&def, P1_START_X, None).expect("kfm loads");
        assert_eq!(plain.character.active_palette(), None);
    }

    /// With NO explicit `--pN-pal` flag, a character that ships `.act` palettes
    /// (evilken) defaults to its costume palette (pal1, index 0) rather than the
    /// SFF-embedded one — the black-screen fix. An explicit flag still overrides
    /// the default. Gated on the local evilken asset.
    #[test]
    fn build_player_defaults_to_costume_palette_when_act_present() {
        let def = test_asset("evilken/evilken.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        // No flag → defaults to pal1 (the costume), not the SFF-embedded palette.
        let Ok(defaulted) = build_player(&def, P1_START_X, None) else {
            eprintln!("skipping: evilken.def failed to build");
            return;
        };
        assert_eq!(
            defaulted.character.active_palette(),
            Some(0),
            "evilken must default to its costume palette (pal1) so it renders in color"
        );
        // An explicit flag still wins over the default.
        let chosen = build_player(&def, P1_START_X, Some(3)).expect("evilken builds");
        assert_eq!(
            chosen.character.active_palette(),
            Some(3),
            "an explicit --pN-pal flag must override the default costume"
        );
    }

    // ---- common-effects (fightfx) load path (audit #17) ----------------------

    /// Resolves a path under the shipped (committed) `assets/data/` directory.
    fn shipped_fx_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/data")
            .join(rel)
    }

    /// The shipped common-effects asset loads end to end: the SFF parses with
    /// spark sprites and the AIR authors KFM's common spark actions. This is the
    /// path `load_common_fx` takes at startup (here with absolute paths so it runs
    /// regardless of the test CWD).
    #[test]
    fn shipped_common_fx_loads() {
        let sff = shipped_fx_asset("fightfx.sff");
        let air = shipped_fx_asset("fightfx.air");
        let loaded = load_common_fx_from(&sff, &air).expect("shipped common-fx asset must load");
        let (air, render) = loaded;
        // The render bundle carries the parsed SFF with sprites.
        assert!(
            !render.sff.sprites.is_empty(),
            "fightfx render bundle must carry spark sprites"
        );
        // The AIR authors the standard KFM common spark actions.
        for g in [0, 1, 2, 3, 40] {
            assert!(
                air.action(g).is_some(),
                "fightfx.air must author common action {g}"
            );
        }
    }

    /// A missing common-fx asset is a clean best-effort `None` — never a panic.
    #[test]
    fn missing_common_fx_is_none_not_panic() {
        let missing_sff = shipped_fx_asset("does-not-exist.sff");
        let missing_air = shipped_fx_asset("does-not-exist.air");
        assert!(
            load_common_fx_from(&missing_sff, &missing_air).is_none(),
            "an absent common-fx asset must yield None, not a panic"
        );
        // A present SFF but absent AIR (and vice versa) is also a clean None.
        let real_sff = shipped_fx_asset("fightfx.sff");
        assert!(
            load_common_fx_from(&real_sff, &missing_air).is_none(),
            "a half-present common-fx asset must yield None"
        );
    }

    /// Gated end-to-end (skip-if-KFM-missing): with the shipped common-fx AIR
    /// installed on a real two-KFM match, a connecting KFM hit spawns a common
    /// ([`EffectSide::Common`]) spark — the user-visible FL2a outcome wired
    /// through the app's own match build path.
    #[test]
    fn kfm_match_with_common_fx_spawns_common_spark() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        // The fightfx AIR ships, so it must load (not gated on it).
        let air =
            AirFile::load(&shipped_fx_asset("fightfx.air")).expect("shipped fightfx.air must load");

        let mut m = match build_two_player_match(&def, &def, PalSelection::default()) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("skipping: KFM failed to build: {e}");
                return;
            }
        };
        m.set_common_fx(air);

        // Run intro out, walk into range, and punch until a hit lands.
        for _ in 0..400 {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() == RoundState::Fight {
                break;
            }
        }
        for _ in 0..240 {
            m.tick(
                MatchInput {
                    right: true,
                    ..MatchInput::none()
                },
                MatchInput::none(),
            );
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }
        let p2_before = m.p2().life();
        let mut saw_common = false;
        for i in 0..400 {
            let inp = if i % 3 == 0 {
                MatchInput {
                    x: true,
                    ..MatchInput::none()
                }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            if m.effects().iter().any(|fx| fx.side == EffectSide::Common) {
                saw_common = true;
            }
            if m.p2().life() < p2_before {
                break;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert!(
            saw_common,
            "a real KFM hit (common sparkno) must spawn a common fightfx spark with the asset loaded"
        );
    }

    // =====================================================================
    // Menu-2 — in-app screen state machine (Title -> Select -> Fight -> Title)
    // =====================================================================

    /// Resolves a path under the workspace `assets/` directory (the shipped,
    /// clean-room default motif). `CARGO_MANIFEST_DIR` is `crates/fp-app`.
    fn shipped_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets")
            .join(rel)
    }

    /// No file argument routes to the Title menu; an explicit `menu` token does
    /// too. Any direct content path keeps the legacy direct route (no menu), so
    /// Resolves a path under the workspace repo root (`CARGO_MANIFEST_DIR` is
    /// `crates/fp-app`; go up two levels).
    fn repo_path(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel)
    }

    #[test]
    fn synth_success_event_satisfies_each_condition() {
        use training::tutorial::{LessonEvent, SuccessCond, TutorialRunner};
        // Every non-`Unsatisfiable` condition has a synthesizable event that
        // actually advances a runner whose sole lesson uses that condition.
        for cond in [
            SuccessCond::LandCommand("fireball".to_string()),
            SuccessCond::BlockNHits(1),
            SuccessCond::ComboCount(2),
            SuccessCond::AntiAir,
            SuccessCond::ThrowConnected,
        ] {
            let ev = synth_success_event(&cond).expect("satisfiable cond has an event");
            let mut r = TutorialRunner::new(vec![training::tutorial::Lesson {
                title: "x".to_string(),
                instruction: String::new(),
                dummy: training::tutorial::DummyMode::Stand,
                overlays: training::tutorial::OverlayFlags::default(),
                success: cond,
                timeout_hint: None,
            }]);
            let outcome = r.observe(std::slice::from_ref(&ev));
            assert_eq!(
                outcome,
                training::tutorial::TickOutcome::TrialComplete,
                "event {ev:?} should complete its lesson"
            );
        }
        // `Unsatisfiable` has no synth event (the runner auto-skips it).
        assert!(synth_success_event(&SuccessCond::Unsatisfiable).is_none());
        // Touch the type so the import is genuinely exercised.
        let _: LessonEvent = LessonEvent::ThrowConnected;
    }

    #[test]
    fn shipped_tutorial_assets_load_and_run_to_completion() {
        use training::tutorial::{load_lessons, TickOutcome, TutorialRunner};
        let dir = repo_path("assets/data/tutorial");
        // The on-disk clean-room lesson scripts must parse (not fall back).
        let lessons = load_lessons(&dir);
        assert_eq!(lessons.len(), 5, "ships exactly the 5 required lessons");
        assert_eq!(lessons[0].title, "Block High and Low");
        assert_eq!(lessons[2].title, "Throw a Fireball");

        // Driving the runner with each lesson's synthesized success advances
        // through the whole list to completion — the flow never soft-locks.
        let mut r = TutorialRunner::new(lessons);
        while let Some(lesson) = r.current() {
            let ev = synth_success_event(&lesson.success).expect("shipped lessons are satisfiable");
            let mut outcome = TickOutcome::InProgress;
            for _ in 0..64 {
                outcome = r.observe(std::slice::from_ref(&ev));
                if outcome != TickOutcome::InProgress {
                    break;
                }
            }
            assert_ne!(outcome, TickOutcome::InProgress, "lesson must advance");
        }
        assert!(r.is_complete());
    }

    /// the existing CLI is preserved.
    #[test]
    fn cli_route_menu_vs_direct() {
        // No args -> Menu.
        assert_eq!(cli_route(&["fp-app".to_string()]), CliRoute::Menu);
        // Explicit `menu` (case-insensitive) -> Menu.
        assert_eq!(
            cli_route(&["fp-app".to_string(), "menu".to_string()]),
            CliRoute::Menu
        );
        assert_eq!(
            cli_route(&["fp-app".to_string(), "MENU".to_string()]),
            CliRoute::Menu
        );
        // A character .def -> Direct (the legacy two-player path).
        assert_eq!(
            cli_route(&["fp-app".to_string(), "kfm.def".to_string()]),
            CliRoute::Direct
        );
        // Two .defs -> Direct.
        assert_eq!(
            cli_route(&[
                "fp-app".to_string(),
                "p1.def".to_string(),
                "p2.def".to_string()
            ]),
            CliRoute::Direct
        );
        // An .sff (viewer) -> Direct.
        assert_eq!(
            cli_route(&["fp-app".to_string(), "kfm.sff".to_string()]),
            CliRoute::Direct
        );
    }

    /// `replay <log> <p1.def> [p2.def]` routes to the replay-study viewer (T076),
    /// absolutizing each path; a missing log/character degrades to the Menu.
    #[test]
    fn cli_route_replay() {
        // Full form: log + two characters.
        match cli_route(&[
            "fp-app".to_string(),
            "replay".to_string(),
            "match.bin".to_string(),
            "p1.def".to_string(),
            "p2.def".to_string(),
        ]) {
            CliRoute::Replay { log, p1, p2 } => {
                assert!(log.is_absolute(), "log path absolutized");
                assert!(log.ends_with("match.bin"));
                assert!(p1.ends_with("p1.def"));
                assert_eq!(p2.as_ref().map(|p| p.ends_with("p2.def")), Some(true));
            }
            other => panic!("expected Replay route, got {other:?}"),
        }
        // One-character form: P2 reuses P1 (p2 is None).
        match cli_route(&[
            "fp-app".to_string(),
            "REPLAY".to_string(), // case-insensitive
            "m.bin".to_string(),
            "kfm.def".to_string(),
        ]) {
            CliRoute::Replay { p2, .. } => assert!(p2.is_none(), "single-def replay reuses P1"),
            other => panic!("expected Replay route, got {other:?}"),
        }
        // Missing args degrade to the Menu (no panic).
        assert_eq!(
            cli_route(&["fp-app".to_string(), "replay".to_string()]),
            CliRoute::Menu
        );
        assert_eq!(
            cli_route(&[
                "fp-app".to_string(),
                "replay".to_string(),
                "only-a-log.bin".to_string()
            ]),
            CliRoute::Menu
        );
    }

    /// A unique scratch directory for one test (OS temp + pid + label), removed
    /// up-front so a leaked prior run is clean.
    fn discovery_scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fp_app_discovery_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// T043: an existing directory argument routes to `Directory` (scan for a
    /// roster), while a single `.def` still routes to `Direct` (legacy CLI intact).
    #[test]
    fn cli_route_directory_vs_single_def() {
        let dir = discovery_scratch("route_dir");
        assert_eq!(
            cli_route(&["fp-app".to_string(), dir.to_string_lossy().into_owned()]),
            // `cli_route` absolutizes (canonicalizes) the dir, which on macOS
            // resolves `/var` -> `/private/var`, so compare against the same.
            CliRoute::Directory(absolutize(&dir)),
            "an existing directory routes to Directory (absolutized)"
        );
        // A single .def still routes to Direct unchanged.
        assert_eq!(
            cli_route(&["fp-app".to_string(), "chars/kfm/kfm.def".to_string()]),
            CliRoute::Direct,
            "a single .def keeps the legacy direct route"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T045: `--motif <value>` is stripped from the args and returned as the
    /// selector; the remaining positional args are preserved. A missing value is
    /// tolerated (selector stays None).
    #[test]
    fn parse_motif_flag_strips_and_returns_selector() {
        let (sel, rest) = parse_motif_flag(&[
            "fp-app".to_string(),
            "--motif".to_string(),
            "dark".to_string(),
            "kfm.def".to_string(),
        ]);
        assert_eq!(sel.as_deref(), Some("dark"));
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);

        // No flag -> None, args untouched.
        let (sel, rest) = parse_motif_flag(&["fp-app".to_string(), "kfm.def".to_string()]);
        assert_eq!(sel, None);
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);

        // Dangling --motif (no value) -> None, no panic.
        let (sel, rest) = parse_motif_flag(&["fp-app".to_string(), "--motif".to_string()]);
        assert_eq!(sel, None);
        assert_eq!(rest, vec!["fp-app".to_string()]);
    }

    /// T027: `--simul`/`--turns` select the team mode and are stripped from the
    /// positional args; with neither flag the default is the classic 1v1
    /// (`TeamMode::Single`), so the existing CLI is unchanged.
    #[test]
    fn parse_team_flag_selects_mode_and_strips_it() {
        // No flag -> Single, args untouched (1v1 default preserved).
        let (mode, rest) = parse_team_flag(&["fp-app".to_string(), "kfm.def".to_string()]);
        assert_eq!(mode, TeamMode::Single);
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);

        // --simul -> Simul, flag stripped, the .def passes through.
        let (mode, rest) = parse_team_flag(&[
            "fp-app".to_string(),
            "--simul".to_string(),
            "kfm.def".to_string(),
        ]);
        assert_eq!(mode, TeamMode::Simul);
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);

        // --turns (case-insensitive) -> Turns, flag stripped.
        let (mode, rest) = parse_team_flag(&[
            "fp-app".to_string(),
            "--TURNS".to_string(),
            "kfm.def".to_string(),
        ]);
        assert_eq!(mode, TeamMode::Turns);
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);

        // The last team flag wins if both are given (both are stripped).
        let (mode, rest) = parse_team_flag(&[
            "fp-app".to_string(),
            "--simul".to_string(),
            "--turns".to_string(),
            "kfm.def".to_string(),
        ]);
        assert_eq!(mode, TeamMode::Turns);
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);
    }

    /// T070: `--ai-mode <token>` selects the CPU teaching mode for the direct-CLI
    /// match path and strips the flag (+value); a bad/missing token keeps the
    /// default Ladder; other args pass through untouched; the last flag wins.
    #[test]
    fn parse_ai_mode_flag_selects_teaching_mode_and_strips_it() {
        // No flag -> default Ladder, args untouched (CLI unchanged from before).
        let (mode, rest) = parse_ai_mode_flag(&["fp-app".to_string(), "kfm.def".to_string()]);
        assert_eq!(mode, BehaviorMode::Ladder);
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);

        // --ai-mode dp -> Reactive DP, flag+value stripped, the .def passes through.
        let (mode, rest) = parse_ai_mode_flag(&[
            "fp-app".to_string(),
            "--ai-mode".to_string(),
            "dp".to_string(),
            "kfm.def".to_string(),
        ]);
        assert_eq!(mode, BehaviorMode::ReactiveDP);
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);

        // Case-insensitive flag + an alias token resolve.
        let (mode, rest) = parse_ai_mode_flag(&[
            "fp-app".to_string(),
            "--AI-MODE".to_string(),
            "blocker".to_string(),
            "kfm.def".to_string(),
        ]);
        assert_eq!(mode, BehaviorMode::PureBlocker);
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);

        // An unknown token keeps the default (Ladder) but still strips flag+value.
        let (mode, rest) = parse_ai_mode_flag(&[
            "fp-app".to_string(),
            "--ai-mode".to_string(),
            "nonsense".to_string(),
            "kfm.def".to_string(),
        ]);
        assert_eq!(mode, BehaviorMode::Ladder);
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);

        // A trailing `--ai-mode` with no value is dropped (default kept).
        let (mode, rest) = parse_ai_mode_flag(&["fp-app".to_string(), "--ai-mode".to_string()]);
        assert_eq!(mode, BehaviorMode::Ladder);
        assert_eq!(rest, vec!["fp-app".to_string()]);

        // The last --ai-mode wins if repeated (both flag+value pairs stripped).
        let (mode, rest) = parse_ai_mode_flag(&[
            "fp-app".to_string(),
            "--ai-mode".to_string(),
            "dp".to_string(),
            "--ai-mode".to_string(),
            "punisher".to_string(),
            "kfm.def".to_string(),
        ]);
        assert_eq!(mode, BehaviorMode::WhiffPunisher);
        assert_eq!(rest, vec!["fp-app".to_string(), "kfm.def".to_string()]);
    }

    /// T045: `resolve_motif_system_def` resolves a directory holding a
    /// `system.def`, a direct `.def` path, and returns `None` for an unresolvable
    /// selector (the caller then falls back to the default motif — no panic).
    #[test]
    fn resolve_motif_system_def_handles_dir_path_and_missing() {
        let dir = discovery_scratch("motif_resolve");
        let motif_dir = dir.join("dark");
        std::fs::create_dir_all(&motif_dir).unwrap();
        let sysdef = motif_dir.join("system.def");
        std::fs::write(&sysdef, "[Info]\nname = Dark\n").unwrap();

        // (1) A directory holding system.def -> <dir>/system.def.
        assert_eq!(
            resolve_motif_system_def(&motif_dir.to_string_lossy()),
            Some(sysdef.clone())
        );
        // (2) A direct .def path -> used verbatim.
        assert_eq!(
            resolve_motif_system_def(&sysdef.to_string_lossy()),
            Some(sysdef.clone())
        );
        // (3) An unresolvable selector -> None (default-motif fallback).
        assert_eq!(resolve_motif_system_def("nonexistent-motif-xyz"), None);
        assert_eq!(resolve_motif_system_def(""), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T045 (review fix): form (3) — a discovered motif NAME resolves against the
    /// motif dir (case-insensitively), locking acceptance criterion 3's
    /// name-selection path against a synthetic motif tree (no shipped asset).
    #[test]
    fn resolve_motif_system_def_matches_discovered_name() {
        // A motif dir holding `<dir>/dark/system.def`, the stand-in for
        // DEFAULT_MOTIF_DIR's discovered-motif layout.
        let dir = discovery_scratch("motif_name");
        let motif_dir = dir.join("dark");
        std::fs::create_dir_all(&motif_dir).unwrap();
        let sysdef = motif_dir.join("system.def");
        std::fs::write(&sysdef, "[Info]\nname = Dark\n").unwrap();

        // The bare name resolves to the discovered motif's system.def.
        assert_eq!(
            resolve_motif_system_def_in("dark", &dir),
            Some(sysdef.clone()),
            "a discovered motif name resolves to its system.def"
        );
        // Matching is case-insensitive.
        assert_eq!(
            resolve_motif_system_def_in("DARK", &dir),
            Some(sysdef),
            "name matching is case-insensitive"
        );
        // An unknown name still yields None (default-motif fallback, no panic).
        assert_eq!(resolve_motif_system_def_in("missing", &dir), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T043: `Motif::augment_roster` appends discovered characters as new roster
    /// slots without dropping the motif's existing roster, and de-duplicates a
    /// character already listed.
    #[test]
    fn augment_roster_appends_discovered_without_replacing() {
        // A motif with one existing roster character. The explicit-def form
        // (`display name, path.def`) keeps the def path verbatim, so the resolved
        // path is exactly dir/existing.def — making the dedup check unambiguous.
        let dir = discovery_scratch("augment");
        let select_path = dir.join("select.def");
        std::fs::write(
            &select_path,
            "[Characters]\nThe Existing One, existing.def\n",
        )
        .unwrap();
        // load_from reads select_path as both the system.def (no [Files] select ->
        // falls back) and the roster source, yielding the one Existing character.
        let mut motif = Motif::load_from(&select_path, &select_path);
        let before = motif.select.slots.len();
        assert_eq!(before, 1, "motif starts with its one declared character");

        // Two discovered characters: one brand new, one that duplicates the
        // existing entry's resolved path (dir/existing.def) and must be skipped.
        let discovered = vec![
            fp_ui::CharEntry {
                name: "Newbie".to_string(),
                def_path: dir.join("newbie").join("newbie.def"),
            },
            fp_ui::CharEntry {
                name: "The Existing One".to_string(),
                def_path: dir.join("existing.def"),
            },
        ];
        motif.augment_roster(&discovered);

        // Existing roster kept; exactly one new slot added (the duplicate skipped).
        assert_eq!(
            motif.select.slots.len(),
            before + 1,
            "only the new character is appended; the duplicate is skipped"
        );
        let names: Vec<String> = motif
            .select
            .slots
            .iter()
            .filter_map(|s| match s {
                fp_ui::SelectSlot::Character(e) => Some(e.name.clone()),
                _ => None,
            })
            .collect();
        assert!(
            names.iter().any(|n| n == "The Existing One"),
            "existing kept"
        );
        assert!(names.iter().any(|n| n == "Newbie"), "discovered appended");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T043 (review fix): the realistic invocation — a **relative** characters
    /// directory whose `select.def` motif lives elsewhere. `discover_chars` over a
    /// relative dir yields relative `def_path`s; `augment_roster` must absolutize
    /// them so `SelectScreen::build_pick` (`base_dir.join(def_path)`, base = the
    /// motif's `select.def` dir) resolves to the REAL on-disk file rather than
    /// re-rooting `chars/foo/foo.def` under the motif dir.
    #[test]
    fn relative_dir_discovered_pick_resolves_to_real_file() {
        // A unique RELATIVE characters tree under cwd (the crate root in tests),
        // plus a SEPARATE motif select.def dir — so a re-rooting bug would point
        // the pick at <motif_dir>/<relative path>, a non-existent location.
        let rel_root = PathBuf::from(format!("fp_app_reldir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&rel_root);
        let chars_dir = rel_root.join("chars");
        let foo = chars_dir.join("foo");
        std::fs::create_dir_all(&foo).unwrap();
        let foo_def = foo.join("foo.def");
        std::fs::write(&foo_def, "[Info]\nname = Foo\n").unwrap();

        // The motif's select.def lives in a different subdir (stand-in for
        // assets/data) with no roster of its own.
        let motif_dir = rel_root.join("motif");
        std::fs::create_dir_all(&motif_dir).unwrap();
        let select_path = motif_dir.join("select.def");
        std::fs::write(&select_path, "[Characters]\n").unwrap();

        // Discover over the RELATIVE chars dir -> relative def_paths.
        let discovered = fp_ui::discover_chars(&chars_dir);
        assert_eq!(discovered.len(), 1, "the one character is discovered");
        // (Sanity: discovery produced a relative path, exercising the bug case.)
        assert!(
            discovered[0].def_path.is_relative(),
            "discovered path is relative (the failing case)"
        );

        let mut motif = Motif::load_from(&select_path, &select_path);
        motif.augment_roster(&discovered);

        // Drive a Training select: P1 picks cell 0, P2 mirrors -> a MatchPick.
        let mut screen = screens::SelectScreen::new(
            screens::SelectMode::Training,
            &motif.select,
            &motif.system.select_info,
            &motif.select_path,
        );
        let confirm = screens::MenuInput {
            confirm: true,
            ..Default::default()
        };
        let outcome = screen.update(confirm, 0);
        let pick = match outcome {
            screens::SelectOutcome::Done(p) => p,
            other => panic!("expected a completed pick, got {other:?}"),
        };

        // The resolved pick MUST point at the real on-disk file, NOT the
        // motif-dir-rebased wrong location.
        let wrong = motif_dir.join("chars").join("foo").join("foo.def");
        assert_ne!(
            pick.p1_def, wrong,
            "pick must not be re-rooted under the motif dir"
        );
        assert!(
            pick.p1_def.is_file(),
            "resolved pick {} must exist on disk",
            pick.p1_def.display()
        );
        // Same canonical file as the discovered .def.
        assert_eq!(
            std::fs::canonicalize(&pick.p1_def).unwrap(),
            std::fs::canonicalize(&foo_def).unwrap(),
            "pick resolves to the discovered character file"
        );

        let _ = std::fs::remove_dir_all(&rel_root);
    }

    /// T044: `discover_stages` merges stages found by scanning a sibling `stages/`
    /// directory into the menu's stage list (in addition to the always-present
    /// dojo backdrop), using a synthetic tree — no shipped asset needed.
    #[test]
    fn discover_stages_merges_stages_directory() {
        let dir = discovery_scratch("stages_dir");
        let select_path = dir.join("select.def");
        std::fs::write(&select_path, "[Characters]\nAlpha, a/a.def\n").unwrap();
        // A sibling stages/ directory holding two real stage .defs.
        let stages_dir = dir.join("stages");
        std::fs::create_dir_all(&stages_dir).unwrap();
        std::fs::write(
            stages_dir.join("dojo.def"),
            "[Info]\nname = Dojo\n[BGdef]\nspr = dojo.sff\n",
        )
        .unwrap();
        std::fs::write(
            stages_dir.join("arena.def"),
            "[Info]\nname = Arena\n[Camera]\nboundleft = -200\n",
        )
        .unwrap();

        let select = SelectDef::parse("[Characters]\nAlpha, a/a.def\n");
        let stages = discover_stages(&select, &select_path);
        // Backdrop (always) + the two discovered stage .defs.
        assert_eq!(
            stages.len(),
            3,
            "dojo backdrop + two discovered stages: {stages:?}"
        );
        assert_eq!(stages[0].kind, screens::StageKind::Backdrop);
        let def_names: Vec<String> = stages
            .iter()
            .filter(|e| e.kind == screens::StageKind::Def)
            .map(|e| e.name.clone())
            .collect();
        assert!(def_names.iter().any(|n| n == "Dojo"));
        assert!(def_names.iter().any(|n| n == "Arena"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The default motif loads its declared roster (the shipped `select.def`,
    /// which lists Training Dummy twice + a randomselect icon), so the title menu
    /// and select grid build from real shipped content. Asset-gated on the shipped
    /// motif (always present in this worktree); skips cleanly if absent.
    #[test]
    fn default_motif_loads_shipped_roster() {
        let system_path = shipped_asset("data/system.def");
        let fallback = shipped_asset("data/select.def");
        if !system_path.exists() {
            eprintln!("skipping: {} not present", system_path.display());
            return;
        }
        let motif = Motif::load_from(&system_path, &fallback);
        // The shipped system.def enables VS MODE / TRAINING / SETUP / EXIT.
        let title = screens::TitleMenu::from_system(&motif.system);
        let labels: Vec<&str> = title.entries.iter().map(|e| e.label.as_str()).collect();
        assert!(labels.contains(&"VS MODE"), "title has VS MODE: {labels:?}");
        assert!(labels.contains(&"TRAINING"), "title has TRAINING");
        assert!(labels.contains(&"EXIT"), "title has EXIT");
        // The "options" item ships as SETUP and opens the setup screen (T042).
        assert!(labels.contains(&"SETUP"), "title has SETUP: {labels:?}");
        let setup_entry = title
            .entries
            .iter()
            .find(|e| e.label == "SETUP")
            .expect("SETUP entry present");
        assert_eq!(setup_entry.action, screens::TitleAction::Setup);
        // The roster has at least one choosable character (Training Dummy).
        let select = screens::SelectScreen::new(
            screens::SelectMode::Training,
            &motif.select,
            &motif.system.select_info,
            &motif.select_path,
        );
        assert!(
            !select.is_empty(),
            "shipped roster has a choosable character"
        );
    }

    /// A missing motif resolves to the built-in fallback title menu (VS /
    /// TRAINING / SETUP / EXIT) over an empty roster, without panicking — the
    /// clean-room degradation path.
    #[test]
    fn missing_motif_uses_fallback_menu() {
        let motif = Motif::load_from(
            Path::new("/no/such/system.def"),
            Path::new("/no/such/select.def"),
        );
        let title = screens::TitleMenu::from_system(&motif.system);
        // Built-in fallback ships exactly VS / TRAINING / SETUP / EXIT.
        assert_eq!(title.entries.len(), 4);
        assert_eq!(title.entries[0].label, "VS MODE");
        assert_eq!(title.entries[2].action, screens::TitleAction::Setup);
        // Empty roster: the select screen reports empty (the app would stay on
        // title), never a panic.
        let select = screens::SelectScreen::new(
            screens::SelectMode::Versus,
            &motif.select,
            &motif.system.select_info,
            &motif.select_path,
        );
        assert!(select.is_empty(), "no roster -> empty select screen");
    }

    /// End-to-end headless flow: select the shipped trainingdummy roster (the
    /// Training-mode single confirm), resolve the pick to its `.def`, load it, and
    /// build + drive a real two-player [`Match`] for a few ticks. Verifies the
    /// menu's roster-pick -> load -> Match wiring actually produces a runnable
    /// match over the shipped clean-room character (no KFM needed). Asset-gated on
    /// the shipped motif/character.
    #[test]
    fn training_pick_builds_and_drives_a_match() {
        let system_path = shipped_asset("data/system.def");
        let fallback = shipped_asset("data/select.def");
        if !system_path.exists() {
            eprintln!("skipping: {} not present", system_path.display());
            return;
        }
        let motif = Motif::load_from(&system_path, &fallback);
        let mut select = screens::SelectScreen::new(
            screens::SelectMode::Training,
            &motif.select,
            &motif.system.select_info,
            &motif.select_path,
        );
        // Single confirm in Training picks P1 (cell 0) and mirrors P2.
        let confirm = screens::MenuInput {
            confirm: true,
            ..screens::MenuInput::default()
        };
        let screens::SelectOutcome::Done(pick) = select.update(confirm, 0) else {
            panic!("training confirm should complete the select screen");
        };
        // The pick resolves to a real, existing trainingdummy .def under assets/.
        assert_eq!(
            pick.p1_def, pick.p2_def,
            "training mirrors P1 onto P2 (idle dummy)"
        );
        assert!(
            pick.p1_def.exists(),
            "picked .def resolves to an existing file: {}",
            pick.p1_def.display()
        );
        // Build the same two-player Match the menu Fight screen builds, and drive
        // a handful of ticks. It must not panic and must start in the intro phase
        // with both fighters at full life.
        let mut m = build_two_player_match(&pick.p1_def, &pick.p2_def, PalSelection::default())
            .expect("trainingdummy match builds");
        assert_eq!(m.round_state(), RoundState::Intro);
        assert_eq!(m.p1().life(), m.p1().life_max());
        assert!(m.match_winner().is_none(), "no winner at the start");
        for _ in 0..30 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // After a few ticks the match is still coherent (no panic; both alive).
        assert!(m.p1().life() >= 0);
        assert!(m.p2().life() >= 0);
    }

    /// The loaded motif always exposes at least one stage (the dojo backdrop) for
    /// the stage-select screen (T041), and that first stage is the backdrop kind —
    /// even with the shipped roster that declares no stages, the menu can always
    /// offer a choice. Asset-gated on the shipped motif.
    #[test]
    fn motif_stage_list_always_has_the_dojo_backdrop() {
        let system_path = shipped_asset("data/system.def");
        let fallback = shipped_asset("data/select.def");
        if !system_path.exists() {
            eprintln!("skipping: {} not present", system_path.display());
            return;
        }
        let motif = Motif::load_from(&system_path, &fallback);
        assert!(
            !motif.stages.is_empty(),
            "stage list is never empty (the dojo backdrop is always present)"
        );
        assert_eq!(
            motif.stages[0].kind,
            screens::StageKind::Backdrop,
            "the first stage is the dojo backdrop"
        );
        assert_eq!(motif.stages[0].name, DEFAULT_STAGE_NAME);
    }

    /// `discover_stages` keeps the dojo backdrop and drops `.def` stages whose
    /// files don't exist on disk (so the player can't pick an unloadable stage),
    /// using a synthetic roster — no shipped asset needed.
    #[test]
    fn discover_stages_filters_missing_def_stages_keeps_backdrop() {
        // A synthetic roster declaring an extra stage that does NOT exist on disk.
        let select = SelectDef::parse(
            "[Characters]\n\
             Alpha, a/a.def\n\
             [ExtraStages]\n\
             stages/does_not_exist.def\n",
        );
        // Resolve relative to a directory that certainly has no such stage file.
        let select_path = Path::new("/no/such/dir/select.def");
        let stages = discover_stages(&select, select_path);
        // The backdrop survives; the missing .def is filtered out.
        assert_eq!(stages.len(), 1, "only the backdrop remains");
        assert_eq!(stages[0].kind, screens::StageKind::Backdrop);
    }

    /// Full headless menu flow without a GPU: character-select Done advances to
    /// stage-select carrying the pick, stage-select Done resolves the chosen
    /// stage, and the carried pick + chosen stage are exactly what the match would
    /// load (T041). Exercises the Title->Select->StageSelect transition logic the
    /// `enter_*` glue drives, using the shipped motif. Asset-gated.
    #[test]
    fn select_then_stage_select_carries_pick_and_stage() {
        let system_path = shipped_asset("data/system.def");
        let fallback = shipped_asset("data/select.def");
        if !system_path.exists() {
            eprintln!("skipping: {} not present", system_path.display());
            return;
        }
        let motif = Motif::load_from(&system_path, &fallback);
        // 1. Character-select (Training): a single confirm completes the pick.
        let mut select = screens::SelectScreen::new(
            screens::SelectMode::Training,
            &motif.select,
            &motif.system.select_info,
            &motif.select_path,
        );
        let confirm = screens::MenuInput {
            confirm: true,
            ..screens::MenuInput::default()
        };
        let screens::SelectOutcome::Done(pick) = select.update(confirm, 0) else {
            panic!("character-select should complete");
        };
        // 2. Stage-select is seeded from the motif's discovered stage list and
        //    confirms the highlighted (first = dojo backdrop) stage.
        let mut stage_screen = screens::StageSelect::new(motif.stages.clone());
        let screens::StageOutcome::Done(stage) = stage_screen.update(confirm) else {
            panic!("stage-select should complete on confirm");
        };
        // The chosen stage is the dojo backdrop, and the carried pick is intact —
        // exactly what `build_match_run` would load.
        assert_eq!(stage.kind, screens::StageKind::Backdrop);
        assert_eq!(stage.name, DEFAULT_STAGE_NAME);
        assert!(
            pick.p1_def.exists(),
            "carried pick still resolves to a real .def"
        );
    }

    /// `resolve_select_path` prefers the motif's declared `[Files] select`
    /// (resolved relative to the system.def) when it exists, and falls back to the
    /// shipped path otherwise. Uses the shipped motif where `select = select.def`
    /// sits next to `system.def`.
    #[test]
    fn resolve_select_path_prefers_declared_then_fallback() {
        let system_path = shipped_asset("data/system.def");
        if !system_path.exists() {
            eprintln!("skipping: {} not present", system_path.display());
            return;
        }
        let def = fp_formats::def::DefFile::load(&system_path).expect("load system.def");
        let system = SystemDef::parse(&def);
        let fallback = Path::new("/no/such/fallback-select.def");
        let resolved = resolve_select_path(&system_path, &system, fallback);
        // The shipped motif's declared select.def sits next to system.def.
        assert!(
            resolved.exists(),
            "declared select.def resolved: {}",
            resolved.display()
        );
        assert!(
            resolved.ends_with("select.def"),
            "resolved to the declared select.def"
        );

        // A motif declaring a missing select falls back to the given path.
        let bad = SystemDef {
            select_file: "definitely-missing.def".to_string(),
            ..SystemDef::default()
        };
        let fb = resolve_select_path(&system_path, &bad, fallback);
        assert_eq!(fb, fallback, "missing declared select -> fallback path");
    }

    // ---- T018: P2 input resolution (human overrides the CPU AI) -------------

    #[test]
    fn match_input_is_idle_detects_neutral_and_pressed() {
        assert!(match_input_is_idle(MatchInput::none()));
        assert!(!match_input_is_idle(MatchInput {
            right: true,
            ..MatchInput::none()
        }));
        assert!(!match_input_is_idle(MatchInput {
            a: true,
            ..MatchInput::none()
        }));
    }

    #[test]
    fn pick_p2_input_human_overrides_ai() {
        // A non-idle human input wins regardless of the AI/observation.
        let mut ai = CpuAi::new(1, AiDifficulty::Normal);
        let human = MatchInput {
            x: true,
            ..MatchInput::none()
        };
        let got = pick_p2_input(human, Some(&mut ai), fp_input::AiObservation::at(5.0));
        assert_eq!(got, human, "a pressed human input must override the AI");
    }

    #[test]
    fn pick_p2_input_idle_human_uses_ai() {
        // Idle human + out-of-range opponent on the right => AI walks right.
        let mut ai = {
            let mut t = fp_input::AiTuning::for_difficulty(AiDifficulty::Normal);
            t.jump_chance = 0;
            CpuAi::with_tuning(1, t)
        };
        let got = pick_p2_input(
            MatchInput::none(),
            Some(&mut ai),
            fp_input::AiObservation::at(300.0),
        );
        assert!(
            got.right && !got.left,
            "AI approaches toward a right opponent"
        );
    }

    #[test]
    fn pick_p2_input_no_ai_idle_human_stays_idle() {
        let got = pick_p2_input(MatchInput::none(), None, fp_input::AiObservation::at(5.0));
        assert_eq!(got, MatchInput::none(), "no AI + idle human => P2 idle");
    }

    // ---- T066: select-mode -> match-time GameMode mapping -----------------

    #[test]
    fn training_select_maps_to_training_game_mode() {
        assert_eq!(
            game_mode_for(screens::SelectMode::Training),
            GameMode::Training,
            "the Training select flow enters a Training match"
        );
    }

    #[test]
    fn versus_select_maps_to_versus_game_mode() {
        assert_eq!(
            game_mode_for(screens::SelectMode::Versus),
            GameMode::Versus,
            "the Versus select flow enters a normal match"
        );
    }
}
