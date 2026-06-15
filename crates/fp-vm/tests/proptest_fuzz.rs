//! Property / fuzz tests for the `fp-vm` expression pipeline (audit #37).
//!
//! These tests prove `fp-vm`'s engine-wide **never-panic contract** against
//! randomized and adversarial input, complementing the hand-written
//! literal-array unit tests (which pin specific cases) and the asset-gated
//! real-KFM no-panic test (which is a green no-op on CI when `test-assets/` is
//! absent — see `CLAUDE.md` §"CI caveat #36"). Unlike that real-content test,
//! everything here is fully synthetic, so it **runs on CI** and exercises the
//! never-panic contract on every push.
//!
//! The four stages of the pipeline are covered:
//!
//! - [`tokenize`] — every arbitrary `String` lexes into a `Vec<Token>` without
//!   panicking; bad characters surface as `TokenKind::Unknown`, never an abort.
//! - [`parse`] / [`parse_str`] — every arbitrary string either parses to an
//!   `Expr` or returns a recoverable `ParseError`; it never unwinds.
//! - [`eval`] — every arbitrary `Expr` evaluates to a `Value` against a
//!   deterministic context; divide / modulo by zero, unknown redirects, and
//!   overflow all resolve to the safe default rather than panicking, and the
//!   result is **deterministic** for a fixed context.
//! - The full `lex → parse → eval` chain over randomly *generated valid-ish
//!   `Expr` trees rendered back to source text* is panic-free and deterministic.
//!
//! Case counts are kept CI-reasonable (256–1024 per property) so the suite stays
//! fast while still covering a large adversarial space. proptest seeds each run
//! from OS entropy by default; when a case fails it persists the **failing seed**
//! to `proptest-regressions/` so the exact counterexample replays deterministically
//! on the next run.

use std::collections::HashMap;

use proptest::prelude::*;

use fp_vm::eval::{EvalContext, Redirect, Value};
use fp_vm::evaluator::eval;
use fp_vm::lexer::tokenize;
use fp_vm::parser::{parse, parse_str, BinaryOp, Bound, Expr, UnaryOp};

// =============================================================================
// A deterministic EvalContext for the evaluator properties.
// =============================================================================

/// A fully deterministic in-memory [`EvalContext`] for the fuzz properties.
///
/// Every read is a pure function of the (fixed) lookup tables, so evaluating the
/// same [`Expr`] twice against the same `FuzzCtx` always yields the same
/// [`Value`] — that is what the determinism properties assert. Crucially,
/// [`EvalContext::random`] returns a *fixed* value (not OS randomness), so even
/// the `random` trigger is reproducible here; determinism of the gameplay RNG
/// itself is covered by the `Rng` unit tests in `evaluator.rs`.
#[derive(Clone, Default)]
struct FuzzCtx {
    /// Trigger values keyed by lowercased name (args ignored — a deterministic
    /// stand-in so triggers return varied-but-fixed values).
    triggers: HashMap<String, Value>,
    /// Integer variable bank.
    vars: HashMap<i32, i32>,
    /// Float variable bank.
    fvars: HashMap<i32, f32>,
    /// The `root` redirect target, if any. A leaf context (no further relations)
    /// so a nested redirect eventually hits `None`. Boxed to break the recursive
    /// type and owned by `self` so the `redirect` borrow is sound.
    root: Option<Box<FuzzCtx>>,
}

impl FuzzCtx {
    /// Builds a small, fixed context with a handful of triggers / vars set, so
    /// the evaluator exercises real lookups (hits and misses) rather than always
    /// hitting the default-0 path. It resolves a `root` redirect to a leaf
    /// (default-everything) context so redirected reads are exercised too.
    fn populated() -> Self {
        let mut triggers = HashMap::new();
        triggers.insert("time".to_string(), Value::Int(42));
        triggers.insert("stateno".to_string(), Value::Int(200));
        triggers.insert("life".to_string(), Value::Int(1000));
        triggers.insert("pos".to_string(), Value::Float(3.5));
        triggers.insert("vel".to_string(), Value::Float(-1.25));
        triggers.insert("anim".to_string(), Value::Int(0));
        let mut vars = HashMap::new();
        vars.insert(0, 7);
        vars.insert(1, -3);
        let mut fvars = HashMap::new();
        fvars.insert(0, 2.5);
        Self {
            triggers,
            vars,
            fvars,
            root: Some(Box::new(FuzzCtx::default())),
        }
    }
}

impl EvalContext for FuzzCtx {
    fn trigger(&self, name: &str, _args: &[Value]) -> Value {
        self.triggers
            .get(&name.to_ascii_lowercase())
            .copied()
            .unwrap_or(Value::DEFAULT)
    }

    fn var(&self, index: i32) -> Value {
        self.vars
            .get(&index)
            .copied()
            .map_or(Value::DEFAULT, Value::Int)
    }

    fn fvar(&self, index: i32) -> Value {
        self.fvars
            .get(&index)
            .copied()
            .map_or(Value::DEFAULT, Value::Float)
    }

    fn redirect(&self, target: Redirect) -> Option<&dyn EvalContext> {
        // Only `root` resolves, and only to a fixed leaf context with no further
        // relations — so a nested redirect (`root, enemy, x`) eventually hits
        // `None` and the evaluator must treat it as the safe default. Every other
        // target is `None`, exercising the unresolved-redirect → 0 path.
        match target {
            Redirect::Root => self.root.as_deref().map(|c| c as &dyn EvalContext),
            _ => None,
        }
    }

    fn random(&self) -> i32 {
        // A FIXED value (not OS randomness) so evaluation stays deterministic.
        12345
    }
}

/// Compares two evaluation results for **bit-identical determinism**.
///
/// `Value` derives `PartialEq`, which compares floats with IEEE-754 semantics
/// where `NaN != NaN`. Arithmetic no longer synthesizes a NaN (audit #19 funnels
/// a non-finite `+ - *` result to bottom → `0`), but a `Value::Float(NaN)` can
/// still enter evaluation deterministically through a trigger / `fvar` lookup,
/// and a plain `a == b` would then spuriously report "non-determinism" for two
/// bit-identical NaN results. This helper instead compares the *bit patterns* of
/// float values (so two NaNs of the same bit pattern are equal), which is the
/// correct notion of "the evaluator returned the same thing both times".
fn same_value(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        _ => false,
    }
}

// =============================================================================
// Strategies.
// =============================================================================

/// A strategy producing a redirection target — covers every [`Redirect`] variant
/// (including ones the `FuzzCtx` never resolves, to exercise the `None` path).
fn redirect_strategy() -> impl Strategy<Value = Redirect> {
    prop_oneof![
        Just(Redirect::Parent),
        Just(Redirect::Root),
        any::<i32>().prop_map(Redirect::Helper),
        proptest::option::of(any::<i32>()).prop_map(Redirect::Target),
        Just(Redirect::Enemy),
        any::<i32>().prop_map(Redirect::EnemyNear),
        Just(Redirect::Partner),
        any::<i32>().prop_map(Redirect::PlayerId),
    ]
}

/// A strategy producing identifier names drawn from a mix of real MUGEN trigger
/// names, variable-bank names, function names, and arbitrary short identifiers,
/// so the generated trees exercise both recognized and unrecognized lookups.
fn ident_name_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        // Recognized triggers / vars / member-keyed / random.
        Just("Time".to_string()),
        Just("StateNo".to_string()),
        Just("Life".to_string()),
        Just("Pos".to_string()),
        Just("Vel".to_string()),
        Just("random".to_string()),
        Just("AnimElem".to_string()),
        // Arbitrary identifier-shaped names (always lex as a single Ident).
        "[A-Za-z_][A-Za-z0-9_]{0,7}",
    ]
}

/// A recursive strategy that builds random *valid* [`Expr`] trees spanning every
/// variant: literals, identifiers, unary / binary ops, calls (including the
/// member-keyed and known-function forms), ranges, redirections, and the
/// `AnimElem` comparison tail.
///
/// Depth and breadth are bounded so trees stay small enough to evaluate cheaply
/// (recursion depth ≤ 4, ≤ 6 nodes per collection). Because the trees are built
/// directly as `Expr`, they bypass the parser — that is intentional: the
/// evaluator must tolerate *any* `Expr` shape (even ones the parser would never
/// produce, e.g. a bare `Range` or a `Str`), all of which must still resolve to a
/// `Value` without panicking.
fn expr_strategy() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        any::<i64>().prop_map(Expr::Int),
        any::<f64>().prop_map(Expr::Float),
        "[A-Za-z0-9_. ]{0,8}".prop_map(Expr::Str),
        ident_name_strategy().prop_map(Expr::Ident),
    ];

    leaf.prop_recursive(
        4,  // max recursion depth
        64, // target max total nodes
        6,  // max items per collection (call args)
        |inner| {
            prop_oneof![
                // Unary.
                (unary_op_strategy(), inner.clone()).prop_map(|(op, operand)| Expr::Unary {
                    op,
                    operand: Box::new(operand),
                }),
                // Binary.
                (binary_op_strategy(), inner.clone(), inner.clone()).prop_map(|(op, lhs, rhs)| {
                    Expr::Binary {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    }
                }),
                // Call: a mix of known function names and arbitrary trigger names.
                (
                    call_name_strategy(),
                    prop::collection::vec(inner.clone(), 0..4)
                )
                    .prop_map(|(name, args)| Expr::Call { name, args }),
                // Member-keyed call (GetHitVar / const) with a bare-ident member.
                ("(?:GetHitVar|const)", "[A-Za-z_][A-Za-z0-9_.]{0,10}").prop_map(
                    |(name, member)| Expr::Call {
                        name,
                        args: vec![Expr::Ident(member)],
                    }
                ),
                // Range.
                (
                    bound_strategy(),
                    inner.clone(),
                    inner.clone(),
                    bound_strategy()
                )
                    .prop_map(|(lb, lo, hi, ub)| Expr::Range {
                        lower_bound: lb,
                        lower: Box::new(lo),
                        upper: Box::new(hi),
                        upper_bound: ub,
                    }),
                // Redirection.
                (redirect_strategy(), inner.clone()).prop_map(|(target, e)| Expr::Redirected {
                    target,
                    expr: Box::new(e),
                }),
                // AnimElem comparison tail.
                (binary_op_strategy(), inner.clone(), inner.clone()).prop_map(
                    |(op, element, operand)| Expr::AnimElemTail {
                        name: "AnimElem".to_string(),
                        element: Box::new(element),
                        op,
                        operand: Box::new(operand),
                    }
                ),
            ]
        },
    )
}

/// All [`UnaryOp`] variants.
fn unary_op_strategy() -> impl Strategy<Value = UnaryOp> {
    prop_oneof![
        Just(UnaryOp::Not),
        Just(UnaryOp::Neg),
        Just(UnaryOp::BitNot)
    ]
}

/// All [`BinaryOp`] variants — covers logical, bitwise, relational, arithmetic,
/// and `**` (which has the saturating-overflow path).
fn binary_op_strategy() -> impl Strategy<Value = BinaryOp> {
    prop_oneof![
        Just(BinaryOp::Or),
        Just(BinaryOp::And),
        Just(BinaryOp::BitOr),
        Just(BinaryOp::BitXor),
        Just(BinaryOp::BitAnd),
        Just(BinaryOp::Eq),
        Just(BinaryOp::Ne),
        Just(BinaryOp::Lt),
        Just(BinaryOp::Le),
        Just(BinaryOp::Gt),
        Just(BinaryOp::Ge),
        Just(BinaryOp::Add),
        Just(BinaryOp::Sub),
        Just(BinaryOp::Mul),
        Just(BinaryOp::Div),
        Just(BinaryOp::Mod),
        Just(BinaryOp::Pow),
    ]
}

/// Both [`Bound`] kinds.
fn bound_strategy() -> impl Strategy<Value = Bound> {
    prop_oneof![Just(Bound::Inclusive), Just(Bound::Exclusive)]
}

/// Call names: the known evaluator functions (which take the typed / lazy paths)
/// plus arbitrary trigger-shaped names (which fall through to the numeric path).
fn call_name_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("cond".to_string()),
        Just("ifelse".to_string()),
        Just("abs".to_string()),
        Just("floor".to_string()),
        Just("ceil".to_string()),
        Just("min".to_string()),
        Just("max".to_string()),
        Just("sin".to_string()),
        Just("cos".to_string()),
        Just("atan".to_string()),
        Just("exp".to_string()),
        Just("ln".to_string()),
        Just("var".to_string()),
        Just("fvar".to_string()),
        Just("sysvar".to_string()),
        Just("random".to_string()),
        "[A-Za-z_][A-Za-z0-9_]{0,7}",
    ]
}

/// Renders an [`Expr`] back to MUGEN source text, so a generated tree can be fed
/// through `tokenize → parse` to exercise the full front-end on structured (but
/// adversarial) input. The rendering is intentionally fully parenthesized so it
/// round-trips through the precedence-climbing parser regardless of the tree's
/// shape; exact source-string equality is *not* required — only that the chain
/// never panics.
fn render(expr: &Expr) -> String {
    match expr {
        Expr::Int(n) => format!("({n})"),
        // Use a precise rendering so it re-lexes as a float.
        Expr::Float(f) => {
            if f.is_finite() {
                format!("({f:?})")
            } else {
                // Non-finite floats have no literal form; substitute 0.
                "(0.0)".to_string()
            }
        }
        Expr::Str(s) => {
            // Strip embedded quotes/backslashes so the string literal stays well
            // formed; the content is irrelevant to the never-panic property.
            let cleaned: String = s.chars().filter(|c| *c != '"' && *c != '\\').collect();
            format!("\"{cleaned}\"")
        }
        Expr::Ident(name) => format!("({name})"),
        Expr::Unary { op, operand } => format!("({op}{})", render(operand)),
        Expr::Binary { op, lhs, rhs } => {
            format!("({} {op} {})", render(lhs), render(rhs))
        }
        Expr::Call { name, args } => {
            let rendered: Vec<String> = args.iter().map(render).collect();
            format!("{name}({})", rendered.join(", "))
        }
        Expr::Range {
            lower_bound,
            lower,
            upper,
            upper_bound,
        } => {
            let lc = match lower_bound {
                Bound::Inclusive => '[',
                Bound::Exclusive => '(',
            };
            let rc = match upper_bound {
                Bound::Inclusive => ']',
                Bound::Exclusive => ')',
            };
            format!("{lc}{}, {}{rc}", render(lower), render(upper))
        }
        Expr::Redirected { target, expr } => format!("{target}, {}", render(expr)),
        Expr::AnimElemTail {
            name,
            element,
            op,
            operand,
        } => {
            format!("{name} = {}, {op} {}", render(element), render(operand))
        }
        Expr::TimeModTail { divisor, remainder } => {
            format!("TimeMod = {}, {}", render(divisor), render(remainder))
        }
        Expr::HitDefAttrTail {
            standtype,
            attr_codes,
        } => {
            format!("HitDefAttr = {standtype}, {}", attr_codes.join(", "))
        }
        Expr::ProjTail {
            name,
            value,
            op,
            time,
        } => {
            format!("{name} = {}, {op} {}", render(value), render(time))
        }
    }
}

// =============================================================================
// Properties.
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig {
        // CI-reasonable case count; a failing seed is persisted to
        // proptest-regressions/ so any failure replays deterministically.
        cases: 1024,
        ..ProptestConfig::default()
    })]

    /// `tokenize` never panics on an arbitrary string and always returns a `Vec`.
    /// Any byte that is not part of the grammar must surface as a
    /// `TokenKind::Unknown` token rather than aborting tokenization.
    #[test]
    fn tokenize_never_panics(input in ".*") {
        let tokens = tokenize(&input);
        // The result is a Vec (trivially true by type); assert the lexer made
        // progress in a bounded way — never more tokens than input chars (each
        // token consumes at least one char; comments/whitespace consume without
        // emitting).
        prop_assert!(tokens.len() <= input.chars().count());
    }

    /// `tokenize` is deterministic: the same input lexes identically every time.
    #[test]
    fn tokenize_is_deterministic(input in ".*") {
        prop_assert_eq!(tokenize(&input), tokenize(&input));
    }

    /// Tokenizing then re-rendering each token via `Display` and re-tokenizing is
    /// panic-free — exercises the `TokenKind::Display` impl on fuzzed tokens.
    #[test]
    fn token_display_never_panics(input in ".*") {
        for tok in tokenize(&input) {
            // Must not panic; the textual form is used in parser diagnostics.
            let _ = tok.kind.to_string();
        }
    }

    /// `parse_str` never panics on an arbitrary string: it returns either
    /// `Ok(Expr)` or a recoverable `ParseError`, never an unwind.
    #[test]
    fn parse_str_never_panics(input in ".*") {
        // The act of calling is the assertion: a panic would fail the test. We
        // additionally confirm the two-step `tokenize`→`parse` path agrees with
        // the one-step `parse_str` convenience wrapper.
        let one_step = parse_str(&input);
        let two_step = parse(&tokenize(&input));
        prop_assert_eq!(one_step.is_ok(), two_step.is_ok());
    }

    /// `parse_str` is deterministic for a fixed input.
    #[test]
    fn parse_str_is_deterministic(input in ".*") {
        prop_assert_eq!(parse_str(&input), parse_str(&input));
    }

    /// Parsing adversarial strings built from grammar fragments (operators,
    /// delimiters, numbers, idents, redirect keywords) never panics. This biases
    /// the random input toward *structurally plausible* expressions, hitting more
    /// of the parser's recovery paths than purely random bytes would.
    #[test]
    fn parse_grammar_fragments_never_panics(
        frags in prop::collection::vec(
            prop_oneof![
                Just("+"), Just("-"), Just("*"), Just("/"), Just("%"), Just("**"),
                Just("&&"), Just("||"), Just("!"), Just("&"), Just("|"), Just("^"),
                Just("~"), Just("="), Just("=="), Just("!="), Just("<"), Just("<="),
                Just(">"), Just(">="), Just(":="), Just("("), Just(")"), Just("["),
                Just("]"), Just(","), Just("1"), Just("2.5"), Just("Time"),
                Just("var"), Just("enemy"), Just("root"), Just("helper"),
                Just("cond"), Just("\"x\""), Just("AnimElem"), Just("@"), Just(":"),
            ],
            0..24,
        ),
    ) {
        let src = frags.join(" ");
        // Either Ok or a recoverable error — the call itself must not panic.
        let _ = parse_str(&src);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        ..ProptestConfig::default()
    })]

    /// `eval` never panics on an arbitrary `Expr` and always returns a `Value`.
    /// Generated trees include divide/modulo-by-zero, unresolved redirects, and
    /// overflowing arithmetic — every one must resolve to a `Value`
    /// (the safe-default `0` on the error paths) rather than panicking.
    #[test]
    fn eval_never_panics(expr in expr_strategy()) {
        let ctx = FuzzCtx::populated();
        // The real assertion is that this call *returns* at all: any panic
        // (unwind / abort) inside `eval` fails the proptest. `Value` has only the
        // `Int`/`Float` variants by construction — the internal `Bottom` sentinel
        // is collapsed to `Int(0)` before it can reach the public boundary — so
        // simply binding the result here is what proves the never-panic contract;
        // there is no third variant a runtime check could catch.
        let _v = eval(&expr, &ctx);
    }

    /// `eval` is deterministic: the same `Expr` against the same (fixed) context
    /// yields the same `Value` every time. This pins down that nothing in the
    /// evaluator (RNG seam, lookups, ordering) introduces hidden nondeterminism.
    #[test]
    fn eval_is_deterministic(expr in expr_strategy()) {
        let ctx = FuzzCtx::populated();
        let a = eval(&expr, &ctx);
        let b = eval(&expr, &ctx);
        // Bit-identical comparison: two deterministic NaN results are "the same"
        // even though `NaN != NaN` under `Value`'s derived `PartialEq`.
        prop_assert!(same_value(a, b), "non-deterministic eval: {a:?} vs {b:?} for {expr:?}");
    }

    /// The safety contract that protects downstream consumers holds for **every**
    /// evaluated `Value`: its `as_bool()` and `to_int()` coercions are total and
    /// honor the "an erroring expression never fires a trigger" rule —
    /// specifically, any NaN result would read as **false** (never fires) and
    /// narrow to `0`, never to garbage, and neither coercion panics.
    ///
    /// As of audit #19 the evaluator also funnels a non-finite float result from
    /// the generic `+ - *` path to bottom (collapsing to `0`), matching `**` /
    /// `ln` / the transcendentals, so `eval` no longer returns a public
    /// `Value::Float(NaN)` — see `nan_leak_from_float_multiply_is_closed` for the
    /// regression pin. The NaN-guard assertions below are retained as
    /// defense-in-depth: they must hold for *any* float a `Value` could carry
    /// (e.g. one sourced directly from a trigger / `fvar` lookup), independent of
    /// whether arithmetic can still synthesize one.
    #[test]
    fn eval_coercions_are_total_and_nan_safe(expr in expr_strategy()) {
        let ctx = FuzzCtx::populated();
        let v = eval(&expr, &ctx);
        if let Value::Float(f) = v {
            if f.is_nan() {
                // A NaN must never read as truthy and must narrow to 0.
                prop_assert!(!v.as_bool(), "NaN read as true for {:?}", expr);
                prop_assert_eq!(v.to_int(), 0, "NaN narrowed to non-zero for {:?}", expr);
            }
        }
        // Coercions are total (the calls themselves are the assertion — a panic
        // would fail the test) and `to_int` is always a finite i32.
        let _ = v.as_bool();
        let _ = v.to_int();
    }

    /// The **full front-end + evaluator chain** over a randomly generated tree
    /// rendered back to source text is panic-free and deterministic: render the
    /// `Expr`, `tokenize` it, `parse` it, and `eval` the result twice — the two
    /// evaluations must agree. This is the end-to-end `lex → parse → eval` panic /
    /// determinism property the audit asks for.
    #[test]
    fn lex_parse_eval_chain_is_panic_free_and_deterministic(expr in expr_strategy()) {
        let ctx = FuzzCtx::populated();
        let src = render(&expr);

        // Stage 1 + 2: tokenize and parse the rendered source. Either outcome is
        // acceptable (a recoverable error is fine); only a panic fails the test.
        let parsed = parse_str(&src);
        if let Ok(reparsed) = parsed {
            // Stage 3: evaluate the *re-parsed* tree twice; results must match
            // (bit-identical, so a deterministic NaN counts as equal).
            let a = eval(&reparsed, &ctx);
            let b = eval(&reparsed, &ctx);
            prop_assert!(same_value(a, b), "non-deterministic eval of reparsed {src:?}");
            prop_assert!(a.is_int() || a.is_float());
        }

        // Independently, the original generated tree must also evaluate
        // deterministically (covers Expr shapes the parser would never produce).
        let direct_a = eval(&expr, &ctx);
        let direct_b = eval(&expr, &ctx);
        prop_assert!(same_value(direct_a, direct_b), "non-deterministic eval for {expr:?}");
    }
}

// =============================================================================
// A couple of fixed adversarial regressions (not proptest) pinning the exact
// error-path values the properties only assert "is some Value" about.
// =============================================================================

#[test]
fn div_and_mod_by_zero_are_zero() {
    let ctx = FuzzCtx::populated();
    assert_eq!(eval(&parse_str("1 / 0").unwrap(), &ctx), Value::Int(0));
    assert_eq!(eval(&parse_str("5 % 0").unwrap(), &ctx), Value::Int(0));
}

#[test]
fn unresolved_redirect_is_zero() {
    let ctx = FuzzCtx::populated();
    // `parent` never resolves in FuzzCtx → the whole redirected read is 0.
    assert_eq!(
        eval(&parse_str("parent, Life").unwrap(), &ctx),
        Value::Int(0)
    );
}

#[test]
fn unknown_token_is_recoverable_error_not_panic() {
    // A lone `@` lexes to Unknown and the parser rejects it as a recoverable
    // error — never a panic.
    assert!(parse_str("1 @ 2").is_err());
}

#[test]
fn nan_leak_from_float_multiply_is_closed() {
    // Regression for audit #19: the generic `+ - *` float path now funnels a
    // non-finite (`NaN` / `±inf`) result to the internal bottom sentinel, exactly
    // like the `**`, `ln`, and transcendental paths. Previously this expression
    // leaked a public `Value::Float(NaN)` (an unknown trigger `0` times a float
    // literal that overflows f32 to `-inf` → `0 * -inf == NaN`); now the bottom
    // collapses to the safe default `Value::Int(0)` at the public boundary, so no
    // public NaN escapes `eval`.
    let ctx = FuzzCtx::populated();
    // `A` is an unknown trigger → Int(0); the literal overflows f32 to -inf, so
    // the multiply is non-finite and funnels to bottom → 0.
    let v = eval(&parse_str("A * -5.8751624776690396e128").unwrap(), &ctx);
    assert_eq!(
        v,
        Value::Int(0),
        "the NaN leak must now collapse to 0, got {v:?}"
    );
    // No public NaN survived: the result is a plain finite integer.
    assert!(
        !matches!(v, Value::Float(f) if f.is_nan()),
        "no public NaN may escape eval"
    );
}
