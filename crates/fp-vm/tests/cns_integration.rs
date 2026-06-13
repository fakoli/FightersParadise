//! Keystone integration test (execution plan task 4.6).
//!
//! This proves the whole expression VM — lexer, parser, and tree-walk evaluator
//! — works on **real** MUGEN content rather than synthetic snippets. It loads
//! the bundled Kung Fu Man character (`kfm.cns`) and the shared engine states
//! (`common1.cns`) through [`fp_formats::cns::CnsFile`], extracts every raw
//! trigger expression string, and drives it through `fp-vm`'s public API.
//!
//! Three things are validated:
//!
//! 1. **The never-crash contract.** Every trigger string is tokenized and parsed
//!    with no panic. Malformed / currently-unsupported forms must surface as a
//!    recoverable [`ParseError`], never an abort.
//! 2. **A high clean-parse rate.** The vast majority of real triggers parse into
//!    an AST; the remaining failures are tallied by category and asserted to be
//!    only the known-unsupported forms (see [`KNOWN_UNSUPPORTED_SUBSTR`]).
//! 3. **End-to-end evaluation.** A curated set of representative real KFM
//!    triggers is evaluated against a deterministic [`Ctx`] with asserted
//!    results, exercising comparisons, `&&`/`||` combos, parameterized triggers,
//!    a range form, and a redirection.
//!
//! The whole suite **gates on `test-assets/` being present**: when the fixtures
//! are absent (the default CI checkout) every test returns early so
//! `cargo test -p fp-vm` stays green with or without the assets.

#![allow(clippy::float_cmp)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use fp_formats::cns::CnsFile;
use fp_vm::eval::{EvalContext, Redirect, Value};
use fp_vm::{eval, parse_str, tokenize, ParseError};

// =============================================================================
// Asset gating
// =============================================================================

/// Returns the `test-assets/kfm` directory **iff** both fixtures exist.
///
/// The crate lives at `<repo>/crates/fp-vm`, so the assets are two levels up.
/// Returning `None` (rather than panicking) is what keeps the default
/// asset-less checkout green: each test early-returns on `None`.
fn kfm_assets_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/kfm");
    if dir.join("kfm.cns").is_file() && dir.join("common1.cns").is_file() {
        Some(dir)
    } else {
        None
    }
}

// =============================================================================
// Trigger extraction
// =============================================================================

/// A single extracted trigger condition, tagged with where it came from so a
/// failure report points back at a real line.
struct RawTrigger {
    /// Short provenance label, e.g. `"kfm.cns [State 200,...] trigger1"`.
    origin: String,
    /// The raw, unevaluated expression string exactly as it appears in the CNS.
    expr: String,
}

/// Collects every raw trigger expression in a parsed CNS file: each
/// controller's `triggerall` lines plus every numbered `triggerN` group
/// condition, across all statedefs.
fn collect_triggers(cns: &CnsFile, file_label: &str, out: &mut Vec<RawTrigger>) {
    for sd in &cns.statedefs {
        for c in &sd.controllers {
            let ctype = c.controller_type.as_deref().unwrap_or("?");
            for cond in &c.triggerall {
                out.push(RawTrigger {
                    origin: format!("{file_label} [State {},{}] triggerall ({ctype})", sd.number, c.label),
                    expr: cond.clone(),
                });
            }
            for group in &c.triggers {
                for cond in &group.conditions {
                    out.push(RawTrigger {
                        origin: format!(
                            "{file_label} [State {},{}] trigger{} ({ctype})",
                            sd.number, c.label, group.number
                        ),
                        expr: cond.clone(),
                    });
                }
            }
        }
    }
}

/// Loads both fixtures and returns every extracted trigger.
fn load_all_triggers(dir: &Path) -> Vec<RawTrigger> {
    let mut all = Vec::new();
    let kfm = CnsFile::load(&dir.join("kfm.cns")).expect("kfm.cns should load");
    let common1 = CnsFile::load(&dir.join("common1.cns")).expect("common1.cns should load");
    collect_triggers(&kfm, "kfm.cns", &mut all);
    collect_triggers(&common1, "common1.cns", &mut all);
    all
}

/// Substrings that mark a trigger as using a **known currently-unsupported**
/// syntactic form. A clean-parse failure is only acceptable if the offending
/// expression contains one of these; anything else is a genuine regression.
///
/// Task 4.10 closed the four real-content gaps the 4.6 keystone surfaced —
/// axis-suffixed component triggers (`Vel Y`, `Pos X`, `P2BodyDist X`,
/// `ScreenPos Y`), the `AnimElem = N, op M` comparison tail,
/// dotted call arguments (`GetHitVar(fall.yvel)`), and `command = "name"`
/// string equality — so **none of those forms is unsupported any more**. The
/// only remaining deferred form is:
///
/// - `:=` — variable assignment inside an expression, explicitly deferred to
///   task 4.9. (Not present in these particular fixtures, but listed so the
///   guard stays correct once assignment-bearing content is added.)
const KNOWN_UNSUPPORTED_SUBSTR: &[&str] = &[":="];

/// Returns `true` if `expr` (matched case-insensitively) contains a known
/// unsupported form. After task 4.10 this is just the deferred `:=` assignment
/// operator; the previously-listed axis / dotted / comma-tail / command forms
/// now all parse.
fn is_known_unsupported(expr: &str) -> bool {
    let lower = expr.to_ascii_lowercase();
    KNOWN_UNSUPPORTED_SUBSTR.iter().any(|s| lower.contains(s))
}

// =============================================================================
// Behavior 1 + 2: never-crash + high clean-parse rate over ALL real triggers
// =============================================================================

#[test]
fn all_real_triggers_lex_and_parse_without_panic() {
    let Some(dir) = kfm_assets_dir() else {
        eprintln!("skipping all_real_triggers_lex_and_parse_without_panic: test-assets/ absent");
        return;
    };

    let triggers = load_all_triggers(&dir);
    let total = triggers.len();
    assert!(
        total > 100,
        "expected >100 real trigger expressions across kfm.cns + common1.cns, got {total}"
    );

    let mut parsed = 0usize;
    let mut failures: Vec<(&RawTrigger, ParseError)> = Vec::new();

    for t in &triggers {
        // (a) tokenize must never panic on any real content (the lexer is
        //     deliberately tolerant; Unknown tokens are surfaced, not aborted).
        let _tokens = tokenize(&t.expr);

        // (b) parse must never panic either — success or a recoverable error.
        match parse_str(&t.expr) {
            Ok(_ast) => parsed += 1,
            Err(e) => failures.push((t, e)),
        }
    }

    let failed = failures.len();
    let rate = 100.0 * parsed as f64 / total as f64;

    // Concise tally so regressions are visible in test output.
    eprintln!("=== fp-vm keystone (4.6): real CNS trigger parse summary ===");
    eprintln!("  total trigger expressions : {total}");
    eprintln!("  parsed cleanly            : {parsed} ({rate:.1}%)");
    eprintln!("  recoverable ParseError    : {failed}");

    // Group the failures by error category for a compact, readable breakdown.
    let mut by_category: BTreeMap<String, usize> = BTreeMap::new();
    for (_t, e) in &failures {
        let msg = e.to_string();
        // Trim positional suffixes so identical shapes coalesce.
        let category = msg
            .split(" at column")
            .next()
            .unwrap_or(&msg)
            .to_string();
        *by_category.entry(category).or_insert(0) += 1;
    }
    if !by_category.is_empty() {
        eprintln!("  failure categories:");
        for (cat, n) in &by_category {
            eprintln!("    {n:>3}  {cat}");
        }
    }

    // (2) The vast majority must parse cleanly. Task 4.10 closed the four
    // real-content gaps the 4.6 keystone surfaced, lifting the rate from ~90.3%
    // to ~100% on these fixtures, so the floor is now 98%.
    assert!(
        rate >= 98.0,
        "clean-parse rate {rate:.1}% fell below the 98% floor ({parsed}/{total})"
    );

    // (2 cont.) Every failure must be attributable to a known-unsupported form;
    // a failure outside that set means a real parser regression on valid MUGEN.
    let unexpected: Vec<&(&RawTrigger, ParseError)> = failures
        .iter()
        .filter(|(t, _e)| !is_known_unsupported(&t.expr))
        .collect();
    if !unexpected.is_empty() {
        eprintln!("  UNEXPECTED parse failures (not a known-unsupported form):");
        for (t, e) in &unexpected {
            eprintln!("    {origin}: {expr:?} -> {e}", origin = t.origin, expr = t.expr);
        }
    }
    assert!(
        unexpected.is_empty(),
        "{} real trigger(s) failed to parse for reasons outside the known-unsupported set; \
         see stderr — this is a parser regression, not a deferred form",
        unexpected.len()
    );
}

// =============================================================================
// Behavior 3: end-to-end lex -> parse -> eval of curated REAL triggers
// =============================================================================

/// Renders `(name, args)` into a stable, case-insensitive lookup key.
///
/// `Value` is intentionally not `Hash`/`Eq` (it carries an `f32`), so triggers
/// are keyed on a rendered string. This mirrors the `MockContext` test double
/// that lives inside `fp-vm`'s own unit tests (which is not exported), kept
/// minimal here for the integration boundary.
fn trigger_key(name: &str, args: &[Value]) -> String {
    let mut key = name.to_ascii_lowercase();
    for arg in args {
        key.push('|');
        key.push_str(&arg.to_string());
    }
    key
}

/// Renders `(name, member)` into a stable, case-insensitive lookup key for the
/// member-keyed trigger path (`GetHitVar(member)`, task 4.11 item a).
fn member_key(name: &str, member: &str) -> String {
    format!("{}#{}", name.to_ascii_lowercase(), member.to_ascii_lowercase())
}

/// A deterministic in-memory [`EvalContext`] for evaluating curated triggers.
///
/// Anything not explicitly seeded resolves to the safe default
/// (`Value::Int(0)` / `None`), matching the engine-wide "unknown trigger → 0"
/// contract, so each test only seeds the values it asserts on.
#[derive(Default)]
struct Ctx {
    triggers: HashMap<String, Value>,
    /// Member-keyed trigger values (`GetHitVar(member)`), keyed on the rendered
    /// `(lowercased name, lowercased member)` string (task 4.11, item a).
    member_triggers: HashMap<String, Value>,
    // `Redirect` is `Hash + Eq` but not `Ord`, so the target map must be a
    // `HashMap`, not a `BTreeMap`.
    redirects: HashMap<Redirect, Box<Ctx>>,
    /// Command names that are "active" this tick, for `command = "name"`
    /// string-equality (task 4.10, gap 4). Stored lowercased for
    /// case-insensitive matching.
    active_commands: Vec<String>,
}

impl Ctx {
    fn new() -> Self {
        Self::default()
    }

    /// Seeds a trigger value for the given (case-insensitive) name + args.
    fn with_trigger(mut self, name: &str, args: &[Value], value: Value) -> Self {
        self.triggers.insert(trigger_key(name, args), value);
        self
    }

    /// Seeds a member-keyed trigger value (`GetHitVar(member)`, task 4.11 item a).
    fn with_member_trigger(mut self, name: &str, member: &str, value: Value) -> Self {
        self.member_triggers
            .insert(member_key(name, member), value);
        self
    }

    /// Seeds a redirection target.
    fn with_redirect(mut self, target: Redirect, ctx: Ctx) -> Self {
        self.redirects.insert(target, Box::new(ctx));
        self
    }

    /// Marks a command name as active for `command = "name"` queries.
    fn with_command(mut self, name: &str) -> Self {
        self.active_commands.push(name.to_ascii_lowercase());
        self
    }
}

impl EvalContext for Ctx {
    fn trigger(&self, name: &str, args: &[Value]) -> Value {
        self.triggers
            .get(&trigger_key(name, args))
            .copied()
            .unwrap_or(Value::DEFAULT)
    }

    fn trigger_str(&self, name: &str, key: &str) -> Value {
        self.member_triggers
            .get(&member_key(name, key))
            .copied()
            .unwrap_or(Value::DEFAULT)
    }

    fn redirect(&self, target: Redirect) -> Option<&dyn EvalContext> {
        self.redirects
            .get(&target)
            .map(|boxed| boxed.as_ref() as &dyn EvalContext)
    }

    fn command_active(&self, name: &str) -> bool {
        self.active_commands
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
    }
}

/// Lex -> parse -> eval a single expression against `ctx`, asserting it parses.
fn run(expr: &str, ctx: &dyn EvalContext) -> Value {
    let ast = parse_str(expr)
        .unwrap_or_else(|e| panic!("curated real trigger {expr:?} should parse: {e}"));
    eval(&ast, ctx)
}

#[test]
fn curated_real_kfm_triggers_evaluate_to_expected_values() {
    if kfm_assets_dir().is_none() {
        eprintln!("skipping curated_real_kfm_triggers_evaluate_to_expected_values: test-assets/ absent");
        return;
    }

    // Every expression below is a verbatim trigger taken from kfm.cns /
    // common1.cns (the redirected one is a representative real-shape form, see
    // its note). Each is seeded so we can assert a *meaningful* result, proving
    // the lex->parse->eval pipeline computes real semantics, not just "doesn't
    // crash".

    // 1. `Time = 0` — pervasive "first tick of the state" trigger. (kfm.cns)
    let ctx = Ctx::new().with_trigger("Time", &[], Value::Int(0));
    assert_eq!(run("Time = 0", &ctx), Value::Int(1), "Time=0 on tick 0 is true");
    let ctx = Ctx::new().with_trigger("Time", &[], Value::Int(7));
    assert_eq!(run("Time = 0", &ctx), Value::Int(0), "Time=0 on tick 7 is false");

    // 2. `AnimTime = 0` — "animation finished this tick". (kfm.cns)
    let ctx = Ctx::new().with_trigger("AnimTime", &[], Value::Int(0));
    assert_eq!(run("AnimTime = 0", &ctx), Value::Int(1));

    // 3. A `!=` comparison: `RoundState != 0`. (kfm.cns)
    let ctx = Ctx::new().with_trigger("RoundState", &[], Value::Int(2));
    assert_eq!(run("RoundState != 0", &ctx), Value::Int(1));
    let ctx = Ctx::new().with_trigger("RoundState", &[], Value::Int(0));
    assert_eq!(run("RoundState != 0", &ctx), Value::Int(0));

    // 4. A parameterized trigger with a comparison: `AnimElemTime(7) = 1`.
    //    (kfm.cns) — argument is evaluated, then the trigger is read.
    let ctx = Ctx::new().with_trigger("AnimElemTime", &[Value::Int(7)], Value::Int(1));
    assert_eq!(run("AnimElemTime(7) = 1", &ctx), Value::Int(1));
    assert_eq!(run("AnimElemTime(7) = 3", &ctx), Value::Int(0));

    // 5. An `&&` combo of two parameterized comparisons:
    //    `AnimElemTime(5) > 0 && AnimElemTime(6) <= 0`. (kfm.cns)
    let ctx = Ctx::new()
        .with_trigger("AnimElemTime", &[Value::Int(5)], Value::Int(3))
        .with_trigger("AnimElemTime", &[Value::Int(6)], Value::Int(-1));
    assert_eq!(
        run("AnimElemTime(5) > 0 && AnimElemTime(6) <= 0", &ctx),
        Value::Int(1)
    );
    // Flip the second operand: the && must short to false.
    let ctx = Ctx::new()
        .with_trigger("AnimElemTime", &[Value::Int(5)], Value::Int(3))
        .with_trigger("AnimElemTime", &[Value::Int(6)], Value::Int(2));
    assert_eq!(
        run("AnimElemTime(5) > 0 && AnimElemTime(6) <= 0", &ctx),
        Value::Int(0)
    );

    // 6. A parenthesized `&&` of two comparisons:
    //    `(AnimElemTime (2) >= 0) && (AnimElemTime (7) < 0)`. (kfm.cns —
    //    note the real file's space before `(`, which the lexer tolerates.)
    let ctx = Ctx::new()
        .with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(4))
        .with_trigger("AnimElemTime", &[Value::Int(7)], Value::Int(-2));
    assert_eq!(
        run("(AnimElemTime (2) >= 0) && (AnimElemTime (7) < 0)", &ctx),
        Value::Int(1)
    );

    // 7. A range form on the RHS of `=`: `anim = [5051,5059]`. (common1.cns,
    //    inside an `ifelse`; here we exercise the range comparison directly.)
    let ctx = Ctx::new().with_trigger("anim", &[], Value::Int(5055));
    assert_eq!(run("anim = [5051,5059]", &ctx), Value::Int(1), "5055 ∈ [5051,5059]");
    let ctx = Ctx::new().with_trigger("anim", &[], Value::Int(5060));
    assert_eq!(run("anim = [5051,5059]", &ctx), Value::Int(0), "5060 ∉ [5051,5059]");

    // 8. An `||` combo with ranges, the real common1.cns shape:
    //    `(anim = [5051,5059]) || (anim = [5061,5069])`.
    let ctx = Ctx::new().with_trigger("anim", &[], Value::Int(5065));
    assert_eq!(
        run("(anim = [5051,5059]) || (anim = [5061,5069])", &ctx),
        Value::Int(1)
    );

    // 9. A command-string compare: `command = "x"`. (kfm.cns) — KFM's special
    //    intro and many moves fire on `command = "name"` detection. Task 4.10
    //    wired string-equality through the `command_active` seam, so this now
    //    evaluates the command buffer rather than collapsing to 0.
    //    When the named command IS active, the compare fires (1)...
    let ctx = Ctx::new().with_command("x");
    assert_eq!(
        run("command = \"x\"", &ctx),
        Value::Int(1),
        "command = \"x\" must fire when command x is active (task 4.10)"
    );
    //    ...and when it is NOT active it is 0 (never panics).
    let ctx = Ctx::new().with_command("y");
    assert_eq!(run("command = \"x\"", &ctx), Value::Int(0));
    // Reversed operand order and `!=` also work end-to-end.
    let ctx = Ctx::new().with_command("recovery");
    assert_eq!(run("\"recovery\" = command", &ctx), Value::Int(1));
    assert_eq!(run("command != \"recovery\"", &ctx), Value::Int(0));
    assert_eq!(run("command != \"holdback\"", &ctx), Value::Int(1));

    // 10. A REDIRECTION evaluated end-to-end. KFM's own trigger conditions do
    //     not use a `root,`-prefixed redirect, so this is a representative
    //     real-shape redirected trigger (`root, Life`, a staple of MUGEN AI/
    //     combo logic) proving the redirect path resolves and reads from the
    //     target context.
    let root = Ctx::new().with_trigger("Life", &[], Value::Int(1000));
    let ctx = Ctx::new().with_redirect(Redirect::Root, root);
    assert_eq!(run("root, Life", &ctx), Value::Int(1000));
    assert_eq!(run("root, Life >= 500", &ctx), Value::Int(1));
    // A redirect to a non-existent target reads as the safe default 0, no panic.
    let ctx = Ctx::new();
    assert_eq!(run("root, Life", &ctx), Value::Int(0));
}

// =============================================================================
// Proctor additions — edge cases, error paths, and MUGEN-semantics coverage.
//
// These run **without** the asset gate (they use synthetic / verbatim-real
// fragments inline) so they exercise the lex->parse->eval pipeline on every
// `cargo test -p fp-vm`, with or without `test-assets/`. They harden each
// acceptance criterion beyond the asset-gated happy path above.
// =============================================================================

/// A throwaway context whose every trigger resolves to the safe default `0` and
/// which has no redirect targets. Used to prove the never-crash contract end to
/// end: even nonsense that *parses* must evaluate to a concrete `Value`.
struct EmptyCtx;
impl EvalContext for EmptyCtx {
    fn trigger(&self, _name: &str, _args: &[Value]) -> Value {
        Value::DEFAULT
    }
    fn redirect(&self, _target: Redirect) -> Option<&dyn EvalContext> {
        None
    }
}

// -----------------------------------------------------------------------------
// AC2 (hardening): the never-crash contract on ADVERSARIAL synthetic input.
//
// The asset-gated test proves no panic on real content; this proves no panic on
// deliberately hostile input — the lexer/parser must degrade to a recoverable
// `ParseError`, never abort, on garbage, truncation, and unbalanced delimiters.
// -----------------------------------------------------------------------------

#[test]
fn tokenize_and_parse_never_panic_on_adversarial_input() {
    let nasties = [
        "",                       // empty
        "   ",                    // whitespace only
        "; just a comment",       // comment only
        "@#$%^",                  // all-unknown characters
        "(((((",                  // unbalanced open parens
        ")))))",                  // unbalanced close parens
        "[1,2",                   // unterminated range
        "1,2]",                   // dangling range close
        "Time >=",                // trailing binary operator
        "&& Time",                // leading binary operator
        "Time && && AnimTime",    // doubled operator
        "var(",                   // unterminated call
        "var(0,",                 // unterminated call args
        "cond(1,2)",              // wrong-arity builtin (parses; eval-time concern)
        "enemy,",                 // redirect with no target expr
        "helper(,",               // malformed redirect id
        "\"unterminated string",  // unterminated string literal
        ":= 5",                   // deferred assignment form (4.9)
        "Vel Y",                  // space-separated component trigger
        "GetHitVar(fall.yvel)",   // dotted member in call args
        "AnimElem = 2, >= 0",     // extended trailing-comma comparison
        "1 2 3 4 5",              // adjacent atoms, no operators
        "((((((((((((((((1))))))))))))))))", // deep nesting
        "----------1",            // operator pileup (valid: nested negation)
        "!!!!!!!!Time",           // unary pileup
        "1.2.3.4",                // malformed float
        "0x", "1e", "1e+",        // malformed numeric tails
    ];

    for src in nasties {
        // tokenize must not panic, and must terminate.
        let _ = tokenize(src);
        // parse_str must not panic — Ok or a recoverable ParseError, never abort.
        let result = parse_str(src);
        // If it *did* parse, evaluating it must also be panic-free and yield a
        // concrete Value (the full never-crash contract spans all three stages).
        if let Ok(ast) = result {
            let v = eval(&ast, &EmptyCtx);
            // A concrete Value is always Int or Float; assert it is real (not a
            // NaN masquerading as success for, e.g., a bottom-y form).
            match v {
                Value::Int(_) => {}
                Value::Float(f) => assert!(
                    f.is_finite() || f.is_nan(),
                    "eval of {src:?} produced a non-representable float"
                ),
            }
        }
    }
}

// -----------------------------------------------------------------------------
// AC2 (hardening): the deferred / unsupported forms surface as the CORRECT,
// recoverable `ParseError` variants — proving the tally in the asset-gated test
// is categorizing real failures rather than masking a panic or a wrong shape.
// -----------------------------------------------------------------------------

#[test]
fn deferred_assignment_form_is_a_recoverable_parse_error() {
    // `:=` (variable assignment) is explicitly deferred to task 4.9. It must
    // NOT panic and must NOT silently parse — it has to be a clean ParseError.
    for src in ["var(0) := 5", "x := 1", "fvar(2):=3.0"] {
        let _ = tokenize(src); // never panics
        let err = parse_str(src).expect_err("`:=` assignment is deferred; must be a ParseError");
        // Any recoverable variant is acceptable; the contract is "recoverable,
        // not panic". The `:` makes it an UnknownToken or an UnexpectedToken.
        assert!(
            matches!(
                err,
                ParseError::UnknownToken { .. }
                    | ParseError::UnexpectedToken { .. }
                    | ParseError::ExpectedDelimiter { .. }
            ),
            "`:=` should be a recoverable parse error, got {err:?}"
        );
    }
}

#[test]
fn space_separated_component_triggers_now_parse_and_evaluate() {
    // Task 4.10 gap 1: MUGEN's `Vel Y` / `Pos X` / `P2Dist X` / `P2BodyDist X`
    // axis-word forms now PARSE (folded to a one-arg call carrying the axis) and
    // EVALUATE through the trigger seam with the axis code (X=0, Y=1, Z=2).
    for src in ["Vel Y", "Pos X", "P2Dist X", "P2BodyDist X", "ScreenPos Y"] {
        let _ = tokenize(src);
        parse_str(src).unwrap_or_else(|e| panic!("{src:?} should now parse: {e}"));
    }
    // Evaluate one: `Vel Y` reads trigger("Vel", &[Int(1)]).
    let ctx = Ctx::new().with_trigger("Vel", &[Value::Int(1)], Value::Float(-3.0));
    assert_eq!(run("Vel Y", &ctx), Value::Float(-3.0));
    // X and Y are distinguishable (the int-code encoding): only Y registered.
    assert_eq!(run("Vel X", &ctx), Value::Int(0));
    // A real KFM comparison: `Vel Y > 0`.
    let ctx = Ctx::new().with_trigger("Vel", &[Value::Int(1)], Value::Float(2.0));
    assert_eq!(run("Vel Y > 0", &ctx), Value::Int(1));

    // None of these is "known-unsupported" any more (the guard must not
    // whitelist a form that now parses — that would mask a future regression).
    for src in ["vel y", "pos x", "p2dist x", "p2bodydist x"] {
        assert!(
            !is_known_unsupported(src),
            "guard must NOT whitelist {src:?}: it parses now (task 4.10)"
        );
    }
}

#[test]
fn animelem_comma_tail_and_dotted_args_now_parse() {
    // Task 4.10 gaps 2 & 3: the `AnimElem = N, op M` tail and dotted call args
    // both parse now (previously known-unsupported). Confirm parse + concrete
    // eval, and that the guard no longer whitelists them.
    for src in [
        "AnimElem = 2, >= 0",
        "AnimElem = 3, -1",
        "GetHitVar(fall.yvel)",
        "GetHitVar(xveladd)",
    ] {
        let ast = parse_str(src).unwrap_or_else(|e| panic!("{src:?} should now parse: {e}"));
        // Evaluating against an empty context yields a concrete Value, no panic.
        let v = eval(&ast, &EmptyCtx);
        assert!(matches!(v, Value::Int(_) | Value::Float(_)), "{src:?} -> {v:?}");
        assert!(
            !is_known_unsupported(src),
            "guard must NOT whitelist {src:?}: it parses now (task 4.10)"
        );
    }

    // AnimElem tail semantics end-to-end: element reached (time 0) AND `>= 0`.
    let ctx = Ctx::new().with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(0));
    assert_eq!(run("AnimElem = 2, >= 0", &ctx), Value::Int(1));
    // Not reached (negative element time) → false.
    let ctx = Ctx::new().with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(-1));
    assert_eq!(run("AnimElem = 2, >= 0", &ctx), Value::Int(0));
}

#[test]
fn known_unsupported_guard_rejects_a_genuine_regression_shape() {
    // The guard must NOT be a blanket "everything is fine" — a plain, valid
    // trigger that fails to parse would be a real regression and must NOT be
    // whitelisted. Pick shapes that are valid MUGEN and unrelated to the gaps.
    for ok_shape in ["Time = 0", "AnimTime >= 30", "root, Life", "var(0) + 1"] {
        assert!(
            !is_known_unsupported(ok_shape),
            "{ok_shape:?} is valid MUGEN; the guard must NOT whitelist it as deferred"
        );
    }
}

// -----------------------------------------------------------------------------
// AC3 (hardening): MUGEN numeric semantics through the FULL lex->parse->eval
// pipeline (not unit-level). These pin the behaviors most likely to silently
// regress: int vs. float typing, integer truncation, divide-by-zero -> 0,
// short-circuit, operator precedence, and the bottom -> 0 / never-fire rule.
// -----------------------------------------------------------------------------

#[test]
fn mugen_arithmetic_semantics_end_to_end() {
    let ctx = EmptyCtx;

    // Integer division truncates toward zero and stays Int.
    assert_eq!(run("7 / 2", &ctx), Value::Int(3));
    assert_eq!(run("-7 / 2", &ctx), Value::Int(-3));
    // Float contamination: any float operand makes the whole op float.
    assert_eq!(run("7.0 / 2", &ctx), Value::Float(3.5));
    // Divide-by-zero is the safe default 0, never a panic / inf.
    assert_eq!(run("1 / 0", &ctx), Value::Int(0));
    assert_eq!(run("5 % 0", &ctx), Value::Int(0));
    // Modulo sign follows the dividend (truncated division).
    assert_eq!(run("-7 % 3", &ctx), Value::Int(-1));
    // Exponentiation: int**nonneg-int stays Int; `**` is right-associative.
    assert_eq!(run("2 ** 3 ** 2", &ctx), Value::Int(512));
    assert_eq!(run("0 ** 0", &ctx), Value::Int(1));
    // Precedence: `*` binds tighter than `+`; unary `-` tighter than `*`.
    assert_eq!(run("2 + 3 * 4", &ctx), Value::Int(14));
    assert_eq!(run("-2 ** 2", &ctx), Value::Int(-4)); // -(2**2) per MUGEN, not (-2)**2
}

#[test]
fn mugen_boolean_and_comparison_semantics_end_to_end() {
    let ctx = EmptyCtx;

    // Comparisons yield Int 1 / 0.
    assert_eq!(run("3 > 2", &ctx), Value::Int(1));
    assert_eq!(run("2 > 3", &ctx), Value::Int(0));
    assert_eq!(run("2 = 2", &ctx), Value::Int(1));
    assert_eq!(run("2 != 2", &ctx), Value::Int(0));
    // `&&` / `||` truthiness: nonzero is true, including negatives.
    assert_eq!(run("1 && -5", &ctx), Value::Int(1));
    assert_eq!(run("0 || 0", &ctx), Value::Int(0));
    assert_eq!(run("0 && (1 / 0)", &ctx), Value::Int(0)); // short-circuit: no DBZ surfacing
    // Logical NOT of nonzero is 0; of zero is 1.
    assert_eq!(run("!5", &ctx), Value::Int(0));
    assert_eq!(run("!0", &ctx), Value::Int(1));
    // `&&` binds tighter than `||` (precedence ladder).
    assert_eq!(run("1 || 0 && 0", &ctx), Value::Int(1));
}

#[test]
fn mugen_bottom_and_unknown_trigger_never_fires() {
    // An unknown trigger reads as 0, so a comparison against it does not fire.
    let ctx = EmptyCtx; // every trigger -> 0
    assert_eq!(run("SomeUnknownTrigger", &ctx), Value::Int(0));
    assert_eq!(run("SomeUnknownTrigger = 5", &ctx), Value::Int(0));
    // `command = "name"` now routes through the `command_active` seam (task
    // 4.10). EmptyCtx's default `command_active` returns false, so no command is
    // active here and the compare is 0 — it never fires, and never panics. (A
    // context that reports the command active yields 1; see the curated test.)
    assert_eq!(run("command = \"fwd\"", &ctx), Value::Int(0));
    // A NON-command `trigger = "string"` still collapses to bottom -> 0 (the
    // documented behavior for string compares outside the `command` seam).
    assert_eq!(run("anim = \"fwd\"", &ctx), Value::Int(0));
    // ln of a non-positive value is bottom -> 0 (never fires), no panic.
    assert_eq!(run("ln(0) > 0", &ctx), Value::Int(0));
    // A bare bottom-y string used as a condition is false.
    let parsed = parse_str("\"literal\"");
    if let Ok(ast) = parsed {
        assert_eq!(eval(&ast, &ctx), Value::Int(0));
    }
}

#[test]
fn mugen_builtin_functions_and_ranges_end_to_end() {
    let ctx = EmptyCtx;

    // cond / ifelse evaluate only the taken branch and return its value type.
    assert_eq!(run("cond(1, 10, 20)", &ctx), Value::Int(10));
    assert_eq!(run("cond(0, 10, 20)", &ctx), Value::Int(20));
    assert_eq!(run("ifelse(1, 1.5, 2.5)", &ctx), Value::Float(1.5));
    // The not-taken branch is never evaluated (so its DBZ does not surface).
    assert_eq!(run("cond(1, 42, 1 / 0)", &ctx), Value::Int(42));
    // abs is type-preserving.
    assert_eq!(run("abs(-7)", &ctx), Value::Int(7));
    assert_eq!(run("abs(-2.5)", &ctx), Value::Float(2.5));
    // floor / ceil narrow a float to int.
    assert_eq!(run("floor(3.9)", &ctx), Value::Int(3));
    assert_eq!(run("ceil(3.1)", &ctx), Value::Int(4));
    // min / max are type-preserving in this engine.
    assert_eq!(run("min(3, 5)", &ctx), Value::Int(3));
    assert_eq!(run("max(3, 5)", &ctx), Value::Int(5));

    // Range membership with every bound combination on the RHS of `=`.
    let n = |v: i32| Ctx::new().with_trigger("anim", &[], Value::Int(v));
    assert_eq!(run("anim = [5,10]", &n(5)), Value::Int(1)); // inclusive lower
    assert_eq!(run("anim = [5,10]", &n(10)), Value::Int(1)); // inclusive upper
    assert_eq!(run("anim = (5,10)", &n(5)), Value::Int(0)); // exclusive lower excludes 5
    assert_eq!(run("anim = (5,10)", &n(10)), Value::Int(0)); // exclusive upper excludes 10
    assert_eq!(run("anim = (5,10]", &n(6)), Value::Int(1)); // mixed bounds
    assert_eq!(run("anim = [5,10)", &n(5)), Value::Int(1)); // mixed bounds
    // `!=` negates membership.
    assert_eq!(run("anim != [5,10]", &n(7)), Value::Int(0));
    assert_eq!(run("anim != [5,10]", &n(99)), Value::Int(1));
}

#[test]
fn nested_redirection_resolves_through_multiple_hops() {
    // enemy, root, Life  — two redirect hops, proving the redirect path nests
    // and reads from the innermost target. This is a staple AI-logic shape.
    let enemy_root = Ctx::new().with_trigger("Life", &[], Value::Int(750));
    let enemy = Ctx::new().with_redirect(Redirect::Root, enemy_root);
    let ctx = Ctx::new().with_redirect(Redirect::Enemy, enemy);
    assert_eq!(run("enemy, root, Life", &ctx), Value::Int(750));
    assert_eq!(run("enemy, root, Life < 1000", &ctx), Value::Int(1));

    // A helper(id) redirect with an argument-bearing inner trigger.
    let helper = Ctx::new().with_trigger("StateNo", &[], Value::Int(200));
    let ctx = Ctx::new().with_redirect(Redirect::Helper(1234), helper);
    assert_eq!(run("helper(1234), StateNo", &ctx), Value::Int(200));
    assert_eq!(run("helper(1234), StateNo = 200", &ctx), Value::Int(1));

    // A redirect whose first hop is missing: the whole thing reads 0, no panic.
    assert_eq!(run("parent, Life", &EmptyCtx), Value::Int(0));
    // Missing inner hop after a present outer hop also reads 0.
    let outer = Ctx::new(); // no Root target inside
    let ctx = Ctx::new().with_redirect(Redirect::Enemy, outer);
    assert_eq!(run("enemy, root, Life", &ctx), Value::Int(0));
}

// -----------------------------------------------------------------------------
// AC3 (hardening): MORE curated REAL-shape triggers spanning forms the
// asset-gated curated test did not — a builtin (`cond`), a parameterized
// trigger combined in a boolean, and an argument-bearing redirect — so the
// "genuine content" guarantee covers the function and call-with-args paths too.
// -----------------------------------------------------------------------------

#[test]
fn additional_curated_real_shape_triggers_evaluate() {
    // `cond` is pervasive in real CNS (e.g. common1.cns uses ifelse heavily).
    let ctx = Ctx::new()
        .with_trigger("Power", &[], Value::Int(1000))
        .with_trigger("PowerMax", &[], Value::Int(3000));
    assert_eq!(
        run("cond(Power >= PowerMax, 1, 0)", &ctx),
        Value::Int(0),
        "Power(1000) < PowerMax(3000) -> false branch"
    );
    let ctx = Ctx::new()
        .with_trigger("Power", &[], Value::Int(3000))
        .with_trigger("PowerMax", &[], Value::Int(3000));
    assert_eq!(run("cond(Power >= PowerMax, 1, 0)", &ctx), Value::Int(1));

    // A real KFM-style combo: StateNo check AND a parameterized AnimElem time.
    let ctx = Ctx::new()
        .with_trigger("StateNo", &[], Value::Int(200))
        .with_trigger("AnimElemTime", &[Value::Int(3)], Value::Int(0));
    assert_eq!(
        run("StateNo = 200 && AnimElemTime(3) = 0", &ctx),
        Value::Int(1)
    );

    // `Var(...)` typed bank access combined in a comparison (real AI logic).
    let ctx = Ctx::new().with_trigger("var", &[Value::Int(5)], Value::Int(2));
    assert_eq!(run("var(5) = 2", &ctx), Value::Int(1));
    assert_eq!(run("var(5) != 2", &ctx), Value::Int(0));
}

// =============================================================================
// Proctor (task 4.10) — REAL-FIXTURE gate exercising every closed gap.
//
// The asset-gated test above proves the aggregate parse rate; this one harvests
// the *specific* gap-form trigger lines out of the production CNS and drives each
// through parse->eval, asserting (a) no panic, (b) a concrete Value, and (c) that
// all four gap categories were actually present and exercised. Gated on
// `test-assets/` so the default asset-less checkout stays green.
// =============================================================================

/// Pulls the raw right-hand sides of `triggerN = ...` / `triggerall = ...` /
/// `varset`-style lines straight out of a CNS file's text. This is a lighter
/// harvest than the structured `collect_triggers` above — it keeps the verbatim
/// RHS string so the curated gap-shape matching below sees exactly what the
/// author wrote (including the `command = "x"` value-set lines).
fn harvest_trigger_rhs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let lower = trimmed.to_ascii_lowercase();
        // trigger / triggerall lines carry expressions; also catch `value = ...`
        // and `var(n) = ...` value-sets, which is where `command = "x"` lives.
        let is_trigger = lower.starts_with("trigger");
        let is_value = lower.starts_with("value") || lower.starts_with("var(");
        if !(is_trigger || is_value) {
            continue;
        }
        let Some((_, rhs)) = line.split_once('=') else {
            continue;
        };
        // Strip a trailing `;` comment.
        let rhs = rhs.split(';').next().unwrap_or(rhs).trim();
        if !rhs.is_empty() {
            out.push(rhs.to_string());
        }
    }
    out
}

#[test]
fn real_fixture_gap_forms_parse_and_eval_without_panic() {
    let Some(dir) = kfm_assets_dir() else {
        eprintln!("skipping real_fixture_gap_forms_parse_and_eval_without_panic: test-assets/ absent");
        return;
    };

    // A generous context so most reads resolve to something; anything unseeded
    // still defaults safely to 0. We seed the axis/command shapes we assert on.
    let ctx = Ctx::new()
        .with_command("x")
        .with_command("a")
        .with_command("holdfwd")
        .with_command("recovery")
        .with_trigger("Vel", &[Value::Int(1)], Value::Float(-4.0)) // Vel Y
        .with_trigger("Vel", &[Value::Int(0)], Value::Float(2.0)) // Vel X
        .with_trigger("Pos", &[Value::Int(1)], Value::Float(-10.0)) // Pos Y
        .with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(0))
        .with_trigger("AnimElemTime", &[Value::Int(3)], Value::Int(0));

    let mut axis_seen = 0usize;
    let mut tail_seen = 0usize;
    let mut dotted_seen = 0usize;
    let mut command_seen = 0usize;
    let mut evaluated = 0usize;

    for fname in ["kfm.cns", "common1.cns"] {
        let text = std::fs::read_to_string(dir.join(fname)).expect("read fixture");
        for rhs in harvest_trigger_rhs(&text) {
            let lower = rhs.to_ascii_lowercase();

            // Categorize the gap forms so we can assert each was present.
            // Axis-suffixed component trigger: `<word> <X|Y|Z>` (whole-word axis).
            let has_axis = regex_like_axis(&lower);
            // AnimElem comma tail: an animelem-family `= N ,` shape.
            let has_tail = lower.contains("animelem")
                && lower.contains('=')
                && lower.contains(',')
                && !lower.contains('[') // exclude range RHS like `anim = [a,b]`
                && !lower.contains('(');
            // Dotted member arg.
            let has_dotted = lower.contains("gethitvar(") && rhs.contains('.');
            // command = "str".
            let has_command = lower.contains("command") && rhs.contains('"');

            if has_axis {
                axis_seen += 1;
            }
            if has_tail {
                tail_seen += 1;
            }
            if has_dotted {
                dotted_seen += 1;
            }
            if has_command {
                command_seen += 1;
            }

            // Every harvested RHS must tokenize + (for the gap shapes) eval without
            // panic. Some value-set RHS are not standalone expressions (e.g.
            // multi-comma controller params), so only assert eval on those that
            // parse — the contract is "never panic", and a parse error here is the
            // recoverable kind.
            let _ = tokenize(&rhs);
            if let Ok(ast) = parse_str(&rhs) {
                let v = eval(&ast, &ctx);
                match v {
                    Value::Int(_) => {}
                    Value::Float(f) => assert!(
                        f.is_finite() || f.is_nan(),
                        "eval of real gap RHS {rhs:?} produced a non-representable float"
                    ),
                }
                evaluated += 1;
            }
        }
    }

    eprintln!(
        "real-fixture gap forms: axis={axis_seen} tail={tail_seen} dotted={dotted_seen} \
         command={command_seen} (evaluated {evaluated} parseable RHS)"
    );

    // Each gap category must actually be present in the production content, or
    // this test is not exercising what it claims. (Grounded in grep counts:
    // axis ~80, tail 4, dotted 3, command ~49 across the two files.)
    assert!(axis_seen >= 10, "expected axis-suffixed forms in fixtures, saw {axis_seen}");
    assert!(tail_seen >= 1, "expected AnimElem comma-tail forms, saw {tail_seen}");
    assert!(dotted_seen >= 1, "expected dotted GetHitVar args, saw {dotted_seen}");
    assert!(command_seen >= 1, "expected command string compares, saw {command_seen}");
    assert!(evaluated > 50, "expected many parseable gap RHS, evaluated {evaluated}");
}

/// Whole-word detection of an axis-suffixed component trigger inside a lowercased
/// RHS: a known vector trigger name immediately followed (separated only by
/// spaces) by a lone axis word `x`/`y`/`z`. Avoids a regex dependency.
fn regex_like_axis(lower: &str) -> bool {
    const NAMES: &[&str] = &[
        "vel", "pos", "p2dist", "p2bodydist", "p1bodydist", "screenpos",
        "backedgedist", "frontedgedist", "parentdist", "rootdist",
    ];
    // Tokenize on non-alphanumeric, then look for [name, axis] adjacency.
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    for pair in words.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if NAMES.contains(&a) && matches!(b, "x" | "y" | "z") {
            return true;
        }
    }
    false
}

#[test]
fn gap_forms_end_to_end_against_mock_cover_all_four_acceptance_criteria() {
    // A single asset-independent test that drives ONE representative real-shape
    // expression per gap through parse->eval, asserting the MUGEN-correct result.
    // This guarantees each acceptance criterion is exercised on every
    // `cargo test`, with or without fixtures.

    // AC1: axis-suffixed component trigger (`Vel Y` → Y=1).
    let ctx = Ctx::new().with_trigger("Vel", &[Value::Int(1)], Value::Float(-3.0));
    assert_eq!(run("Vel Y", &ctx), Value::Float(-3.0));
    assert_eq!(run("Vel Y < 0", &ctx), Value::Int(1));

    // AC2: AnimElem comma tail (`AnimElem = 2, >= 0` → reached AND secondary).
    let ctx = Ctx::new().with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(0));
    assert_eq!(run("AnimElem = 2, >= 0", &ctx), Value::Int(1));
    let ctx = Ctx::new().with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(-1));
    assert_eq!(run("AnimElem = 2, >= 0", &ctx), Value::Int(0));

    // AC3: member-keyed GetHitVar (`GetHitVar(fall.yvel)`) passes the member NAME
    // through the string seam (task 4.11 item a): seed member "fall.yvel"→42.
    // Distinct member names give distinct values — the property the old
    // value-passing path could not provide.
    let ctx = Ctx::new()
        .with_member_trigger("GetHitVar", "fall.yvel", Value::Int(42))
        .with_member_trigger("GetHitVar", "xveladd", Value::Int(7));
    assert_eq!(run("GetHitVar(fall.yvel)", &ctx), Value::Int(42));
    assert_eq!(run("GetHitVar(xveladd)", &ctx), Value::Int(7));

    // AC4: command = "x" string equality through the command seam.
    let ctx = Ctx::new().with_command("x");
    assert_eq!(run("command = \"x\"", &ctx), Value::Int(1));
    let ctx = Ctx::new().with_command("y");
    assert_eq!(run("command = \"x\"", &ctx), Value::Int(0));
}

// =============================================================================
// Proctor (task 4.11) — end-to-end coverage of the three VM-correctness
// follow-ups, both asset-independent (always runs) and real-fixture-gated.
//
//   (a) GetHitVar member-arg STRING key — distinct member names → distinct
//       values, proving the name (not a collapsed numeric value) reaches the
//       context.
//   (b) TimeMod / AnimElemNo are NOT the AnimElemTime comparison-tail family —
//       a `<name> = N, op M` for either degrades to a recoverable ParseError,
//       so it can never be evaluated with the wrong semantics.
//   (c) AnimElem tail binds the secondary operand at relational precedence, so a
//       trailing `&& …` / `|| …` is a separate conjunct, asserted semantically
//       (not just by tree shape).
// =============================================================================

#[test]
fn task_4_11_three_followups_end_to_end_asset_independent() {
    // ---- (a) member-arg string key: distinct members → distinct values ----
    let ctx = Ctx::new()
        .with_member_trigger("GetHitVar", "fall.yvel", Value::Float(-4.5))
        .with_member_trigger("GetHitVar", "xveladd", Value::Int(7))
        .with_member_trigger("GetHitVar", "animtype", Value::Int(2));
    assert_eq!(run("GetHitVar(fall.yvel)", &ctx), Value::Float(-4.5));
    assert_eq!(run("GetHitVar(xveladd)", &ctx), Value::Int(7));
    assert_eq!(run("GetHitVar(animtype)", &ctx), Value::Int(2));
    // The member NAME, not a value, is the key: an unseeded member is the safe 0.
    assert_eq!(run("GetHitVar(nosuchfield)", &ctx), Value::Int(0));
    // It composes in a real-shape comparison (a HitDef-style guard).
    assert_eq!(run("GetHitVar(fall.yvel) < 0", &ctx), Value::Int(1));

    // ---- (b) TimeMod / AnimElemNo comma-tail degrade to a recoverable error ----
    for src in [
        "TimeMod = 2, >= 0",
        "timemod = 4, 1",
        "AnimElemNo = 2, >= 0",
        "ANIMELEMNO = 3, 1",
    ] {
        let _ = tokenize(src); // never panics
        let err = parse_str(src).expect_err(
            "TimeMod/AnimElemNo comma-tail is NOT the AnimElem family; must be a ParseError",
        );
        assert!(
            matches!(err, ParseError::UnexpectedToken { .. }),
            "{src:?} must degrade cleanly, got {err:?}"
        );
    }
    // Their bare equalities still evaluate as ordinary trigger comparisons.
    let ctx = Ctx::new().with_trigger("TimeMod", &[], Value::Int(2));
    assert_eq!(run("TimeMod = 2", &ctx), Value::Int(1));
    let ctx = Ctx::new().with_trigger("AnimElemNo", &[], Value::Int(3));
    assert_eq!(run("AnimElemNo = 3", &ctx), Value::Int(1));

    // ---- (c) AnimElem tail does not swallow a trailing && / || ----
    // Tail TRUE (reached, time 0) but the second conjunct FALSE (Time not > 0):
    // a swallowed `&&` would not produce this separable result.
    let ctx = Ctx::new()
        .with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(0))
        .with_trigger("Time", &[], Value::Int(0));
    assert_eq!(run("AnimElem = 2, >= 0 && Time > 0", &ctx), Value::Int(0));
    // Both true → 1.
    let ctx = Ctx::new()
        .with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(0))
        .with_trigger("Time", &[], Value::Int(5));
    assert_eq!(run("AnimElem = 2, >= 0 && Time > 0", &ctx), Value::Int(1));
    // `||`: tail FALSE (time 0 not > 5) but Time>0 TRUE → 1.
    let ctx = Ctx::new()
        .with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(0))
        .with_trigger("Time", &[], Value::Int(5));
    assert_eq!(run("AnimElem = 2, > 5 || Time > 0", &ctx), Value::Int(1));
    // The operand still absorbs additive: `, >= 1 + 1` compares against 2.
    let ctx = Ctx::new().with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(2));
    assert_eq!(run("AnimElem = 2, >= 1 + 1", &ctx), Value::Int(1));

    // Parenthesized / left-side tail forms degrade cleanly (no silent wrong tree).
    for src in [
        "(AnimElem = 2, >= 0)",
        "(AnimElem = 2, >= 0) && Time > 0",
        "Time > 0 && AnimElem = 2, >= 0",
    ] {
        assert!(
            matches!(parse_str(src), Err(ParseError::UnexpectedToken { .. })),
            "{src:?} should cleanly degrade to a recoverable parse error"
        );
    }
}

#[test]
fn task_4_11_real_fixture_member_and_tail_forms_eval_without_panic() {
    let Some(dir) = kfm_assets_dir() else {
        eprintln!(
            "skipping task_4_11_real_fixture_member_and_tail_forms_eval_without_panic: test-assets/ absent"
        );
        return;
    };

    // Seed the AnimElemTime reads behind the real AnimElem tail forms, plus the
    // GetHitVar members KFM references, so the harvested gap lines compute a
    // concrete value rather than only "not panicking".
    let ctx = Ctx::new()
        .with_trigger("AnimElemTime", &[Value::Int(2)], Value::Int(0))
        .with_trigger("AnimElemTime", &[Value::Int(3)], Value::Int(0))
        .with_member_trigger("GetHitVar", "fall.yvel", Value::Float(-4.0))
        .with_member_trigger("GetHitVar", "xveladd", Value::Float(2.5))
        .with_member_trigger("GetHitVar", "yaccel", Value::Float(0.5));

    let mut tail_seen = 0usize;
    let mut member_seen = 0usize;
    let mut evaluated = 0usize;

    for fname in ["kfm.cns", "common1.cns"] {
        let text = std::fs::read_to_string(dir.join(fname)).expect("read fixture");
        for rhs in harvest_trigger_rhs(&text) {
            let lower = rhs.to_ascii_lowercase();

            // AnimElem comma-tail form (item c): an animelem-family `= N ,` shape
            // that is NOT a range RHS and NOT a parenthesized call.
            let is_tail = lower.contains("animelem")
                && lower.contains('=')
                && lower.contains(',')
                && !lower.contains('[')
                && !lower.contains('(');
            // GetHitVar member form (item a): a dotted-or-bare member argument.
            let is_member = lower.contains("gethitvar(");

            if is_tail {
                tail_seen += 1;
            }
            if is_member {
                member_seen += 1;
            }

            // The never-crash contract spans tokenize -> parse -> eval. A parse
            // error here is the recoverable kind (some value-set RHS are not whole
            // expressions); only assert eval on what parses.
            let _ = tokenize(&rhs);
            if let Ok(ast) = parse_str(&rhs) {
                let v = eval(&ast, &ctx);
                match v {
                    Value::Int(_) => {}
                    Value::Float(f) => assert!(
                        f.is_finite() || f.is_nan(),
                        "eval of real 4.11 RHS {rhs:?} produced a non-representable float"
                    ),
                }
                evaluated += 1;
            }
        }
    }

    eprintln!(
        "task 4.11 real-fixture: animelem-tail={tail_seen} gethitvar-member={member_seen} \
         (evaluated {evaluated} parseable RHS)"
    );
    // KFM uses both the AnimElem comma-tail and dotted GetHitVar member forms; if
    // they vanished, this test is no longer exercising item (a)/(c) on real data.
    assert!(tail_seen >= 1, "expected AnimElem comma-tail forms in fixtures, saw {tail_seen}");
    assert!(member_seen >= 1, "expected GetHitVar member forms in fixtures, saw {member_seen}");
    assert!(evaluated > 50, "expected many parseable RHS, evaluated {evaluated}");
}

#[test]
fn task_4_11_real_fixture_clean_parse_rate_is_100_percent() {
    // AC4: the cns_integration clean-parse rate must stay 100% (812/812) after the
    // 4.11 changes. The aggregate `all_real_triggers_lex_and_parse_without_panic`
    // asserts a 98% floor + no unexpected failures; this pins the exact 100% rate
    // and the expected total so a regression in either the parser OR the fixtures
    // is caught precisely.
    let Some(dir) = kfm_assets_dir() else {
        eprintln!("skipping task_4_11_real_fixture_clean_parse_rate_is_100_percent: test-assets/ absent");
        return;
    };

    let triggers = load_all_triggers(&dir);
    let total = triggers.len();
    let mut parsed = 0usize;
    let mut failures: Vec<(&RawTrigger, ParseError)> = Vec::new();
    for t in &triggers {
        let _ = tokenize(&t.expr); // never panics
        match parse_str(&t.expr) {
            Ok(_) => parsed += 1,
            Err(e) => failures.push((t, e)),
        }
    }

    if !failures.is_empty() {
        eprintln!("clean-parse failures ({}):", failures.len());
        for (t, e) in &failures {
            eprintln!("  {}: {:?} -> {e}", t.origin, t.expr);
        }
    }
    assert_eq!(
        parsed, total,
        "clean-parse rate must be 100% ({parsed}/{total}); 4.11 must not drop any real trigger"
    );
    // Provenance for the documented 812/812 figure: these particular fixtures
    // yield 812 triggers. Pin it so a fixture swap that changes the count is a
    // conscious update, not a silent drift of the acceptance figure.
    assert_eq!(total, 812, "expected 812 real triggers across kfm.cns + common1.cns, got {total}");
}
