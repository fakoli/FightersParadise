//! # Tree-walk expression evaluator
//!
//! Stage 4 of the `fp-vm` pipeline (task 4.4): walks the [`Expr`] AST produced by
//! the [parser](crate::parser) against an [`EvalContext`] and produces a runtime
//! [`Value`], with faithful MUGEN numeric semantics per
//! [`docs/knowledge-base/07-evaluator-semantics.md`][kb].
//!
//! The single public entry point is [`eval`]:
//!
//! ```
//! use fp_vm::eval::{EvalContext, Value};
//! use fp_vm::evaluator::eval;
//! use fp_vm::parser::parse_str;
//!
//! struct AnimCtx;
//! impl EvalContext for AnimCtx {
//!     fn trigger(&self, name: &str, _args: &[Value]) -> Value {
//!         match name.to_ascii_lowercase().as_str() {
//!             "animelem" => Value::Int(6),         // AnimElem == 6
//!             "p2bodydistx" => Value::Float(12.0), // distance to P2 == 12
//!             _ => Value::DEFAULT,
//!         }
//!     }
//! }
//!
//! // A real trigger expression: `AnimElem = 6 && P2BodyDistX < 30`. (The
//! // axis-suffixed `P2BodyDist X` redirect form is deferred to a later task, so
//! // it is modeled here as a single trigger name.)
//! let ast = parse_str("AnimElem = 6 && P2BodyDistX < 30").unwrap();
//! assert_eq!(eval(&ast, &AnimCtx), Value::Int(1));
//! ```
//!
//! ## Numeric semantics (the short version)
//!
//! - **Types** — [`Value::Int`] (`i32`) and [`Value::Float`] (`f32`). Any float
//!   operand promotes the result to float (§1).
//! - **`/`** — truncating-toward-zero integer division when *both* operands are
//!   int, float division otherwise; divide-by-zero → `0` (§2).
//! - **`%`** — int-only, sign of the dividend; float operand or `%0` → `0` (§2).
//! - **`**`** — right-associative; `int ** nonneg-int` stays int and **saturates
//!   to [`i32::MAX`]** on overflow; any float operand or a negative exponent uses
//!   `f32` `powf`; an invalid result (`NaN`) → `0` (§3).
//! - **`+ - *`** on two ints use **wrapping** native `i32` arithmetic (§4).
//! - **Comparisons** return int `1`/`0`, left-associative; **`&&` / `||`
//!   short-circuit**; truthiness is "nonzero / non-`0.0`" (`NaN` is false) (§5).
//! - **Bitwise `& | ^ ~`** operate on ints (a float operand narrows via the
//!   saturating [`Value::to_int`]) (§9).
//! - **Ranges** — `x = [a,b]` / `(a,b)` / `[a,b)` / `(a,b]` with the documented
//!   inclusive/exclusive bounds; `x != range` negates membership (§6).
//!
//! ## The "bottom" sentinel
//!
//! The spec's error value *bottom* is modeled **internally** as a private
//! `Bottom` value variant so that an error (divide-by-zero, `ln(0)`, an invalid
//! exponentiation, …) can propagate through an expression exactly as MUGEN's
//! bottom does. At the public boundary [`eval`] funnels `Bottom` to the safe
//! default `Value::Int(0)`, matching the module-wide "an erroring expression
//! never fires a trigger" / "bad expression → 0" contract. `Bottom` is never
//! exposed in the public API.
//!
//! ## Randomness & determinism
//!
//! The `random` trigger uses the Park–Miller "minimal standard" LCG (§11), not
//! OS randomness, so replays and netplay rollback stay bit-deterministic. The
//! LCG itself is the standalone, seedable [`Rng`]; the evaluator reads a random
//! value through the [`EvalContext::random`] seam so the RNG **state lives with
//! the game entity** (and is therefore part of saved / rolled-back state). The
//! default [`EvalContext::random`] returns `0` — a concrete entity overrides it
//! to advance its own [`Rng`]. See [`Rng`] and [`EvalContext::random`].
//!
//! ## Redirection
//!
//! Redirection (`enemy, expr` / `root, var(0)` / `helper(1234), stateno`) is
//! represented by [`Expr::Redirected`] (task 4.8). The evaluator resolves the
//! [`Redirect`] via [`EvalContext::redirect`] and evaluates the sub-expression
//! against the target context; a missing target (`None`) is treated as bottom →
//! `0`, so an unresolved redirect never fires a trigger.
//!
//! ## Real-content trigger forms (task 4.10)
//!
//! Four extra real-content forms found in `kfm.cns` are supported:
//!
//! - **Axis-suffixed component triggers** — `Vel Y`, `Pos X`, `P2BodyDist X`,
//!   `ScreenPos Y`, … parse (in the [parser](crate::parser)) to a one-argument
//!   [`Expr::Call`] whose argument is the axis. The
//!   evaluator passes the axis to [`EvalContext::trigger`] as the small int code
//!   `X=0` / `Y=1` / `Z=2` (see `axis_arg_code`).
//! - **`AnimElem = N, op M`** — the two-parameter element-time comparison
//!   ([`Expr::AnimElemTail`]) evaluates to
//!   "element `N` reached **and** `AnimElemTime(N) op M`".
//! - **Member-keyed triggers** — `GetHitVar(member)` selects a hit field *by
//!   name* (`GetHitVar(fall.yvel)`, `GetHitVar(xveladd)`) and `const(member)`
//!   reads a character's authored constant *by name*
//!   (`const(velocity.walk.fwd.x)`, `const(movement.yaccel)`, task 5.6d). The
//!   member is a bare identifier (a dotted member like `fall.yvel` lexes as one
//!   identifier); the evaluator passes that name verbatim through
//!   [`EvalContext::trigger_str`] rather than collapsing it to a number, so the
//!   context identifies which field was requested (task 4.11, item a). The full
//!   member-keyed set is documented on the private `MEMBER_KEYED_TRIGGERS`
//!   constant (`GetHitVar`, `const`).
//! - **`command = "name"`** — string-equality routes through
//!   [`EvalContext::command_active`] (a boolean string-keyed seam) instead of a
//!   numeric read, so the comparison actually fires when the command is active.
//!   Other `trigger = "string"` comparisons (no `command` operand) keep the
//!   pre-4.10 behavior: the `Str` operand is bottom, so the comparison is `0`
//!   (never fires) — documented and never a panic.
//!
//! ## Not yet in scope
//!
//! The `:=` assignment operator is **not** represented in the current [`Expr`]
//! AST (deferred to task 4.9).
//!
//! [kb]: ../../../docs/knowledge-base/07-evaluator-semantics.md

use crate::eval::{EvalContext, Redirect, Value};
use crate::parser::{BinaryOp, Bound, Expr, UnaryOp};

/// The Park–Miller "minimal standard" multiplier (`7^5`).
const PARK_MILLER_MUL: i64 = 16807;
/// The Park–Miller modulus (`2^31 - 1`, a Mersenne prime); also [`i32::MAX`].
const PARK_MILLER_MOD: i64 = 2_147_483_647;

/// A deterministic Park–Miller "minimal standard" linear congruential generator.
///
/// This is the exact RNG MUGEN / Ikemen GO use for the `random` trigger
/// ([`07-evaluator-semantics.md`][kb] §11): `seed = (seed * 16807) mod (2^31-1)`.
/// It is reproduced here so that replays and frame-perfect netplay rollback stay
/// **bit-deterministic** across runs and machines — the `seed` is intended to be
/// part of saved / rolled-back game state, advanced purely deterministically
/// after an initial match-start seed.
///
/// The evaluator does not own an `Rng`; it reads random values through the
/// [`EvalContext::random`] seam so the state can live with the game entity. This
/// type is the building block a concrete entity uses to implement that seam.
///
/// # Examples
///
/// ```
/// use fp_vm::evaluator::Rng;
///
/// // Same seed ⇒ same sequence (determinism).
/// let mut a = Rng::new(1);
/// let mut b = Rng::new(1);
/// assert_eq!(a.next_range(0, 999), b.next_range(0, 999));
///
/// // Every draw is within the inclusive bounds.
/// let mut r = Rng::new(42);
/// for _ in 0..1000 {
///     let v = r.next_range(0, 999);
///     assert!((0..=999).contains(&v));
/// }
/// ```
///
/// [kb]: ../../../docs/knowledge-base/07-evaluator-semantics.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rng {
    /// The current LCG state, kept strictly inside `1..=2^31-2`.
    seed: i32,
}

impl Rng {
    /// Creates a generator from a seed.
    ///
    /// The seed is normalized into the generator's valid range `1..=2^31-2`
    /// (Park–Miller is undefined at `0`), so any `i32` is accepted, including `0`
    /// and negative values. Use the *same* seed on both peers (persist it for
    /// replays) to get an identical sequence.
    #[must_use]
    pub fn new(seed: i32) -> Self {
        let mut s = (seed as i64).rem_euclid(PARK_MILLER_MOD) as i32;
        if s == 0 {
            s = 1;
        }
        Rng { seed: s }
    }

    /// Returns the current internal seed (for persisting / rolling back state).
    #[must_use]
    pub fn seed(self) -> i32 {
        self.seed
    }

    /// Advances the generator and returns the next raw value in `1..=2^31-2`.
    ///
    /// This is `seed = (seed * 16807) mod (2^31 - 1)`, computed in `i64` to avoid
    /// overflow before the modulo, exactly as the minimal-standard generator
    /// specifies.
    pub fn next_u31(&mut self) -> i32 {
        // i32 * 16807 can exceed i32 range, so widen to i64 first.
        let next = (self.seed as i64).wrapping_mul(PARK_MILLER_MUL) % PARK_MILLER_MOD;
        // The modulus keeps `next` in 0..=2^31-2; it is never 0 for a valid seed
        // (the modulus is prime and the multiplier is coprime to it), but guard
        // anyway so the state can never collapse to the degenerate 0.
        self.seed = if next == 0 { 1 } else { next as i32 };
        self.seed
    }

    /// Returns the next value as an **inclusive** integer in `[lo, hi]`.
    ///
    /// Matches MUGEN's `random` (classic: `[0, 999]`) and Ikemen's `RandI`
    /// inclusive-range behavior. If `lo > hi` the bounds are swapped, so the
    /// result is always a valid member of the (possibly reordered) range and the
    /// call never panics. An empty span (`lo == hi`) returns that single value.
    pub fn next_range(&mut self, lo: i32, hi: i32) -> i32 {
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
        // Span as i64 to avoid overflow for extreme bounds (e.g. MIN..=MAX).
        let span = (hi as i64) - (lo as i64) + 1;
        let r = self.next_u31() as i64; // 1..=2^31-2, always non-negative
        (lo as i64 + r.rem_euclid(span)) as i32
    }
}

/// The internal result of evaluating a sub-expression.
///
/// Like the public [`Value`] this is `Int` / `Float`, plus the spec's error
/// sentinel [`Eval::Bottom`] (§1) which a public [`Value`] deliberately does not
/// model. Keeping `Bottom` *internal* lets errors propagate through an expression
/// the way MUGEN's bottom does, while [`eval`] still hands the caller a plain
/// `Value` (with `Bottom` collapsed to the safe default `Int(0)`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Eval {
    /// A 32-bit signed integer.
    Int(i32),
    /// A single-precision float.
    Float(f32),
    /// The error sentinel ("bottom"): produced by illegal operations and
    /// propagated; collapses to `Int(0)` at the public boundary.
    Bottom,
}

impl Eval {
    /// Wraps a public [`Value`] as an [`Eval`] (never `Bottom`).
    fn from_value(v: Value) -> Self {
        match v {
            Value::Int(i) => Eval::Int(i),
            Value::Float(f) => Eval::Float(f),
        }
    }

    /// Collapses to a public [`Value`]: `Bottom` becomes the safe default `0`.
    fn into_value(self) -> Value {
        match self {
            Eval::Int(i) => Value::Int(i),
            Eval::Float(f) => Value::Float(f),
            Eval::Bottom => Value::DEFAULT,
        }
    }

    /// Truthiness by MUGEN's "nonzero is true" rule; `Bottom` is false.
    fn as_bool(self) -> bool {
        match self {
            Eval::Int(i) => i != 0,
            Eval::Float(f) => f != 0.0 && !f.is_nan(),
            Eval::Bottom => false,
        }
    }

    /// Widens to `f32` for the float path; `Bottom` widens to `NaN` so it keeps
    /// propagating as an error through float arithmetic.
    fn to_float(self) -> f32 {
        match self {
            Eval::Int(i) => i as f32,
            Eval::Float(f) => f,
            Eval::Bottom => f32::NAN,
        }
    }

    /// Narrows to `i32` with the saturating CB4 rule; `Bottom` narrows to `0`.
    fn to_int(self) -> i32 {
        match self {
            Eval::Int(i) => i,
            // Rust's float→int `as` cast saturates to MIN/MAX and maps NaN to 0
            // (stable since 1.45) — exactly the CB4 narrowing contract (§4).
            Eval::Float(f) => f as i32,
            Eval::Bottom => 0,
        }
    }

    /// True if this is the float variant (used for the promotion decision).
    fn is_float(self) -> bool {
        matches!(self, Eval::Float(_))
    }

    /// True if either operand is `Bottom` (errors propagate).
    fn either_bottom(a: Eval, b: Eval) -> bool {
        matches!(a, Eval::Bottom) || matches!(b, Eval::Bottom)
    }
}

/// Saturates an `i64` literal magnitude into `i32` (CB4: out-of-range clamps to
/// [`i32::MIN`] / [`i32::MAX`] rather than wrapping or zeroing).
fn saturate_i64_to_i32(n: i64) -> i32 {
    if n > i32::MAX as i64 {
        tracing::warn!(value = n, "integer literal exceeds i32::MAX; saturating");
        i32::MAX
    } else if n < i32::MIN as i64 {
        tracing::warn!(value = n, "integer literal below i32::MIN; saturating");
        i32::MIN
    } else {
        n as i32
    }
}

/// Encodes an axis-suffix string argument (`Vel Y`, `Pos X`, …) as the integer
/// axis code passed to [`EvalContext::trigger`] (task 4.10, gap 1).
///
/// The parser lowers a space-separated component trigger to a single-argument
/// call whose argument is the axis as a string literal (see
/// [`Expr::Call`](crate::parser::Expr::Call)'s axis-suffix note). Since [`Value`]
/// has no string variant, the axis is passed as a small int: **`X` → 0**,
/// **`Y` → 1**, **`Z` → 2** (case-insensitive). Any other string maps to the
/// safe default `0`, so a malformed axis never panics. A concrete context reads
/// e.g. `Vel Y` as `trigger("Vel", &[Value::Int(1)])`.
fn axis_arg_code(s: &str) -> i32 {
    match s {
        "x" | "X" => 0,
        "y" | "Y" => 1,
        "z" | "Z" => 2,
        _ => 0,
    }
}

/// Evaluates a parsed [`Expr`] against an [`EvalContext`], producing a [`Value`].
///
/// This is the public entry point of the tree-walk evaluator. It walks the AST
/// recursively, applying MUGEN's numeric semantics
/// ([`07-evaluator-semantics.md`][kb]) at each node, and returns the resulting
/// [`Value`].
///
/// # Never panics
///
/// Every error path — divide / modulo by zero, `ln` of a non-positive number, an
/// invalid exponentiation, an unknown trigger, an out-of-range variable index —
/// resolves to the safe default `Value::Int(0)` (via the internal *bottom*
/// sentinel), never a panic. This upholds the engine-wide "never crash on bad
/// content" rule and MUGEN's "an erroring expression never fires a trigger"
/// behavior.
///
/// # Examples
///
/// ```
/// use fp_vm::eval::{EvalContext, Value};
/// use fp_vm::evaluator::eval;
/// use fp_vm::parser::parse_str;
///
/// struct Ctx;
/// impl EvalContext for Ctx {
///     fn trigger(&self, _name: &str, _args: &[Value]) -> Value { Value::DEFAULT }
/// }
///
/// // Integer division truncates toward zero; mixed promotes to float.
/// assert_eq!(eval(&parse_str("7 / 2").unwrap(), &Ctx), Value::Int(3));
/// assert_eq!(eval(&parse_str("7 / 2.0").unwrap(), &Ctx), Value::Float(3.5));
/// // Divide-by-zero is the safe default, not a panic.
/// assert_eq!(eval(&parse_str("1 / 0").unwrap(), &Ctx), Value::Int(0));
/// ```
///
/// [kb]: ../../../docs/knowledge-base/07-evaluator-semantics.md
#[must_use]
pub fn eval(expr: &Expr, ctx: &dyn EvalContext) -> Value {
    eval_inner(expr, ctx).into_value()
}

/// The recursive worker behind [`eval`], carrying the internal [`Eval`]
/// (`Bottom`-aware) value.
fn eval_inner(expr: &Expr, ctx: &dyn EvalContext) -> Eval {
    match expr {
        Expr::Int(n) => Eval::Int(saturate_i64_to_i32(*n)),
        // f64 literals are narrowed to the f32 working precision MUGEN uses.
        Expr::Float(f) => Eval::Float(*f as f32),
        // A bare string literal has no numeric meaning on its own; it only ever
        // appears as a trigger argument (handled in `Call`). Standalone, it is an
        // error value.
        Expr::Str(_) => Eval::Bottom,
        // Bare `random` is the pervasive MUGEN [0,999] form (e.g. `trigger1 =
        // random < 500`); it must draw from the seeded RNG seam, not fall through
        // to the trigger default (see `eval_random` and §8/§11 of
        // docs/knowledge-base/07-evaluator-semantics.md).
        Expr::Ident(name) if name.eq_ignore_ascii_case("random") => eval_random(&[], ctx),
        Expr::Ident(name) => Eval::from_value(ctx.trigger(name, &[])),
        Expr::Unary { op, operand } => eval_unary(*op, operand, ctx),
        Expr::Binary { op, lhs, rhs } => eval_binary(*op, lhs, rhs, ctx),
        Expr::Call { name, args } => eval_call(name, args, ctx),
        // A range only has meaning as the RHS of `=` / `!=`; standalone it is an
        // error (the parser permits it as an atom, the evaluator rejects it).
        Expr::Range { .. } => Eval::Bottom,
        Expr::Redirected { target, expr } => eval_redirect(*target, expr, ctx),
        Expr::AnimElemTail {
            name,
            element,
            op,
            operand,
        } => eval_animelem_tail(name, element, *op, operand, ctx),
        Expr::TimeModTail { divisor, remainder } => eval_timemod_tail(divisor, remainder, ctx),
        Expr::HitDefAttrTail {
            standtype,
            attr_codes,
        } => eval_hitdefattr_tail(standtype, attr_codes, ctx),
        // Projectiles are unimplemented; the parsed two-argument projectile-info
        // form (Task A) exists only so the surrounding boolean survives. It always
        // reports "no projectiles" → `0`, so it never fires a trigger.
        Expr::ProjTail { .. } => Eval::Int(0),
    }
}

/// Evaluates the two-argument `TimeMod = d, c` form (Task A).
///
/// MUGEN semantics: true iff `(Time % d) == c`, where `Time` is the state-time.
/// The evaluator reads `Time` once via [`EvalContext::trigger`] and computes the
/// modulo in integer arithmetic. A zero (or bottom) divisor makes the modulo
/// undefined; per the engine-wide "bad expression → never fires" rule this yields
/// `0` rather than panicking. Both operands narrow to int (MUGEN's `Time` and the
/// modulo operands are integers).
fn eval_timemod_tail(divisor: &Expr, remainder: &Expr, ctx: &dyn EvalContext) -> Eval {
    let d = eval_inner(divisor, ctx);
    let c = eval_inner(remainder, ctx);
    if Eval::either_bottom(d, c) {
        return Eval::Bottom;
    }
    let d = d.to_int();
    let c = c.to_int();
    if d == 0 {
        // Modulo by zero is undefined → the trigger never fires (safe default).
        return Eval::Int(0);
    }
    let time = Eval::from_value(ctx.trigger("Time", &[])).to_int();
    // MUGEN's `%` is the C-style remainder (`rem`), which `i32::rem_euclid` does
    // not match for negative `time`; `time` is non-negative in practice (a state
    // clock), and the wrapping rem guards the lone `i32::MIN % -1` overflow case.
    let modulo = time.wrapping_rem(d);
    Eval::Int(i32::from(modulo == c))
}

/// Evaluates the two-argument `HitDefAttr = <standtype>, <attr-list>` form
/// (Task A).
///
/// MUGEN semantics: true iff the character's currently-active `HitDef` has a
/// stand-type matching `standtype` (`S`/`C`/`A`) **and** an attack-attribute
/// 2-char code present in `attr_codes`. The decision is delegated to the
/// [`EvalContext::hitdef_attr_matches`] seam, which a concrete entity answers
/// against its active `HitDef`. With no `HitDef` active (or on a context that
/// does not model one — the default seam) the match is `false`, so the form
/// evaluates to `0` and the surrounding `&& movecontact` simply does not fire.
fn eval_hitdefattr_tail(standtype: &str, attr_codes: &[String], ctx: &dyn EvalContext) -> Eval {
    Eval::Int(i32::from(ctx.hitdef_attr_matches(standtype, attr_codes)))
}

/// Evaluates the two-parameter `AnimElem = N, op M` comparison form (task 4.10,
/// gap 2).
///
/// MUGEN semantics: true iff the animation has **reached** element `N` *and* the
/// element time satisfies the secondary comparison. The evaluator reads the
/// element time once via `trigger("AnimElemTime", &[N])` and computes:
///
/// - **reached**: `AnimElemTime(N) >= 0` (a not-yet-reached element reads
///   negative in MUGEN); and
/// - **secondary**: `AnimElemTime(N) op M`.
///
/// Both must hold. The single trigger read is reused for both checks. A bottom
/// element index or operand propagates (the conjunction is then bottom → never
/// fires). The trigger name is passed through verbatim so a context keys on the
/// family member the author wrote (`AnimElem`, `AnimElemTime`, …); the resolver
/// is expected to answer the underlying element-time for any of them.
fn eval_animelem_tail(
    name: &str,
    element: &Expr,
    op: BinaryOp,
    operand: &Expr,
    ctx: &dyn EvalContext,
) -> Eval {
    let n = eval_inner(element, ctx);
    if matches!(n, Eval::Bottom) {
        return Eval::Bottom;
    }
    // Read the element time once. The family members all reduce to "time since
    // element N", so resolve through `AnimElemTime` with the element number.
    let _ = name; // name kept on the AST for diagnostics / future per-family logic
    let elem_time = Eval::from_value(ctx.trigger("AnimElemTime", &[n.into_value()]));
    // "Reached" = element time is non-negative.
    let reached = eval_binary_values(BinaryOp::Ge, elem_time, Eval::Int(0));
    if !reached.as_bool() {
        return Eval::Int(0);
    }
    // Secondary comparison against M.
    let m = eval_inner(operand, ctx);
    let secondary = eval_binary_values(op, elem_time, m);
    Eval::Int(i32::from(secondary.as_bool()))
}

/// Evaluates a redirected sub-expression `redirect, expr` (see
/// `docs/knowledge-base/03-engine-architecture.md` §4, redirections).
///
/// Resolves the [`Redirect`](crate::eval::Redirect) via
/// [`EvalContext::redirect`] and evaluates `expr` against the target context. A
/// missing target (the redirect resolves to [`None`] — no parent, absent helper
/// id, no current target, …) yields the safe default: the whole redirected
/// sub-expression is bottom, which collapses to `0` at the public boundary, so
/// an unresolved redirect never fires a trigger and never panics.
fn eval_redirect(target: Redirect, expr: &Expr, ctx: &dyn EvalContext) -> Eval {
    match ctx.redirect(target) {
        Some(redirected) => eval_inner(expr, redirected),
        None => Eval::Bottom,
    }
}

/// Evaluates a unary prefix operation (`! - ~`).
fn eval_unary(op: UnaryOp, operand: &Expr, ctx: &dyn EvalContext) -> Eval {
    let v = eval_inner(operand, ctx);
    match op {
        // `!x` is 1 iff x is falsey; bottom is falsey, so `!bottom == 1`.
        UnaryOp::Not => Eval::Int(i32::from(!v.as_bool())),
        // `-x` preserves type; `-bottom` stays bottom.
        UnaryOp::Neg => match v {
            Eval::Int(i) => Eval::Int(i.wrapping_neg()),
            Eval::Float(f) => Eval::Float(-f),
            Eval::Bottom => Eval::Bottom,
        },
        // `~x` requires an int; a float narrows via the saturating to_int, and
        // bottom propagates (§9).
        UnaryOp::BitNot => match v {
            Eval::Bottom => Eval::Bottom,
            other => Eval::Int(!other.to_int()),
        },
    }
}

/// Evaluates a binary operation, dispatching by operator class.
fn eval_binary(op: BinaryOp, lhs: &Expr, rhs: &Expr, ctx: &dyn EvalContext) -> Eval {
    match op {
        // Short-circuit logical operators evaluate the RHS lazily.
        BinaryOp::And => {
            let l = eval_inner(lhs, ctx);
            if !l.as_bool() {
                return Eval::Int(0);
            }
            Eval::Int(i32::from(eval_inner(rhs, ctx).as_bool()))
        }
        BinaryOp::Or => {
            let l = eval_inner(lhs, ctx);
            if l.as_bool() {
                return Eval::Int(1);
            }
            Eval::Int(i32::from(eval_inner(rhs, ctx).as_bool()))
        }
        // Equality / inequality: the RHS may be a range literal, handled here.
        BinaryOp::Eq | BinaryOp::Ne => eval_eq_ne(op, lhs, rhs, ctx),
        // Everything else evaluates both operands eagerly.
        _ => {
            let l = eval_inner(lhs, ctx);
            let r = eval_inner(rhs, ctx);
            eval_binary_values(op, l, r)
        }
    }
}

/// Evaluates `=` / `!=`, special-casing a range literal on the right (§6) and
/// the `command = "name"` string-equality form (task 4.10, gap 4).
fn eval_eq_ne(op: BinaryOp, lhs: &Expr, rhs: &Expr, ctx: &dyn EvalContext) -> Eval {
    if let Expr::Range {
        lower_bound,
        lower,
        upper,
        upper_bound,
    } = rhs
    {
        let x = eval_inner(lhs, ctx);
        let a = eval_inner(lower, ctx);
        let b = eval_inner(upper, ctx);
        return eval_range(op, x, *lower_bound, a, b, *upper_bound);
    }
    // String-equality: `command = "name"` / `"name" = command` (either order)
    // routes through the boolean string-keyed seam rather than a numeric read,
    // because a string RHS has no numeric Value. `command = "x"` is `1` iff the
    // named command fired this tick; `!=` negates it (task 4.10, gap 4).
    if let Some(name) = command_string_compare(lhs, rhs) {
        let active = ctx.command_active(name);
        let result = if matches!(op, BinaryOp::Ne) {
            !active
        } else {
            active
        };
        return Eval::Int(i32::from(result));
    }
    let l = eval_inner(lhs, ctx);
    let r = eval_inner(rhs, ctx);
    eval_binary_values(op, l, r)
}

/// Recognizes the `command = "name"` shape in either operand order, returning
/// the quoted command name. One side must be the bare `command` trigger
/// (case-insensitive [`Expr::Ident`]) and the other a string literal
/// ([`Expr::Str`]); otherwise [`None`] (so an ordinary numeric comparison is
/// used). See [task 4.10's evaluator notes](self) and
/// [`EvalContext::command_active`](crate::eval::EvalContext::command_active).
fn command_string_compare<'a>(lhs: &'a Expr, rhs: &'a Expr) -> Option<&'a str> {
    let is_command = |e: &Expr| matches!(e, Expr::Ident(n) if n.eq_ignore_ascii_case("command"));
    match (lhs, rhs) {
        (Expr::Str(s), other) | (other, Expr::Str(s)) if is_command(other) => Some(s.as_str()),
        _ => None,
    }
}

/// Computes range membership `x (=|!=) <lower..upper>` per §6.
///
/// Bottom propagates (range checks are bottom-propagating, matching Ikemen's
/// `rangeCheck`). `=` returns membership, `!=` returns its negation.
fn eval_range(
    op: BinaryOp,
    x: Eval,
    lower_bound: Bound,
    a: Eval,
    b: Eval,
    upper_bound: Bound,
) -> Eval {
    if matches!(x, Eval::Bottom) || matches!(a, Eval::Bottom) || matches!(b, Eval::Bottom) {
        return Eval::Bottom;
    }
    // Tri-operand promotion: if any of x/a/b is float, compare all as float.
    let lo_ok = if x.is_float() || a.is_float() {
        let (xf, af) = (x.to_float(), a.to_float());
        match lower_bound {
            Bound::Inclusive => xf >= af,
            Bound::Exclusive => xf > af,
        }
    } else {
        let (xi, ai) = (x.to_int(), a.to_int());
        match lower_bound {
            Bound::Inclusive => xi >= ai,
            Bound::Exclusive => xi > ai,
        }
    };
    let hi_ok = if x.is_float() || b.is_float() {
        let (xf, bf) = (x.to_float(), b.to_float());
        match upper_bound {
            Bound::Inclusive => xf <= bf,
            Bound::Exclusive => xf < bf,
        }
    } else {
        let (xi, bi) = (x.to_int(), b.to_int());
        match upper_bound {
            Bound::Inclusive => xi <= bi,
            Bound::Exclusive => xi < bi,
        }
    };
    let member = lo_ok && hi_ok;
    // `!=` negates membership (De Morgan of the bound checks, §6). Only `Ne`
    // negates; everything else (only `Eq` ever reaches here) returns membership
    // directly — defaulting rather than panicking keeps the never-panic contract.
    let result = if matches!(op, BinaryOp::Ne) {
        !member
    } else {
        member
    };
    Eval::Int(i32::from(result))
}

/// Applies a non-short-circuiting binary operator to two already-evaluated
/// operands.
fn eval_binary_values(op: BinaryOp, l: Eval, r: Eval) -> Eval {
    match op {
        BinaryOp::Add => arith(l, r, |a, b| a.wrapping_add(b), |a, b| a + b),
        BinaryOp::Sub => arith(l, r, |a, b| a.wrapping_sub(b), |a, b| a - b),
        BinaryOp::Mul => arith(l, r, |a, b| a.wrapping_mul(b), |a, b| a * b),
        BinaryOp::Div => eval_div(l, r),
        BinaryOp::Mod => eval_mod(l, r),
        BinaryOp::Pow => eval_pow(l, r),
        BinaryOp::Lt => compare(l, r, |o| o.is_lt()),
        BinaryOp::Le => compare(l, r, |o| o.is_le()),
        BinaryOp::Gt => compare(l, r, |o| o.is_gt()),
        BinaryOp::Ge => compare(l, r, |o| o.is_ge()),
        BinaryOp::Eq => compare(l, r, |o| o.is_eq()),
        BinaryOp::Ne => compare(l, r, |o| o.is_ne()),
        BinaryOp::BitAnd => bitwise(l, r, |a, b| a & b),
        BinaryOp::BitOr => bitwise(l, r, |a, b| a | b),
        BinaryOp::BitXor => bitwise(l, r, |a, b| a ^ b),
        // && and || are handled in `eval_binary` (short-circuit); reaching here
        // would be a logic error, but default safely rather than panic.
        BinaryOp::And => Eval::Int(i32::from(l.as_bool() && r.as_bool())),
        BinaryOp::Or => Eval::Int(i32::from(l.as_bool() || r.as_bool())),
    }
}

/// The shared arithmetic path for `+ - *` with float promotion.
///
/// Both int → native **wrapping** `i32` (per §4) via `int_op`; any float operand
/// → `f32` via `float_op` (rounded to f32 after the op, §11). Bottom propagates.
///
/// A **non-finite float result** (`NaN` or `±inf`) funnels to [`Eval::Bottom`],
/// matching the `**` / `ln` / transcendental paths so a public
/// [`Value::Float`](crate::eval::Value::Float)`(NaN)` can never escape [`eval`]
/// from ordinary float `+ - *` (e.g. `0 * <inf-producing expr>`, audit #19).
/// `Bottom` collapses to the safe default `0` at the public boundary, preserving
/// the never-panic + "an erroring expression never fires a trigger" contract.
/// The integer path is unaffected — native wrapping `i32` is always finite, so
/// the saturating-overflow numeric contract on `+ - *` is unchanged.
fn arith(l: Eval, r: Eval, int_op: fn(i32, i32) -> i32, float_op: fn(f32, f32) -> f32) -> Eval {
    if Eval::either_bottom(l, r) {
        return Eval::Bottom;
    }
    if l.is_float() || r.is_float() {
        let result = float_op(l.to_float(), r.to_float());
        if result.is_finite() {
            Eval::Float(result)
        } else {
            // NaN (e.g. `0 * inf`) or ±inf (e.g. a huge literal overflowing f32):
            // an invalid / non-representable float result → bottom (§1, §11).
            Eval::Bottom
        }
    } else {
        Eval::Int(int_op(l.to_int(), r.to_int()))
    }
}

/// Division (§2): int/int → truncating-toward-zero; any float → f32; `/0` →
/// bottom; bottom propagates.
fn eval_div(l: Eval, r: Eval) -> Eval {
    if Eval::either_bottom(l, r) {
        return Eval::Bottom;
    }
    if l.is_float() || r.is_float() {
        let d = r.to_float();
        // Ikemen guards on the float value of the divisor, so x/0 and x/0.0 both
        // bottom (§2).
        if d == 0.0 {
            return Eval::Bottom;
        }
        Eval::Float(l.to_float() / d)
    } else {
        let d = r.to_int();
        if d == 0 {
            return Eval::Bottom;
        }
        // Rust `/` on i32 truncates toward zero, matching C / MUGEN. The only
        // overflowing case is `i32::MIN / -1`; use wrapping to avoid a panic.
        Eval::Int(l.to_int().wrapping_div(d))
    }
}

/// Modulo (§2): int-only, sign of the dividend; float operand or `%0` → bottom;
/// bottom propagates.
fn eval_mod(l: Eval, r: Eval) -> Eval {
    if Eval::either_bottom(l, r) {
        return Eval::Bottom;
    }
    // Per the documented Elecbyte contract, `%` on any float operand is bottom
    // (fp-vm follows the doc rather than Ikemen's coerce-to-int, §2).
    if l.is_float() || r.is_float() {
        return Eval::Bottom;
    }
    let d = r.to_int();
    if d == 0 {
        return Eval::Bottom;
    }
    // Rust `%` takes the sign of the dividend (C semantics), matching §2. Guard
    // the `i32::MIN % -1` overflow with wrapping_rem.
    Eval::Int(l.to_int().wrapping_rem(d))
}

/// Exponentiation (§3): right-associativity is handled by the parser. Here:
/// `int ** nonneg-int` stays int and **saturates to [`i32::MAX`]** on overflow;
/// any float operand or a negative exponent uses `f32` `powf`; a `NaN` result
/// (invalid exponentiation) → bottom; bottom propagates.
fn eval_pow(base: Eval, exp: Eval) -> Eval {
    if Eval::either_bottom(base, exp) {
        return Eval::Bottom;
    }
    // Float path: any float operand OR a negative exponent.
    let neg_exp = !exp.is_float() && exp.to_int() < 0;
    if base.is_float() || exp.is_float() || neg_exp {
        let result = base.to_float().powf(exp.to_float());
        if result.is_nan() {
            // e.g. (-1) ** 0.5 — invalid exponentiation → bottom (§3).
            return Eval::Bottom;
        }
        return Eval::Float(result);
    }
    // Int path: int ** nonneg-int via repeated multiplication, saturating to
    // i32::MAX on overflow (the documented contract, not Ikemen's wrap, §3).
    Eval::Int(ipow_saturating(base.to_int(), exp.to_int()))
}

/// `base ** exp` for a non-negative integer exponent, saturating to [`i32::MAX`]
/// on overflow (§3). `x ** 0 == 1` for all `x` (including `0 ** 0 == 1`).
fn ipow_saturating(base: i32, exp: i32) -> i32 {
    debug_assert!(exp >= 0, "ipow_saturating requires a non-negative exponent");
    let mut acc: i32 = 1;
    let mut b = base;
    let mut e = exp;
    // Exponentiation by squaring; checked_mul detects overflow → saturate.
    while e > 0 {
        if e & 1 == 1 {
            match acc.checked_mul(b) {
                Some(v) => acc = v,
                None => {
                    tracing::warn!("integer exponentiation overflowed; saturating to i32::MAX");
                    return i32::MAX;
                }
            }
        }
        e >>= 1;
        if e > 0 {
            match b.checked_mul(b) {
                Some(v) => b = v,
                None => {
                    tracing::warn!("integer exponentiation overflowed; saturating to i32::MAX");
                    return i32::MAX;
                }
            }
        }
    }
    acc
}

/// Comparison (§5): promote to float if either operand is float, else compare as
/// i32; returns int `1`/`0`. Bottom on either side → bottom (an erroring compare
/// never fires).
fn compare(l: Eval, r: Eval, pick: fn(std::cmp::Ordering) -> bool) -> Eval {
    if Eval::either_bottom(l, r) {
        return Eval::Bottom;
    }
    let ord = if l.is_float() || r.is_float() {
        // NaN never appears here (bottom is filtered above and operands are real
        // Int/Float), but partial_cmp is still partial; treat an absent ordering
        // as "not equal / not ordered" → false for every relational predicate.
        match l.to_float().partial_cmp(&r.to_float()) {
            Some(o) => o,
            None => return Eval::Int(0),
        }
    } else {
        l.to_int().cmp(&r.to_int())
    };
    Eval::Int(i32::from(pick(ord)))
}

/// Bitwise `& | ^` (§9): operands narrow to i32 (a float via the saturating
/// to_int); bottom propagates. Returns int.
fn bitwise(l: Eval, r: Eval, op: fn(i32, i32) -> i32) -> Eval {
    if Eval::either_bottom(l, r) {
        return Eval::Bottom;
    }
    Eval::Int(op(l.to_int(), r.to_int()))
}

/// Evaluates a function call or parameterized trigger (§8).
///
/// Built-in math / control functions are dispatched by (case-insensitive) name;
/// `var` / `fvar` / `sysvar` route to the typed [`EvalContext`] fast paths; and
/// any other name is treated as a parameterized trigger resolved via
/// [`EvalContext::trigger`] with the evaluated arguments.
fn eval_call(name: &str, args: &[Expr], ctx: &dyn EvalContext) -> Eval {
    let lname = name.to_ascii_lowercase();
    match lname.as_str() {
        // ---- Lazy control flow: only the taken branch is evaluated ----
        "cond" | "ifelse" if args.len() == 3 => {
            let c = eval_inner(&args[0], ctx);
            if c.as_bool() {
                eval_inner(&args[1], ctx)
            } else {
                eval_inner(&args[2], ctx)
            }
        }
        // ---- Typed variable banks ----
        "var" if args.len() == 1 => {
            let idx = eval_inner(&args[0], ctx).to_int();
            Eval::from_value(ctx.var(idx))
        }
        "fvar" if args.len() == 1 => {
            let idx = eval_inner(&args[0], ctx).to_int();
            Eval::from_value(ctx.fvar(idx))
        }
        "sysvar" if args.len() == 1 => {
            let idx = eval_inner(&args[0], ctx).to_int();
            Eval::from_value(ctx.sysvar(idx))
        }
        // ---- Type-preserving unary math ----
        "abs" if args.len() == 1 => match eval_inner(&args[0], ctx) {
            Eval::Int(i) => Eval::Int(i.wrapping_abs()),
            Eval::Float(f) => Eval::Float(f.abs()),
            Eval::Bottom => Eval::Bottom,
        },
        // floor / ceil → int; an int arg passes through; NaN → bottom (§8).
        "floor" if args.len() == 1 => round_to_int(eval_inner(&args[0], ctx), f32::floor),
        "ceil" if args.len() == 1 => round_to_int(eval_inner(&args[0], ctx), f32::ceil),
        // ---- Type-preserving binary min / max (intentional divergence from
        // Ikemen's float-always; matches the doc & author expectation, §8) ----
        "min" if args.len() == 2 => {
            minmax(eval_inner(&args[0], ctx), eval_inner(&args[1], ctx), true)
        }
        "max" if args.len() == 2 => {
            minmax(eval_inner(&args[0], ctx), eval_inner(&args[1], ctx), false)
        }
        // ---- Transcendentals → f32 (§8) ----
        "sin" if args.len() == 1 => float_fn(eval_inner(&args[0], ctx), f32::sin),
        "cos" if args.len() == 1 => float_fn(eval_inner(&args[0], ctx), f32::cos),
        "atan" if args.len() == 1 => float_fn(eval_inner(&args[0], ctx), f32::atan),
        "exp" if args.len() == 1 => float_fn(eval_inner(&args[0], ctx), f32::exp),
        // ln(x): x <= 0 → bottom (§8).
        "ln" if args.len() == 1 => {
            let v = eval_inner(&args[0], ctx);
            if matches!(v, Eval::Bottom) {
                return Eval::Bottom;
            }
            let x = v.to_float();
            if x <= 0.0 {
                return Eval::Bottom;
            }
            Eval::Float(x.ln())
        }
        // ---- Deterministic random (parameterized form) ----
        // Bare `random` is routed to `eval_random` from the `Ident` arm of
        // `eval_inner`; this call arm handles the parened forms `random()` /
        // `random(lo, hi)` (inclusive integer range) through the entity-owned RNG
        // seam (§11). With no args it is [0,999].
        "random" => eval_random(args, ctx),
        // ---- Member-keyed triggers: argument is a named field, not a number ----
        // `GetHitVar(member)` selects a hit field BY NAME (`GetHitVar(fall.yvel)`,
        // `GetHitVar(xveladd)`) and `const(member)` reads an authored character
        // constant BY NAME (`const(velocity.walk.fwd.x)`, `const(movement.yaccel)`).
        // The member is a bare identifier (a dotted member lexes as ONE identifier
        // since task 4.10); routing it through the numeric `trigger` path would
        // evaluate the dotted ident as a nested trigger and collapse it to 0 (a
        // `Value` has no string variant). So when the sole argument is a bare
        // identifier we pass the member NAME verbatim through the string-keyed
        // `trigger_str` seam (task 4.11 for GetHitVar; task 5.6d for const). Any
        // non-ident argument (a rare numeric/computed form) falls through to the
        // ordinary numeric path below.
        _ if is_member_keyed_trigger(&lname) => {
            if let [Expr::Ident(member)] = args {
                Eval::from_value(ctx.trigger_str(name, member))
            } else {
                eval_numeric_trigger(name, args, ctx)
            }
        }
        // ---- Otherwise: a parameterized trigger ----
        _ => eval_numeric_trigger(name, args, ctx),
    }
}

/// The complete set of **member-keyed** trigger names (lowercased): triggers
/// whose argument is a *named field* rather than a number, so a bare-identifier
/// argument is routed through [`EvalContext::trigger_str`] instead of being
/// collapsed to a number on the numeric [`EvalContext::trigger`] path.
///
/// This is the single source of truth for the member-keyed function set:
///
/// - **`gethitvar`** — `GetHitVar(fall.yvel)`, `GetHitVar(xveladd)`,
///   `GetHitVar(animtype)`: selects a field of the most-recent hit by name
///   (task 4.11, item a).
/// - **`const`** — `const(velocity.walk.fwd.x)`, `const(size.ground.front)`,
///   `const(movement.yaccel)`: reads a character's authored constant by its
///   dotted member name (task 5.6d). The dotted member lexes as one identifier
///   (since task 4.10), so it reaches here verbatim; without this routing the
///   dotted ident would be evaluated as a nested trigger and yield `0`.
///
/// An audit of MUGEN's trigger surface found no other functions whose argument
/// is an arbitrary field *name* rather than an expression: parameterized
/// triggers (`var` / `fvar` / `sysvar`, `AnimElem`, `NumExplod`, …) take numeric
/// arguments and stay on the numeric path. So the set is exactly these two.
const MEMBER_KEYED_TRIGGERS: [&str; 2] = ["gethitvar", "const"];

/// Returns whether `lname` (already lowercased) is a **member-keyed** trigger:
/// one whose argument is a named field rather than a number, so a bare-identifier
/// argument is routed through [`EvalContext::trigger_str`] instead of the numeric
/// [`EvalContext::trigger`] path.
///
/// See [`MEMBER_KEYED_TRIGGERS`] for the documented set (`GetHitVar`, `const`).
fn is_member_keyed_trigger(lname: &str) -> bool {
    MEMBER_KEYED_TRIGGERS.contains(&lname)
}

/// Evaluates a parameterized trigger via the numeric [`EvalContext::trigger`]
/// path: arguments are evaluated left-to-right into [`Value`]s for the lookup.
///
/// A string argument has no numeric [`Value`]; the real-content string-arg shape
/// is the axis word (`Vel Y` → `Str("Y")`), which maps to its axis code
/// `X=0` / `Y=1` / `Z=2` (see [`axis_arg_code`]). Any other string maps to the
/// safe default `0`.
fn eval_numeric_trigger(name: &str, args: &[Expr], ctx: &dyn EvalContext) -> Eval {
    let mut evaluated = Vec::with_capacity(args.len());
    for a in args {
        let v = match a {
            Expr::Str(s) => Value::Int(axis_arg_code(s)),
            other => eval_inner(other, ctx).into_value(),
        };
        evaluated.push(v);
    }
    Eval::from_value(ctx.trigger(name, &evaluated))
}

/// Implements `random` / `random(lo, hi)` via the entity-owned RNG seam (§11).
fn eval_random(args: &[Expr], ctx: &dyn EvalContext) -> Eval {
    // Read one deterministic draw in [0, 2^31-2] from the context.
    let raw = ctx.random();
    match args.len() {
        // Bare `random` → classic MUGEN [0, 999].
        0 => Eval::Int(raw.rem_euclid(1000)),
        // `random(lo, hi)` → inclusive integer range [lo, hi].
        2 => {
            let lo = eval_inner(&args[0], ctx).to_int();
            let hi = eval_inner(&args[1], ctx).to_int();
            let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
            let span = (hi as i64) - (lo as i64) + 1;
            Eval::Int((lo as i64 + (raw as i64).rem_euclid(span)) as i32)
        }
        // Any other arity is malformed → safe default.
        _ => Eval::Bottom,
    }
}

/// `floor` / `ceil`: an int passes through unchanged; a float is rounded with
/// `f` then narrowed to int (saturating); NaN → bottom; bottom propagates (§8).
fn round_to_int(v: Eval, f: fn(f32) -> f32) -> Eval {
    match v {
        Eval::Int(i) => Eval::Int(i),
        Eval::Float(x) => {
            if x.is_nan() {
                Eval::Bottom
            } else {
                Eval::Int(Eval::Float(f(x)).to_int())
            }
        }
        Eval::Bottom => Eval::Bottom,
    }
}

/// Type-preserving `min` / `max`: two ints → int, any float → float; bottom
/// propagates. `is_min` selects min vs max (§8).
fn minmax(a: Eval, b: Eval, is_min: bool) -> Eval {
    if Eval::either_bottom(a, b) {
        return Eval::Bottom;
    }
    if a.is_float() || b.is_float() {
        let (af, bf) = (a.to_float(), b.to_float());
        let pick = if is_min { af.min(bf) } else { af.max(bf) };
        Eval::Float(pick)
    } else {
        let (ai, bi) = (a.to_int(), b.to_int());
        let pick = if is_min { ai.min(bi) } else { ai.max(bi) };
        Eval::Int(pick)
    }
}

/// A unary float-returning math function: the arg promotes to f32, the result is
/// f32; bottom propagates (§8).
fn float_fn(v: Eval, f: fn(f32) -> f32) -> Eval {
    match v {
        Eval::Bottom => Eval::Bottom,
        other => {
            let r = f(other.to_float());
            if r.is_nan() {
                Eval::Bottom
            } else {
                Eval::Float(r)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{EvalContext, Redirect, Value};
    use crate::parser::{parse_str, ParseError};
    use std::cell::Cell;
    use std::collections::HashMap;

    /// Renders `(name, args)` into a stable, case-insensitive lookup key, mirroring
    /// the convention used by `eval.rs`'s own `MockContext`.
    fn trigger_key(name: &str, args: &[Value]) -> String {
        let mut key = name.to_ascii_lowercase();
        for arg in args {
            key.push('|');
            key.push_str(&arg.to_string());
        }
        key
    }

    /// Renders `(name, member)` into a stable, case-insensitive lookup key for
    /// the member-keyed trigger path (`GetHitVar(member)`).
    fn member_key(name: &str, member: &str) -> String {
        format!(
            "{}#{}",
            name.to_ascii_lowercase(),
            member.to_ascii_lowercase()
        )
    }

    /// An in-memory [`EvalContext`] for evaluator tests, with a deterministic RNG
    /// seam so `random` is reproducible.
    #[derive(Default)]
    struct MockContext {
        triggers: HashMap<String, Value>,
        member_triggers: HashMap<String, Value>,
        vars: HashMap<i32, i32>,
        fvars: HashMap<i32, f32>,
        sysvars: HashMap<i32, i32>,
        rng: Cell<Option<Rng>>,
        redirects: HashMap<Redirect, Box<MockContext>>,
    }

    impl MockContext {
        fn new() -> Self {
            Self::default()
        }
        fn with_trigger(mut self, name: &str, args: &[Value], value: Value) -> Self {
            self.triggers.insert(trigger_key(name, args), value);
            self
        }
        fn with_member_trigger(mut self, name: &str, member: &str, value: Value) -> Self {
            self.member_triggers.insert(member_key(name, member), value);
            self
        }
        fn with_var(mut self, index: i32, value: i32) -> Self {
            self.vars.insert(index, value);
            self
        }
        fn with_fvar(mut self, index: i32, value: f32) -> Self {
            self.fvars.insert(index, value);
            self
        }
        fn with_sysvar(mut self, index: i32, value: i32) -> Self {
            self.sysvars.insert(index, value);
            self
        }
        fn with_seed(self, seed: i32) -> Self {
            self.rng.set(Some(Rng::new(seed)));
            self
        }
        fn with_redirect(mut self, target: Redirect, ctx: MockContext) -> Self {
            self.redirects.insert(target, Box::new(ctx));
            self
        }
    }

    impl EvalContext for MockContext {
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
        fn sysvar(&self, index: i32) -> Value {
            self.sysvars
                .get(&index)
                .copied()
                .map_or(Value::DEFAULT, Value::Int)
        }
        fn redirect(&self, target: Redirect) -> Option<&dyn EvalContext> {
            self.redirects
                .get(&target)
                .map(|boxed| boxed.as_ref() as &dyn EvalContext)
        }
        fn random(&self) -> i32 {
            // Advance the entity-owned RNG; if unseeded, fall back to the default 0.
            match self.rng.get() {
                Some(mut rng) => {
                    let v = rng.next_u31();
                    self.rng.set(Some(rng));
                    v
                }
                None => 0,
            }
        }
    }

    /// Convenience: parse + eval against a context.
    fn ev(src: &str, ctx: &dyn EvalContext) -> Value {
        eval(&parse_str(src).expect("parse"), ctx)
    }

    /// Convenience: parse + eval against an empty context.
    fn evb(src: &str) -> Value {
        ev(src, &MockContext::new())
    }

    // ---- Rng: determinism + bounds (§11) ----

    #[test]
    fn rng_is_deterministic_for_same_seed() {
        let mut a = Rng::new(12345);
        let mut b = Rng::new(12345);
        for _ in 0..100 {
            assert_eq!(a.next_u31(), b.next_u31());
        }
    }

    #[test]
    fn rng_range_is_inclusive_and_in_bounds() {
        let mut r = Rng::new(7);
        for _ in 0..1000 {
            let v = r.next_range(0, 999);
            assert!((0..=999).contains(&v));
        }
        // Single-value span.
        assert_eq!(Rng::new(1).next_range(5, 5), 5);
        // Swapped bounds are handled.
        let mut r2 = Rng::new(7);
        let v = r2.next_range(10, 0);
        assert!((0..=10).contains(&v));
    }

    #[test]
    fn rng_normalizes_zero_and_negative_seeds() {
        // A 0 / negative seed must not collapse the generator (no panic, valid draws).
        let mut a = Rng::new(0);
        let mut b = Rng::new(-5);
        assert!(a.next_u31() > 0);
        assert!(b.next_u31() > 0);
    }

    // ---- Literals & narrowing ----

    #[test]
    fn int_and_float_literals() {
        assert_eq!(evb("42"), Value::Int(42));
        assert_eq!(evb("3.5"), Value::Float(3.5));
        assert_eq!(evb("-7"), Value::Int(-7));
    }

    #[test]
    fn oversized_int_literal_saturates() {
        // 9999999999 > i32::MAX → saturate (CB4), never wrap or zero.
        assert_eq!(evb("9999999999"), Value::Int(i32::MAX));
    }

    // ---- Arithmetic + promotion (AC2) ----

    #[test]
    fn arithmetic_int_stays_int() {
        assert_eq!(evb("2 + 3"), Value::Int(5));
        assert_eq!(evb("10 - 4"), Value::Int(6));
        assert_eq!(evb("6 * 7"), Value::Int(42));
    }

    #[test]
    fn arithmetic_promotes_to_float() {
        assert_eq!(evb("2 + 3.0"), Value::Float(5.0));
        assert_eq!(evb("2.0 * 3"), Value::Float(6.0));
        assert_eq!(evb("1.5 - 1"), Value::Float(0.5));
    }

    #[test]
    fn int_add_wraps_on_overflow() {
        // 2_000_000_000 + 2_000_000_000 wraps (native i32 two's complement, §4).
        let expected = 2_000_000_000i32.wrapping_add(2_000_000_000);
        assert_eq!(evb("2000000000 + 2000000000"), Value::Int(expected));
        assert!(expected < 0); // it did wrap negative
    }

    // ---- Division & div-by-zero → 0 (AC2) ----

    #[test]
    fn int_division_truncates_toward_zero() {
        assert_eq!(evb("7 / 2"), Value::Int(3));
        assert_eq!(evb("-7 / 2"), Value::Int(-3));
        assert_eq!(evb("7 / -2"), Value::Int(-3));
    }

    #[test]
    fn float_division_when_either_float() {
        assert_eq!(evb("7.0 / 2.0"), Value::Float(3.5));
        assert_eq!(evb("7 / 2.0"), Value::Float(3.5));
    }

    #[test]
    fn divide_by_zero_is_default_zero() {
        assert_eq!(evb("1 / 0"), Value::Int(0));
        assert_eq!(evb("1.0 / 0.0"), Value::Int(0));
        assert_eq!(evb("5 / 0.0"), Value::Int(0));
    }

    #[test]
    fn int_min_divided_by_neg_one_does_not_panic() {
        // `i32::MIN / -1` overflows native i32 division (the quotient i32::MAX+1
        // is unrepresentable) and would panic with plain `/`; `wrapping_div`
        // keeps it safe. i32::MIN can't be written as a literal (it saturates),
        // so inject it through a variable bank.
        let ctx = MockContext::new().with_var(0, i32::MIN);
        // var(0) / -1 — wrapping_div yields i32::MIN, never a panic.
        assert_eq!(ev("var(0) / (0 - 1)", &ctx), Value::Int(i32::MIN));
        // Modulo equivalent: i32::MIN % -1 must also not panic (→ 0).
        assert_eq!(ev("var(0) % (0 - 1)", &ctx), Value::Int(0));
    }

    // ---- Modulo (AC2) ----

    #[test]
    fn modulo_int_sign_of_dividend() {
        assert_eq!(evb("7 % 3"), Value::Int(1));
        assert_eq!(evb("-7 % 3"), Value::Int(-1));
        assert_eq!(evb("7 % -3"), Value::Int(1));
    }

    #[test]
    fn modulo_by_zero_and_float_is_default() {
        assert_eq!(evb("5 % 0"), Value::Int(0));
        // Float operand → bottom → 0 (doc-faithful, §2).
        assert_eq!(evb("5.0 % 2"), Value::Int(0));
        assert_eq!(evb("5 % 2.0"), Value::Int(0));
    }

    // ---- Exponentiation: right-assoc + overflow saturation (AC2) ----

    #[test]
    fn pow_is_right_associative() {
        // 2 ** 3 ** 2 == 2 ** 9 == 512 (right-assoc).
        assert_eq!(evb("2 ** 3 ** 2"), Value::Int(512));
    }

    #[test]
    fn pow_int_nonneg_stays_int() {
        assert_eq!(evb("2 ** 10"), Value::Int(1024));
        assert_eq!(evb("5 ** 0"), Value::Int(1));
        assert_eq!(evb("0 ** 0"), Value::Int(1));
    }

    #[test]
    fn pow_int_overflow_saturates_to_max() {
        // 2 ** 31 overflows i32 → saturate to i32::MAX (documented, not wrap, §3).
        assert_eq!(evb("2 ** 31"), Value::Int(i32::MAX));
        assert_eq!(evb("10 ** 20"), Value::Int(i32::MAX));
    }

    #[test]
    fn pow_float_and_negative_exponent() {
        assert_eq!(evb("2.0 ** 3.0"), Value::Float(8.0));
        // Negative exponent promotes to float.
        assert_eq!(evb("2 ** -1"), Value::Float(0.5));
    }

    #[test]
    fn pow_invalid_is_default() {
        // (-1) ** 0.5 is NaN → bottom → 0.
        assert_eq!(evb("(0 - 1) ** 0.5"), Value::Int(0));
    }

    // ---- Short-circuit && / || (AC3) ----

    #[test]
    fn logical_and_short_circuits() {
        // RHS divides by zero; if it were evaluated it would still be 0/bottom,
        // so to *prove* laziness we use a counting trigger via a side-effecting
        // context is overkill — instead assert the documented result and that a
        // false LHS yields 0 without consulting the RHS value.
        let ctx = MockContext::new().with_trigger("rhs", &[], Value::Int(1));
        // false && rhs == 0
        assert_eq!(ev("0 && rhs", &ctx), Value::Int(0));
        // true && rhs == 1
        assert_eq!(ev("1 && rhs", &ctx), Value::Int(1));
    }

    #[test]
    fn logical_or_short_circuits() {
        let ctx = MockContext::new().with_trigger("rhs", &[], Value::Int(0));
        // true || rhs == 1 (rhs not needed)
        assert_eq!(ev("1 || rhs", &ctx), Value::Int(1));
        // false || rhs == 0
        assert_eq!(ev("0 || rhs", &ctx), Value::Int(0));
    }

    #[test]
    fn short_circuit_avoids_rhs_evaluation() {
        // Prove laziness with an observable side effect: a context that counts
        // how many times the `probe` trigger is read.
        struct CountingCtx {
            reads: Cell<u32>,
        }
        impl EvalContext for CountingCtx {
            fn trigger(&self, name: &str, _args: &[Value]) -> Value {
                if name.eq_ignore_ascii_case("probe") {
                    self.reads.set(self.reads.get() + 1);
                    return Value::Int(1);
                }
                Value::DEFAULT
            }
        }
        // `0 && probe` must NOT read `probe`.
        let ctx = CountingCtx {
            reads: Cell::new(0),
        };
        assert_eq!(eval(&parse_str("0 && probe").unwrap(), &ctx), Value::Int(0));
        assert_eq!(
            ctx.reads.get(),
            0,
            "&& should short-circuit and not read probe"
        );

        // `1 || probe` must NOT read `probe`.
        let ctx2 = CountingCtx {
            reads: Cell::new(0),
        };
        assert_eq!(
            eval(&parse_str("1 || probe").unwrap(), &ctx2),
            Value::Int(1)
        );
        assert_eq!(
            ctx2.reads.get(),
            0,
            "|| should short-circuit and not read probe"
        );

        // `1 && probe` MUST read it once.
        let ctx3 = CountingCtx {
            reads: Cell::new(0),
        };
        assert_eq!(
            eval(&parse_str("1 && probe").unwrap(), &ctx3),
            Value::Int(1)
        );
        assert_eq!(ctx3.reads.get(), 1, "&& must evaluate RHS when LHS is true");
    }

    // ---- Comparison chains (AC3) ----

    #[test]
    fn comparisons_return_one_or_zero() {
        assert_eq!(evb("3 < 5"), Value::Int(1));
        assert_eq!(evb("5 < 3"), Value::Int(0));
        assert_eq!(evb("4 >= 4"), Value::Int(1));
        assert_eq!(evb("4 = 4"), Value::Int(1));
        assert_eq!(evb("4 != 4"), Value::Int(0));
    }

    #[test]
    fn comparison_chain_is_left_associative() {
        // `1 < 2 < 3` parses as `(1 < 2) < 3` = `1 < 3` = 1 (MUGEN/C semantics).
        assert_eq!(evb("1 < 2 < 3"), Value::Int(1));
        // `3 < 2 < 1` = `(3<2) < 1` = `0 < 1` = 1.
        assert_eq!(evb("3 < 2 < 1"), Value::Int(1));
    }

    #[test]
    fn comparison_promotes_to_float() {
        assert_eq!(evb("3 < 3.5"), Value::Int(1));
        assert_eq!(evb("3.5 = 3.5"), Value::Int(1));
    }

    // ---- Bitwise (AC3) ----

    #[test]
    fn bitwise_ops_on_ints() {
        assert_eq!(evb("6 & 3"), Value::Int(2));
        assert_eq!(evb("6 | 1"), Value::Int(7));
        assert_eq!(evb("6 ^ 3"), Value::Int(5));
        assert_eq!(evb("~0"), Value::Int(-1));
    }

    #[test]
    fn bitwise_narrows_float_operand() {
        // 6.9 narrows (saturating to_int, trunc toward zero) to 6, then 6 & 3 = 2.
        assert_eq!(evb("6.9 & 3"), Value::Int(2));
    }

    // ---- Logical NOT / truthiness ----

    #[test]
    fn logical_not() {
        assert_eq!(evb("!0"), Value::Int(1));
        assert_eq!(evb("!5"), Value::Int(0));
        assert_eq!(evb("!0.0"), Value::Int(1));
        assert_eq!(evb("!0.001"), Value::Int(0));
    }

    // ---- Ranges: all four forms + != (AC4) ----

    #[test]
    fn range_inclusive_both() {
        // x = [3,6]
        let ctx = MockContext::new().with_trigger("x", &[], Value::Int(3));
        assert_eq!(ev("x = [3,6]", &ctx), Value::Int(1));
        let ctx6 = MockContext::new().with_trigger("x", &[], Value::Int(6));
        assert_eq!(ev("x = [3,6]", &ctx6), Value::Int(1));
        let ctx7 = MockContext::new().with_trigger("x", &[], Value::Int(7));
        assert_eq!(ev("x = [3,6]", &ctx7), Value::Int(0));
    }

    #[test]
    fn range_exclusive_both() {
        // 3 = (3,6) is false (exclusive lower); 4 = (3,6) is true.
        assert_eq!(evb("3 = (3,6)"), Value::Int(0));
        assert_eq!(evb("4 = (3,6)"), Value::Int(1));
        assert_eq!(evb("6 = (3,6)"), Value::Int(0));
    }

    #[test]
    fn range_half_open_forms() {
        // [3,6): includes 3, excludes 6.
        assert_eq!(evb("3 = [3,6)"), Value::Int(1));
        assert_eq!(evb("6 = [3,6)"), Value::Int(0));
        // (3,6]: excludes 3, includes 6.
        assert_eq!(evb("3 = (3,6]"), Value::Int(0));
        assert_eq!(evb("6 = (3,6]"), Value::Int(1));
    }

    #[test]
    fn range_not_equal_negates_membership() {
        // 4 != [3,6] is false (4 is in range); 7 != [3,6] is true.
        assert_eq!(evb("4 != [3,6]"), Value::Int(0));
        assert_eq!(evb("7 != [3,6]"), Value::Int(1));
        // Negation of half-open: 6 != [3,6) is true (6 not in [3,6)).
        assert_eq!(evb("6 != [3,6)"), Value::Int(1));
    }

    #[test]
    fn range_float_promotion() {
        // Any float endpoint promotes the comparison; 3.5 = [3,4] is true.
        assert_eq!(evb("3.5 = [3,4]"), Value::Int(1));
        assert_eq!(evb("3.5 = (3.5,4]"), Value::Int(0));
    }

    // ---- cond / ifelse laziness (AC5) ----

    #[test]
    fn cond_selects_taken_branch() {
        assert_eq!(evb("cond(1, 10, 20)"), Value::Int(10));
        assert_eq!(evb("cond(0, 10, 20)"), Value::Int(20));
        assert_eq!(evb("ifelse(1, 1.5, 2.5)"), Value::Float(1.5));
    }

    #[test]
    fn cond_is_lazy_only_taken_branch_evaluated() {
        // The untaken branch divides by zero; if it were evaluated eagerly the
        // whole expression would still be 0 only by accident — instead use a
        // counting context to prove the untaken branch's trigger is never read.
        struct CountingCtx {
            reads: Cell<u32>,
        }
        impl EvalContext for CountingCtx {
            fn trigger(&self, name: &str, _args: &[Value]) -> Value {
                if name.eq_ignore_ascii_case("untaken") {
                    self.reads.set(self.reads.get() + 1);
                }
                Value::Int(1)
            }
        }
        // cond(1, 42, untaken) → 42, never touching `untaken`.
        let ctx = CountingCtx {
            reads: Cell::new(0),
        };
        assert_eq!(
            eval(&parse_str("cond(1, 42, untaken)").unwrap(), &ctx),
            Value::Int(42)
        );
        assert_eq!(
            ctx.reads.get(),
            0,
            "cond must not evaluate the untaken branch"
        );

        // cond(0, untaken, 7) → 7, never touching `untaken`.
        let ctx2 = CountingCtx {
            reads: Cell::new(0),
        };
        assert_eq!(
            eval(&parse_str("cond(0, untaken, 7)").unwrap(), &ctx2),
            Value::Int(7)
        );
        assert_eq!(
            ctx2.reads.get(),
            0,
            "cond must not evaluate the untaken branch"
        );
    }

    // ---- Math functions (AC5) ----

    #[test]
    fn abs_is_type_preserving() {
        assert_eq!(evb("abs(0 - 5)"), Value::Int(5));
        assert_eq!(evb("abs(5)"), Value::Int(5));
        assert_eq!(evb("abs(0.0 - 2.5)"), Value::Float(2.5));
    }

    #[test]
    fn floor_and_ceil_return_int() {
        assert_eq!(evb("floor(3.7)"), Value::Int(3));
        assert_eq!(evb("floor(0.0 - 3.2)"), Value::Int(-4));
        assert_eq!(evb("ceil(3.2)"), Value::Int(4));
        assert_eq!(evb("ceil(0.0 - 3.7)"), Value::Int(-3));
        // Int arg passes through unchanged.
        assert_eq!(evb("floor(5)"), Value::Int(5));
        assert_eq!(evb("ceil(5)"), Value::Int(5));
    }

    #[test]
    fn min_max_are_type_preserving() {
        assert_eq!(evb("min(3, 7)"), Value::Int(3));
        assert_eq!(evb("max(3, 7)"), Value::Int(7));
        // Any float operand → float result.
        assert_eq!(evb("min(3, 7.0)"), Value::Float(3.0));
        assert_eq!(evb("max(3.5, 2)"), Value::Float(3.5));
    }

    #[test]
    fn trig_exp_ln_return_floats() {
        // sin(0)=0, cos(0)=1, atan(0)=0, exp(0)=1, ln(1)=0.
        assert_eq!(evb("sin(0)"), Value::Float(0.0));
        assert_eq!(evb("cos(0)"), Value::Float(1.0));
        assert_eq!(evb("atan(0)"), Value::Float(0.0));
        assert_eq!(evb("exp(0)"), Value::Float(1.0));
        assert_eq!(evb("ln(1)"), Value::Float(0.0));
    }

    #[test]
    fn ln_of_nonpositive_is_default() {
        assert_eq!(evb("ln(0)"), Value::Int(0));
        assert_eq!(evb("ln(0.0 - 1.0)"), Value::Int(0));
    }

    #[test]
    fn random_is_deterministic_and_bounded() {
        // Bare `random` → [0, 999], deterministic for a fixed seed.
        let ctx_a = MockContext::new().with_seed(99);
        let ctx_b = MockContext::new().with_seed(99);
        let a = ev("random", &ctx_a);
        let b = ev("random", &ctx_b);
        assert_eq!(a, b);
        if let Value::Int(i) = a {
            assert!((0..=999).contains(&i));
        } else {
            panic!("random should be int");
        }
        // random(lo, hi) → inclusive range.
        let ctx_c = MockContext::new().with_seed(99);
        let v = ev("random(10, 20)", &ctx_c);
        if let Value::Int(i) = v {
            assert!((10..=20).contains(&i));
        } else {
            panic!("random(lo,hi) should be int");
        }
    }

    // ---- var / fvar / sysvar lookups (AC5) ----

    #[test]
    fn variable_bank_lookups() {
        let ctx = MockContext::new()
            .with_var(0, 42)
            .with_fvar(1, 3.5)
            .with_sysvar(2, 7);
        assert_eq!(ev("var(0)", &ctx), Value::Int(42));
        assert_eq!(ev("fvar(1)", &ctx), Value::Float(3.5));
        assert_eq!(ev("sysvar(2)", &ctx), Value::Int(7));
        // Unset → safe default.
        assert_eq!(ev("var(99)", &ctx), Value::Int(0));
    }

    #[test]
    fn variable_index_is_an_expression() {
        // The index can itself be an expression: var(1 + 1) reads var(2).
        let ctx = MockContext::new().with_var(2, 123);
        assert_eq!(ev("var(1 + 1)", &ctx), Value::Int(123));
    }

    // ---- Trigger refs (AC5): Ident + parameterized Call with args ----

    #[test]
    fn bare_trigger_ref() {
        let ctx = MockContext::new().with_trigger("Time", &[], Value::Int(30));
        assert_eq!(ev("Time", &ctx), Value::Int(30));
        // Unknown trigger → 0.
        assert_eq!(ev("NoSuchTrigger", &ctx), Value::Int(0));
    }

    #[test]
    fn parameterized_trigger_ref_with_evaluated_args() {
        // A real trigger-with-args case: the args are evaluated before lookup, so
        // `AnimElemTime(1 + 1)` looks up with `[Int(2)]`.
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(2)], Value::Int(5));
        assert_eq!(ev("AnimElemTime(1 + 1)", &ctx), Value::Int(5));
        // Different (unregistered) arg → default.
        assert_eq!(ev("AnimElemTime(9)", &ctx), Value::Int(0));
    }

    // ---- The keystone integration case from the task brief ----

    #[test]
    fn keystone_trigger_expression() {
        // `AnimElem = 6 && P2BodyDist X < 30` — the payoff example. The
        // axis-suffixed `P2BodyDist X` form is deferred to a later task (not in
        // the current AST), so we exercise the structurally equivalent
        // `AnimElem = 6 && dist < 30`, modeling the distance as one trigger read.
        let ctx = MockContext::new()
            .with_trigger("AnimElem", &[], Value::Int(6))
            .with_trigger("dist", &[], Value::Float(12.0));
        assert_eq!(ev("AnimElem = 6 && dist < 30", &ctx), Value::Int(1));
        // When AnimElem != 6 the whole thing is false (and short-circuits).
        let ctx2 = MockContext::new()
            .with_trigger("AnimElem", &[], Value::Int(3))
            .with_trigger("dist", &[], Value::Float(12.0));
        assert_eq!(ev("AnimElem = 6 && dist < 30", &ctx2), Value::Int(0));
    }

    // ---- Precedence sanity (ties the parser + evaluator together) ----

    #[test]
    fn precedence_pow_above_mul_above_add() {
        // 1 + 2 * 3 ** 2 == 1 + 2 * 9 == 19.
        assert_eq!(evb("1 + 2 * 3 ** 2"), Value::Int(19));
        // Unary minus binds tighter than **? In MUGEN `-` is below `**`, so
        // -2 ** 2 is -(2 ** 2) = -4 per the precedence table. Confirm parse+eval.
        // (Parser places unary above **, see parser docs; just assert a stable
        // result that the pipeline produces.)
        let v = evb("0 - 2 ** 2");
        assert_eq!(v, Value::Int(-4));
    }

    // ---- Never-panic on pathological input ----

    #[test]
    fn nested_errors_collapse_to_default() {
        // A pile of error-producing ops nested together must still yield 0.
        assert_eq!(evb("(1 / 0) + (5 % 0) * ln(0)"), Value::Int(0));
        assert_eq!(evb("floor(ln(0))"), Value::Int(0));
    }

    // ---- Redirection (task 4.8) ----

    #[test]
    fn redirect_evaluates_against_target_context() {
        // `root, life` reads Life from the redirected (root) context, NOT self.
        let root = MockContext::new().with_trigger("Life", &[], Value::Int(1000));
        let ctx = MockContext::new()
            .with_trigger("Life", &[], Value::Int(400))
            .with_redirect(Redirect::Root, root);
        assert_eq!(ev("life", &ctx), Value::Int(400));
        assert_eq!(ev("root, life", &ctx), Value::Int(1000));
    }

    #[test]
    fn redirect_each_keyword_resolves() {
        // Wire one child per keyword and confirm the redirected read returns its
        // value. Covers parent/root/partner/enemy/enemynear/target/helper/playerid.
        let mk = |n: i32| MockContext::new().with_trigger("X", &[], Value::Int(n));
        let ctx = MockContext::new()
            .with_redirect(Redirect::Parent, mk(1))
            .with_redirect(Redirect::Root, mk(2))
            .with_redirect(Redirect::Partner, mk(3))
            .with_redirect(Redirect::Enemy, mk(4))
            .with_redirect(Redirect::EnemyNear(1), mk(5))
            .with_redirect(Redirect::Target(Some(2)), mk(6))
            .with_redirect(Redirect::Helper(1234), mk(7))
            .with_redirect(Redirect::PlayerId(7), mk(8));
        assert_eq!(ev("parent, x", &ctx), Value::Int(1));
        assert_eq!(ev("root, x", &ctx), Value::Int(2));
        assert_eq!(ev("partner, x", &ctx), Value::Int(3));
        assert_eq!(ev("enemy, x", &ctx), Value::Int(4));
        assert_eq!(ev("enemynear(1), x", &ctx), Value::Int(5));
        assert_eq!(ev("target(2), x", &ctx), Value::Int(6));
        assert_eq!(ev("helper(1234), x", &ctx), Value::Int(7));
        assert_eq!(ev("playerid(7), x", &ctx), Value::Int(8));
    }

    #[test]
    fn cb8_enemy_index_resolves_through_enemynear() {
        // CB8: `enemy(2)` lowers to EnemyNear(2); `enemy`/`enemy(0)` use Enemy.
        let ctx = MockContext::new()
            .with_redirect(
                Redirect::Enemy,
                MockContext::new().with_trigger("X", &[], Value::Int(10)),
            )
            .with_redirect(
                Redirect::EnemyNear(2),
                MockContext::new().with_trigger("X", &[], Value::Int(20)),
            );
        assert_eq!(ev("enemy, x", &ctx), Value::Int(10));
        assert_eq!(ev("enemy(0), x", &ctx), Value::Int(10));
        assert_eq!(ev("enemy(2), x", &ctx), Value::Int(20));
    }

    #[test]
    fn redirect_applies_to_whole_sub_expression() {
        // The redirect retargets the entire trailing expression.
        let enemy = MockContext::new()
            .with_trigger("life", &[], Value::Int(50))
            .with_trigger("power", &[], Value::Int(3000));
        let ctx = MockContext::new().with_redirect(Redirect::Enemy, enemy);
        // `enemy, life + 100` → 150 (life from enemy, +100 in arithmetic).
        assert_eq!(ev("enemy, life + 100", &ctx), Value::Int(150));
        // A compound boolean is evaluated entirely against the enemy.
        assert_eq!(ev("enemy, life < 100 && power > 1000", &ctx), Value::Int(1));
    }

    #[test]
    fn nested_redirect_chain_evaluates() {
        // `enemy, helper(1), x` walks two hops before reading x.
        let leaf = MockContext::new().with_trigger("x", &[], Value::Int(99));
        let enemy = MockContext::new().with_redirect(Redirect::Helper(1), leaf);
        let ctx = MockContext::new().with_redirect(Redirect::Enemy, enemy);
        assert_eq!(ev("enemy, helper(1), x", &ctx), Value::Int(99));
    }

    #[test]
    fn missing_redirect_target_is_default_zero() {
        // No relations registered → the redirect resolves to None → bottom → 0,
        // never a panic. Confirmed across keyword forms.
        let ctx = MockContext::new();
        assert_eq!(ev("enemy, life", &ctx), Value::Int(0));
        assert_eq!(ev("parent, stateno", &ctx), Value::Int(0));
        assert_eq!(ev("helper(1234), anim", &ctx), Value::Int(0));
        // A missing FIRST hop in a chain also collapses the whole chain to 0.
        assert_eq!(ev("enemy, helper(1), x", &ctx), Value::Int(0));
    }

    #[test]
    fn missing_redirect_is_falsey_in_boolean_context() {
        // A None redirect is bottom (falsey), so `&&` short-circuits to 0 and the
        // redirected read never fires (an unresolved redirect never fires).
        let ctx = MockContext::new();
        assert_eq!(ev("enemy, life > 0 && 1", &ctx), Value::Int(0));
        // ...but it is falsey, so `||` falls through to the (self) RHS.
        assert_eq!(ev("(enemy, life) || 1", &ctx), Value::Int(1));
    }

    #[test]
    fn malformed_redirect_evaluates_to_default_zero() {
        // A malformed redirection is a parse error, which the public pipeline
        // (parse → eval, the bad-expr → 0 contract) maps to 0. Here we make the
        // contract explicit: a parse error yields the safe default.
        let ctx = MockContext::new();
        for src in ["enemy,", "playerid, x", "parent(1), x"] {
            let v = match parse_str(src) {
                Ok(ast) => eval(&ast, &ctx),
                Err(_) => Value::DEFAULT, // CNS layer's bad-expression → 0
            };
            assert_eq!(v, Value::Int(0), "malformed redirect {src:?} should be 0");
        }
    }

    // =====================================================================
    // Proctor (task 4.4): additional edge-case / error-path / MUGEN-semantics
    // coverage layered on top of Forge's evaluator tests. Grouped by the
    // acceptance criterion each block exercises. No impl code is modified —
    // these are pure tests against the public `eval` + `MockContext`.
    // =====================================================================

    // ---- AC1: public eval boundary — Bottom never escapes; Str/Range atoms ----

    #[test]
    fn standalone_string_atom_collapses_to_default() {
        // A bare string literal has no numeric meaning standalone → Bottom → 0
        // at the public boundary (never a panic, never an exposed Bottom).
        assert_eq!(evb("\"hello\""), Value::Int(0));
    }

    #[test]
    fn standalone_range_atom_collapses_to_default() {
        // A range only has meaning as the RHS of = / != . As a top-level atom
        // (the parser permits `(a,b)` to parse) the evaluator rejects it → 0.
        assert_eq!(evb("(1, 5)"), Value::Int(0));
        assert_eq!(evb("[1, 5]"), Value::Int(0));
    }

    #[test]
    fn eval_only_ever_returns_int_or_float_never_bottom() {
        // Sweep a spread of error-producing inputs and confirm the public API
        // hands back a concrete Value every time (the internal Bottom sentinel
        // is fully encapsulated).
        for src in [
            "1 / 0",
            "5 % 0",
            "ln(0)",
            "(0 - 1) ** 0.5",
            "\"x\"",
            "(1,2)",
            "5.0 % 2",
            "floor(ln(0))",
            "(1 / 0) + 1",
            "abs(ln(0))",
        ] {
            let v = evb(src);
            assert!(
                matches!(v, Value::Int(_) | Value::Float(_)),
                "eval({src:?}) leaked a non-Value: {v:?}"
            );
        }
    }

    // ---- AC2: arithmetic promotion in both operand orders ----

    #[test]
    fn promotion_holds_for_int_op_float_and_float_op_int() {
        // Promotion must be symmetric: whichever side is float promotes.
        assert_eq!(evb("3 - 1.5"), Value::Float(1.5));
        assert_eq!(evb("1.5 - 3"), Value::Float(-1.5));
        assert_eq!(evb("4 * 0.5"), Value::Float(2.0));
        assert_eq!(evb("0.5 * 4"), Value::Float(2.0));
        // Float division when the LEFT operand is the float (mirror of the
        // existing `7 / 2.0` which has the float on the right).
        assert_eq!(evb("7.0 / 2"), Value::Float(3.5));
    }

    #[test]
    fn int_mul_wraps_on_overflow() {
        // Native i32 `*` wraps (two's complement) per §4 — not saturating, not a
        // debug panic. 100000 * 100000 = 1e10 which overflows i32.
        let expected = 100_000i32.wrapping_mul(100_000);
        assert_eq!(evb("100000 * 100000"), Value::Int(expected));
    }

    #[test]
    fn int_subtraction_wraps_at_min() {
        // i32::MIN - 1 wraps to i32::MAX. i32::MIN can't be a literal (it
        // saturates), so inject it via a var bank.
        let ctx = MockContext::new().with_var(0, i32::MIN);
        assert_eq!(ev("var(0) - 1", &ctx), Value::Int(i32::MAX));
    }

    #[test]
    fn bottom_poisons_arithmetic_result() {
        // A divide-by-zero (Bottom) anywhere in an arithmetic chain poisons the
        // whole result to Bottom → 0, even when combined with valid operands.
        assert_eq!(evb("(1 / 0) + 100"), Value::Int(0));
        assert_eq!(evb("100 + (1 / 0)"), Value::Int(0));
        assert_eq!(evb("(1 / 0) * 100"), Value::Int(0));
        // A float-side bottom likewise poisons.
        assert_eq!(evb("(1.0 / 0.0) + 2.5"), Value::Int(0));
    }

    #[test]
    fn float_arith_nan_funnels_to_bottom_zero() {
        // Audit #19: `+ - *` funnel a NaN float result to bottom → 0, matching
        // `**`/`ln`/transcendentals. `0 * (huge literal that overflows f32 to
        // -inf)` is `0 * -inf == NaN`; previously this leaked a public
        // Value::Float(NaN), now it is the safe default 0.
        let v = evb("0 * -5.8751624776690396e128");
        assert_eq!(v, Value::Int(0));
        assert!(
            !matches!(v, Value::Float(f) if f.is_nan()),
            "no public NaN may escape"
        );
        // The mirror case via the trigger seam (unknown trigger → Int(0)).
        let ctx = MockContext::new();
        assert_eq!(ev("A * -5.8751624776690396e128", &ctx), Value::Int(0));
    }

    #[test]
    fn float_arith_infinity_funnels_to_bottom_zero() {
        // An overflowing float `*`/`+`/`-` result is non-finite (±inf) and funnels
        // to bottom → 0, so no public infinity escapes ordinary `+ - *`.
        // 1e38 is finite in f32 (f32::MAX ≈ 3.4e38) but 1e38 * 1e38 == +inf.
        assert_eq!(evb("1e38 * 1e38"), Value::Int(0));
        // A literal beyond f32::MAX already overflows to +inf at the Float node;
        // adding to it keeps it non-finite → 0. (`+` only overflows once a sum
        // exceeds f32::MAX, so use an already-infinite operand.)
        assert_eq!(evb("1e40 + 1.0"), Value::Int(0));
        // The literal itself overflowing to -inf, summed, stays non-finite → 0.
        assert_eq!(evb("-1e40 - 1.0"), Value::Int(0));
        // Sanity: an in-range float `+` stays finite and unaffected.
        assert_eq!(evb("1e38 + 1e38"), Value::Float(2e38));
        // Sanity: an in-range float `*` is unaffected and stays Float.
        assert_eq!(evb("2.5 * 4.0"), Value::Float(10.0));
    }

    #[test]
    fn eval_never_returns_public_nan_from_arith() {
        // The public-boundary guarantee: a NaN-producing arithmetic expression
        // collapses to a finite Int, never a public Value::Float(NaN).
        for src in [
            "0 * -5.8751624776690396e128",
            "(0.0) * 5e128",
            "1e38 * 1e38 - 1e38 * 1e38", // (+inf) - (+inf) chain, every step non-finite
        ] {
            let v = evb(src);
            assert!(
                !matches!(v, Value::Float(f) if !f.is_finite()),
                "{src} must not yield a public non-finite float, got {v:?}"
            );
        }
    }

    #[test]
    fn negative_dividend_float_division_keeps_sign() {
        // Float division is true division, not truncating — confirm sign & value.
        assert_eq!(evb("0.0 - 7.0 / 2.0"), Value::Float(-3.5));
        assert_eq!(evb("(0.0 - 7.0) / 2.0"), Value::Float(-3.5));
    }

    // ---- AC2: modulo — additional sign / float-poison cases ----

    #[test]
    fn modulo_exact_division_is_zero() {
        assert_eq!(evb("6 % 3"), Value::Int(0));
        assert_eq!(evb("0 % 5"), Value::Int(0));
    }

    #[test]
    fn modulo_both_negative() {
        // -7 % -3 takes the sign of the dividend (C semantics) → -1.
        assert_eq!(evb("-7 % -3"), Value::Int(-1));
    }

    // ---- AC2: exponentiation — extra type/edge cases ----

    #[test]
    fn pow_one_and_identity_exponents() {
        assert_eq!(evb("1 ** 100"), Value::Int(1));
        assert_eq!(evb("7 ** 1"), Value::Int(7));
        assert_eq!(evb("0 ** 5"), Value::Int(0));
    }

    #[test]
    fn pow_float_base_with_int_exponent_is_float() {
        // Any float operand → float path even with a non-negative int exponent.
        assert_eq!(evb("2.0 ** 3"), Value::Float(8.0));
        assert_eq!(evb("3.0 ** 2"), Value::Float(9.0));
    }

    #[test]
    fn pow_zero_to_negative_is_infinity_not_bottom() {
        // 0 ** -1 promotes to float; powf(0, -1) == +inf (finite-ish, NOT NaN),
        // so it is NOT an invalid exponentiation → it stays Float(inf), not 0.
        // This pins the documented "negative exponent ⇒ float pow" path and the
        // NaN-only bottom rule (§3).
        assert_eq!(evb("0 ** -1"), Value::Float(f32::INFINITY));
    }

    #[test]
    fn pow_negative_base_fractional_exp_is_bottom() {
        // (-8) ** (1.0/3.0) is NaN in powf (no real cube root via pow) → bottom → 0.
        assert_eq!(evb("(0 - 8) ** (1.0 / 3.0)"), Value::Int(0));
    }

    #[test]
    fn pow_bottom_operand_propagates() {
        // A bottom base or exponent makes the whole power bottom → 0.
        assert_eq!(evb("(1 / 0) ** 2"), Value::Int(0));
        assert_eq!(evb("2 ** (1 / 0)"), Value::Int(0));
    }

    // ---- AC3: every comparison operator individually ----

    #[test]
    fn each_comparison_operator_true_and_false() {
        // Le and Gt are not directly exercised by the existing tests; pin all six.
        assert_eq!(evb("3 <= 3"), Value::Int(1));
        assert_eq!(evb("4 <= 3"), Value::Int(0));
        assert_eq!(evb("5 > 3"), Value::Int(1));
        assert_eq!(evb("3 > 5"), Value::Int(0));
        assert_eq!(evb("3 >= 4"), Value::Int(0));
        assert_eq!(evb("3 < 3"), Value::Int(0));
        assert_eq!(evb("5 != 3"), Value::Int(1));
        assert_eq!(evb("5 = 3"), Value::Int(0));
    }

    #[test]
    fn comparison_bottom_operand_collapses_to_default() {
        // A comparison with a bottom side is bottom → 0 (an erroring compare
        // never fires), distinct from a plain false `0`. We can only observe the
        // collapsed public value, which must be 0.
        assert_eq!(evb("(1 / 0) < 5"), Value::Int(0));
        assert_eq!(evb("5 > (1 / 0)"), Value::Int(0));
    }

    #[test]
    fn comparison_mixed_promotion_both_directions() {
        assert_eq!(evb("3.5 > 3"), Value::Int(1));
        assert_eq!(evb("3 < 3.5"), Value::Int(1));
        assert_eq!(evb("4.0 = 4"), Value::Int(1));
        assert_eq!(evb("4 = 4.0"), Value::Int(1));
    }

    // ---- AC3: && / || value-vs-truthiness + bottom LHS ----

    #[test]
    fn logical_ops_coerce_floats_via_nonzero() {
        // Nonzero float is truthy; 0.0 is falsey. Result is int 1/0.
        assert_eq!(evb("0.5 && 1"), Value::Int(1));
        assert_eq!(evb("0.0 && 1"), Value::Int(0));
        assert_eq!(evb("0.0 || 2.5"), Value::Int(1));
        assert_eq!(evb("0.0 || 0.0"), Value::Int(0));
    }

    #[test]
    fn logical_and_with_bottom_lhs_is_false_without_touching_rhs() {
        // A bottom LHS is falsey, so `&&` short-circuits to 0 and never reads RHS.
        struct CountingCtx {
            reads: Cell<u32>,
        }
        impl EvalContext for CountingCtx {
            fn trigger(&self, name: &str, _args: &[Value]) -> Value {
                if name.eq_ignore_ascii_case("probe") {
                    self.reads.set(self.reads.get() + 1);
                }
                Value::Int(1)
            }
        }
        let ctx = CountingCtx {
            reads: Cell::new(0),
        };
        // `(1/0)` is Bottom → falsey → short-circuit.
        assert_eq!(
            eval(&parse_str("(1 / 0) && probe").unwrap(), &ctx),
            Value::Int(0)
        );
        assert_eq!(
            ctx.reads.get(),
            0,
            "&& must not read RHS when LHS is bottom/false"
        );
    }

    #[test]
    fn logical_or_with_bottom_lhs_falls_through_to_rhs() {
        // A bottom LHS is falsey, so `||` must evaluate the RHS.
        assert_eq!(evb("(1 / 0) || 1"), Value::Int(1));
        assert_eq!(evb("(1 / 0) || 0"), Value::Int(0));
    }

    #[test]
    fn logical_and_returns_one_when_rhs_truthy_value_not_passed_through() {
        // `&&` yields int 1/0, NOT the RHS value itself (unlike some languages).
        assert_eq!(evb("1 && 5"), Value::Int(1));
        assert_eq!(evb("1 && 0"), Value::Int(0));
        assert_eq!(evb("2 || 0"), Value::Int(1));
    }

    // ---- AC3: bitwise — XOR/OR/NOT values & float narrowing on both sides ----

    #[test]
    fn bitwise_not_of_nonzero() {
        // ~5 == -6 (two's complement).
        assert_eq!(evb("~5"), Value::Int(-6));
        assert_eq!(evb("~(0 - 1)"), Value::Int(0)); // ~(-1) == 0
    }

    #[test]
    fn bitwise_narrows_float_on_either_side() {
        // The float operand can be on either side; both narrow via to_int.
        assert_eq!(evb("3 & 6.9"), Value::Int(2));
        assert_eq!(evb("6.9 | 1"), Value::Int(7));
        // ~ on a float operand narrows then complements: ~6.9 == ~6 == -7.
        assert_eq!(evb("~6.9"), Value::Int(-7));
    }

    #[test]
    fn bitwise_bottom_operand_propagates() {
        assert_eq!(evb("(1 / 0) & 7"), Value::Int(0));
        assert_eq!(evb("~(1 / 0)"), Value::Int(0));
    }

    // ---- AC3: unary neg type-preservation & wrapping ----

    #[test]
    fn unary_neg_preserves_type() {
        assert_eq!(evb("-5"), Value::Int(-5));
        assert_eq!(evb("-5.5"), Value::Float(-5.5));
        // Double negation.
        assert_eq!(evb("--5"), Value::Int(5));
    }

    #[test]
    fn unary_neg_of_int_min_wraps_to_itself() {
        // -(i32::MIN) is unrepresentable; wrapping_neg keeps it at i32::MIN
        // rather than panicking. Inject MIN via a var (it can't be a literal).
        let ctx = MockContext::new().with_var(0, i32::MIN);
        assert_eq!(ev("-var(0)", &ctx), Value::Int(i32::MIN));
    }

    #[test]
    fn unary_not_on_bottom_is_one() {
        // bottom is falsey, so `!bottom` == 1 (per the impl's documented rule).
        assert_eq!(evb("!(1 / 0)"), Value::Int(1));
    }

    // ---- AC4: ranges — bottom propagation, float-mixed bounds, != half-open ----

    #[test]
    fn range_with_bottom_member_or_bound_collapses() {
        // If x, a, or b is bottom the whole range check is bottom → 0
        // (bottom-propagating, matching Ikemen's rangeCheck, §6).
        assert_eq!(evb("(1 / 0) = [0, 10]"), Value::Int(0));
        assert_eq!(evb("5 = [(1 / 0), 10]"), Value::Int(0));
        assert_eq!(evb("5 = [0, (1 / 0)]"), Value::Int(0));
    }

    #[test]
    fn range_member_is_an_arbitrary_expression() {
        // The left side of a range test is a full expression; `2 + 2 = [3,5]`.
        assert_eq!(evb("2 + 2 = [3, 5]"), Value::Int(1));
        assert_eq!(evb("2 + 2 = [5, 9]"), Value::Int(0));
    }

    #[test]
    fn range_ne_all_four_bound_forms() {
        // `!=` is the negation of `=` for every bound form (§6).
        // [3,6]: 6 is in → != is 0; 7 out → != is 1.
        assert_eq!(evb("6 != [3,6]"), Value::Int(0));
        // (3,6): 6 excluded → not a member → != is 1.
        assert_eq!(evb("6 != (3,6)"), Value::Int(1));
        // (3,6]: 6 included → member → != is 0.
        assert_eq!(evb("6 != (3,6]"), Value::Int(0));
        // [3,6): 3 included → member → != is 0; 6 excluded → != is 1.
        assert_eq!(evb("3 != [3,6)"), Value::Int(0));
    }

    #[test]
    fn range_negative_and_descending_bounds() {
        // Negative bounds work; a "backwards" range (lower > upper) is simply
        // never satisfied (no auto-swap in MUGEN range semantics).
        assert_eq!(evb("-3 = [-5, 0]"), Value::Int(1));
        assert_eq!(evb("5 = [10, 0]"), Value::Int(0));
    }

    #[test]
    fn range_member_float_against_int_bounds() {
        // A float member with int bounds promotes all three to float.
        assert_eq!(evb("2.5 = [2, 3]"), Value::Int(1));
        assert_eq!(evb("1.9 = [2, 3]"), Value::Int(0));
        assert_eq!(evb("3.0 = (2, 3]"), Value::Int(1));
    }

    // ---- AC5: cond / ifelse arity guard + value passthrough types ----

    #[test]
    fn cond_branch_value_type_is_preserved() {
        // The selected branch's type flows out unchanged (int or float).
        assert_eq!(evb("cond(1, 3, 4.0)"), Value::Int(3));
        assert_eq!(evb("cond(0, 3, 4.0)"), Value::Float(4.0));
        assert_eq!(evb("ifelse(0, 1, 2)"), Value::Int(2));
    }

    #[test]
    fn cond_condition_uses_truthiness() {
        // A nonzero float condition is true; negative is true; 0 is false.
        assert_eq!(evb("cond(0.5, 1, 2)"), Value::Int(1));
        assert_eq!(evb("cond(0 - 3, 1, 2)"), Value::Int(1));
        assert_eq!(evb("cond(0.0, 1, 2)"), Value::Int(2));
    }

    #[test]
    fn cond_wrong_arity_falls_through_to_trigger_lookup() {
        // `cond`/`ifelse` are only special-cased at arity 3. A 2-arg `cond(...)`
        // is not a builtin; it routes to the trigger seam (→ default 0 here).
        // This documents the dispatch fall-through, not a panic.
        assert_eq!(evb("cond(1, 2)"), Value::Int(0));
        // Registered as a trigger, it resolves through ctx.trigger with the
        // evaluated args (proving the args ARE evaluated for the fall-through).
        let ctx = MockContext::new().with_trigger(
            "cond",
            &[Value::Int(1), Value::Int(2)],
            Value::Int(77),
        );
        assert_eq!(ev("cond(1, 2)", &ctx), Value::Int(77));
    }

    // ---- AC5: math fns — arity guard, case-insensitivity, edge values ----

    #[test]
    fn math_function_names_are_case_insensitive() {
        assert_eq!(evb("ABS(0 - 5)"), Value::Int(5));
        assert_eq!(evb("Floor(3.7)"), Value::Int(3));
        assert_eq!(evb("MAX(3, 7)"), Value::Int(7));
        assert_eq!(evb("CoS(0)"), Value::Float(1.0));
        assert_eq!(evb("Cond(1, 8, 9)"), Value::Int(8));
    }

    #[test]
    fn abs_of_int_min_wraps_per_documented_edge() {
        // abs uses wrapping_abs, so abs(i32::MIN) stays i32::MIN (the one value
        // whose magnitude is unrepresentable). Pinned so a refactor to a panicking
        // abs would be caught. Inject MIN via a var.
        let ctx = MockContext::new().with_var(0, i32::MIN);
        assert_eq!(ev("abs(var(0))", &ctx), Value::Int(i32::MIN));
    }

    #[test]
    fn floor_ceil_saturate_huge_floats() {
        // floor/ceil narrow to i32 with saturation (CB4) — a huge float clamps
        // to i32::MAX rather than wrapping. 5e9 > i32::MAX.
        assert_eq!(evb("floor(5000000000.0)"), Value::Int(i32::MAX));
        assert_eq!(evb("ceil(0.0 - 5000000000.0)"), Value::Int(i32::MIN));
    }

    #[test]
    fn floor_ceil_on_already_integral_floats() {
        // An integral-valued float floors/ceils to itself.
        assert_eq!(evb("floor(4.0)"), Value::Int(4));
        assert_eq!(evb("ceil(4.0)"), Value::Int(4));
        assert_eq!(evb("floor(0.0 - 4.0)"), Value::Int(-4));
    }

    #[test]
    fn min_max_bottom_operand_collapses() {
        assert_eq!(evb("min(1 / 0, 5)"), Value::Int(0));
        assert_eq!(evb("max(5, 1 / 0)"), Value::Int(0));
    }

    #[test]
    fn min_max_with_negative_and_equal_args() {
        assert_eq!(evb("min(0 - 3, 0 - 7)"), Value::Int(-7));
        assert_eq!(evb("max(0 - 3, 0 - 7)"), Value::Int(-3));
        assert_eq!(evb("min(5, 5)"), Value::Int(5));
        // Mixed: any float promotes the result to float.
        assert_eq!(evb("max(0 - 3.5, 0 - 1)"), Value::Float(-1.0));
    }

    #[test]
    fn transcendental_bottom_operand_propagates() {
        // sin/cos/exp/atan of a bottom argument is bottom → 0.
        assert_eq!(evb("sin(1 / 0)"), Value::Int(0));
        assert_eq!(evb("exp(1 / 0)"), Value::Int(0));
        assert_eq!(evb("atan(1 / 0)"), Value::Int(0));
    }

    #[test]
    fn exp_and_ln_are_inverses_in_value() {
        // exp(ln(x)) ~= x for x>0; pin a clean integer-ish case loosely.
        if let Value::Float(f) = evb("exp(ln(5.0))") {
            assert!((f - 5.0).abs() < 1e-3, "exp(ln(5)) ~= 5, got {f}");
        } else {
            panic!("exp(ln(..)) must be float");
        }
    }

    #[test]
    fn ln_positive_argument_is_float() {
        // ln of a positive int promotes to float; ln(e) ~= 1.
        if let Value::Float(f) = evb("ln(2.718281828)") {
            assert!((f - 1.0).abs() < 1e-4, "ln(e) ~= 1, got {f}");
        } else {
            panic!("ln must return float for positive arg");
        }
    }

    // ---- AC5: var / fvar / sysvar — fall-through, arity, expression index ----

    #[test]
    fn fvar_sysvar_wrong_arity_falls_through_to_trigger() {
        // var/fvar/sysvar are special-cased only at arity 1. Other arities route
        // to the trigger seam (no panic).
        assert_eq!(evb("var()"), Value::Int(0));
        assert_eq!(evb("fvar(0, 1)"), Value::Int(0));
        // And a registered trigger of the same name+arity still resolves.
        let ctx = MockContext::new().with_trigger("var", &[], Value::Int(13));
        assert_eq!(ev("var()", &ctx), Value::Int(13));
    }

    #[test]
    fn fvar_index_is_an_expression() {
        let ctx = MockContext::new().with_fvar(3, 9.5);
        assert_eq!(ev("fvar(1 + 2)", &ctx), Value::Float(9.5));
    }

    #[test]
    fn var_default_methods_route_through_trigger_when_only_trigger_impl() {
        // A context overriding only `trigger` still answers var(i)/fvar(i)/sysvar(i)
        // through the default trait methods, all the way through the evaluator.
        struct TriggerOnly;
        impl EvalContext for TriggerOnly {
            fn trigger(&self, name: &str, args: &[Value]) -> Value {
                match (name.to_ascii_lowercase().as_str(), args) {
                    ("var", [Value::Int(0)]) => Value::Int(11),
                    ("fvar", [Value::Int(0)]) => Value::Float(2.5),
                    ("sysvar", [Value::Int(0)]) => Value::Int(99),
                    _ => Value::DEFAULT,
                }
            }
        }
        assert_eq!(
            eval(&parse_str("var(0)").unwrap(), &TriggerOnly),
            Value::Int(11)
        );
        assert_eq!(
            eval(&parse_str("fvar(0)").unwrap(), &TriggerOnly),
            Value::Float(2.5)
        );
        assert_eq!(
            eval(&parse_str("sysvar(0)").unwrap(), &TriggerOnly),
            Value::Int(99)
        );
    }

    // ---- AC5: trigger refs — case-insensitive, float-valued, multi-arg ----

    #[test]
    fn trigger_ref_is_case_insensitive_through_evaluator() {
        // MUGEN is case-insensitive; the evaluator passes the name verbatim and
        // the (mock) context matches case-insensitively.
        let ctx = MockContext::new().with_trigger("StateNo", &[], Value::Int(200));
        assert_eq!(ev("stateno", &ctx), Value::Int(200));
        assert_eq!(ev("STATENO", &ctx), Value::Int(200));
    }

    #[test]
    fn trigger_ref_returning_float_flows_through() {
        // A trigger that holds a float (e.g. PosX) flows into arithmetic and
        // promotes correctly.
        let ctx = MockContext::new().with_trigger("PosX", &[], Value::Float(-3.5));
        assert_eq!(ev("PosX", &ctx), Value::Float(-3.5));
        assert_eq!(ev("PosX + 1", &ctx), Value::Float(-2.5));
        assert_eq!(ev("PosX < 0", &ctx), Value::Int(1));
    }

    #[test]
    fn trigger_ref_with_multiple_evaluated_args() {
        // A multi-arg trigger: both args are evaluated (left-to-right) before the
        // lookup. Models e.g. `ProjHit(id, ...)`-shaped triggers.
        let ctx =
            MockContext::new().with_trigger("two", &[Value::Int(3), Value::Int(8)], Value::Int(55));
        assert_eq!(ev("two(1 + 2, 2 * 4)", &ctx), Value::Int(55));
    }

    #[test]
    fn unknown_trigger_participates_as_zero_in_expression() {
        // An unknown trigger defaults to 0 and that 0 flows through arithmetic /
        // comparison (the engine-wide "bad expression → 0" contract).
        let ctx = MockContext::new();
        assert_eq!(ev("Unknown + 5", &ctx), Value::Int(5));
        assert_eq!(ev("Unknown = 0", &ctx), Value::Int(1));
        assert_eq!(ev("Unknown && 1", &ctx), Value::Int(0));
    }

    // ---- AC5: random — arity guard, swapped bounds, determinism across draws ----

    #[test]
    fn random_call_form_zero_args_is_classic_range() {
        // `random()` (parened, zero args) behaves like the bare `random`: [0,999].
        let ctx = MockContext::new().with_seed(123);
        if let Value::Int(i) = ev("random()", &ctx) {
            assert!((0..=999).contains(&i));
        } else {
            panic!("random() should be int");
        }
    }

    #[test]
    fn random_swapped_bounds_are_handled() {
        // random(hi, lo) with hi > lo must not panic and must land in the range.
        let ctx = MockContext::new().with_seed(7);
        if let Value::Int(i) = ev("random(20, 10)", &ctx) {
            assert!((10..=20).contains(&i));
        } else {
            panic!("random(swapped) should be int");
        }
    }

    #[test]
    fn random_bad_arity_is_default() {
        // 1-arg or 3-arg random is malformed → bottom → 0 (never a panic).
        let ctx = MockContext::new().with_seed(7);
        assert_eq!(ev("random(5)", &ctx), Value::Int(0));
        assert_eq!(ev("random(1, 2, 3)", &ctx), Value::Int(0));
    }

    #[test]
    fn random_unseeded_context_defaults_to_zero_draw() {
        // The default/unseeded EvalContext::random returns 0, so a `random(lo,hi)`
        // call reads the bottom of its range deterministically.
        let ctx = MockContext::new(); // no seed → random() seam yields 0
        assert_eq!(ev("random(10, 20)", &ctx), Value::Int(10));
        assert_eq!(ev("random(0, 999)", &ctx), Value::Int(0));
    }

    #[test]
    fn random_call_form_advances_across_successive_draws() {
        // Two draws from the same seeded context via the parened `random(lo,hi)`
        // call form must (with overwhelming probability) differ — proving the RNG
        // state advances through the EvalContext::random seam (§11). The bare
        // `random` form advances the very same seam (see
        // `bare_random_draws_from_rng_seam`); this test pins the call form.
        let ctx = MockContext::new().with_seed(42);
        let a = ev("random(0, 1000000)", &ctx);
        let b = ev("random(0, 1000000)", &ctx);
        assert_ne!(
            a, b,
            "successive random(..) draws should advance the RNG state"
        );
    }

    #[test]
    fn bare_random_draws_from_rng_seam() {
        // Regression guard for a fixed semantics gap. Bare `random` (no parens)
        // parses as `Expr::Ident("random")`. The evaluator USED to route every
        // plain Ident straight to `ctx.trigger("random", &[])`, so the bare form
        // never reached `eval_random` / the `EvalContext::random` seam — it returned
        // the trigger default (0) instead of a [0,999] draw, even though classic
        // MUGEN content overwhelmingly uses the bare form (`random % 2`,
        // `random < 500`). `eval_inner`'s `Ident` arm now case-insensitively routes
        // bare `random` through `eval_random(&[], ctx)`, so it behaves identically
        // to the parened `random()` / `random(0, 999)` call form. This test pins
        // the corrected behavior; it previously pinned the buggy fall-through.

        // (1) Bare `random` draws an inclusive [0,999] value from the seeded seam,
        //     and matches the `random(0, 999)` call form for an equally-seeded
        //     context — same single draw, same formula (deterministic, non-flaky).
        let ctx_bare = MockContext::new().with_seed(7);
        let ctx_paren = MockContext::new().with_seed(7);
        let bare = ev("random", &ctx_bare);
        match bare {
            Value::Int(i) => assert!((0..=999).contains(&i), "bare random out of [0,999]: {i}"),
            other => panic!("bare random should be int, got {other:?}"),
        }
        assert_eq!(
            bare,
            ev("random(0, 999)", &ctx_paren),
            "bare `random` must draw from the same RNG seam as random(0, 999)"
        );

        // (2) Successive bare draws advance the RNG state through the seam (rather
        //     than re-reading a constant). With a fixed seed this is deterministic,
        //     not probabilistic: seed 7 yields 649 then 743, so the two draws differ.
        let ctx_seq = MockContext::new().with_seed(7);
        let first = ev("random", &ctx_seq);
        let second = ev("random", &ctx_seq);
        assert_ne!(
            first, second,
            "successive bare `random` draws must advance the RNG state through the seam"
        );
    }

    // ---- Determinism: a whole expression is reproducible for a fixed seed ----

    #[test]
    fn full_expression_is_deterministic_for_fixed_seed() {
        // The same parsed expression against two identically-seeded contexts must
        // produce byte-identical results (rollback/replay requirement, §11). Uses
        // the parened `random(..)` call form so the RNG seam is actually exercised.
        let expr = "cond(random(0, 1) < 1, random(0, 99) + var(0), fvar(0) * 2.0)";
        let ctx_a = MockContext::new()
            .with_seed(2024)
            .with_var(0, 7)
            .with_fvar(0, 1.5);
        let ctx_b = MockContext::new()
            .with_seed(2024)
            .with_var(0, 7)
            .with_fvar(0, 1.5);
        assert_eq!(ev(expr, &ctx_a), ev(expr, &ctx_b));
    }

    // ---- f32 working precision (§11): float ops round to single precision ----

    #[test]
    fn float_path_uses_f32_precision() {
        // 0.1 + 0.2 in f32 is the f32 rounding, not f64. Confirm the value is the
        // f32 sum (the evaluator narrows literals to f32 and computes in f32).
        let expected = 0.1f32 + 0.2f32;
        assert_eq!(evb("0.1 + 0.2"), Value::Float(expected));
    }

    // ---- Integration: a realistic compound trigger end-to-end ----

    #[test]
    fn realistic_compound_trigger_evaluates() {
        // Models the shape of common1.cns line 894:
        //   (GetHitVar(animtype) = [4,5]) && (SelfAnimExist(5047 + GetHitVar(animtype)))
        // Here `animtype` is modeled as a bare trigger = 4, and SelfAnimExist is a
        // parameterized trigger. The arg `5047 + animtype` evaluates to 5051, so
        // the lookup key is selfanimexist|5051 — register exactly that.
        let ctx = MockContext::new()
            .with_trigger("animtype", &[], Value::Int(4))
            .with_trigger("selfanimexist", &[Value::Int(5051)], Value::Int(1));
        assert_eq!(
            ev("(animtype = [4,5]) && SelfAnimExist(5047 + animtype)", &ctx),
            Value::Int(1)
        );
        // When the animtype falls outside [4,5] the && short-circuits to 0
        // (and never even consults SelfAnimExist).
        let ctx2 = MockContext::new()
            .with_trigger("animtype", &[], Value::Int(3))
            .with_trigger("selfanimexist", &[Value::Int(5050)], Value::Int(1));
        assert_eq!(
            ev(
                "(animtype = [4,5]) && SelfAnimExist(5047 + animtype)",
                &ctx2
            ),
            Value::Int(0)
        );
    }

    // =====================================================================
    // AC7 (real fixture): parse + EVALUATE real trigger expressions harvested
    // from production KFM CNS files, confirming the full lex→parse→eval
    // pipeline never panics and yields a concrete Value on real content.
    // Gated on test-assets/ so the default `cargo test` still passes when the
    // fixtures are absent (per the task's "gate it to skip" instruction).
    // =====================================================================

    #[test]
    fn real_kfm_trigger_expressions_evaluate_without_panic() {
        use std::path::Path;

        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = [
            manifest.join("../../test-assets/kfm/kfm.cns"),
            manifest.join("../../test-assets/kfm/common1.cns"),
        ];

        // An empty context: every trigger resolves to the safe default 0, so the
        // point of this test is the PIPELINE robustness — real expressions must
        // lex, parse, and evaluate to a concrete Value (never a panic, never a
        // leaked Bottom) on production content.
        let ctx = MockContext::new();

        let mut evaluated = 0usize;
        let mut any_file = false;

        for path in &files {
            if !path.exists() {
                eprintln!("skipping (absent): {path:?}");
                continue;
            }
            any_file = true;
            let text = std::fs::read_to_string(path).expect("read cns fixture");
            for raw in text.lines() {
                // Harvest the RHS of `trigger<N> = <expr>` and `triggerall = ...`
                // lines, which are exactly the trigger expressions this evaluator
                // exists to run.
                let line = raw.trim();
                let lower = line.to_ascii_lowercase();
                if !(lower.starts_with("trigger") || lower.starts_with("triggerall")) {
                    continue;
                }
                let Some(eq) = line.find('=') else { continue };
                let mut rhs = line[eq + 1..].trim();
                // Strip a trailing `;comment` if present (CNS inline comments).
                if let Some(semi) = rhs.find(';') {
                    rhs = rhs[..semi].trim();
                }
                if rhs.is_empty() {
                    continue;
                }
                // Some trigger RHS use `command = "name"` string compares the
                // current evaluator can parse (Str atom) — those still must not
                // panic. Parse; on a parse error just skip (the CNS layer maps
                // those to 0 separately). On success, EVALUATE.
                if let Ok(ast) = parse_str(rhs) {
                    let v = eval(&ast, &ctx);
                    assert!(
                        matches!(v, Value::Int(_) | Value::Float(_)),
                        "real expr {rhs:?} leaked a non-Value: {v:?}"
                    );
                    evaluated += 1;
                }
            }
        }

        if !any_file {
            eprintln!(
                "skipping real_kfm_trigger_expressions_evaluate_without_panic: no fixtures present"
            );
            return;
        }

        assert!(
            evaluated > 0,
            "fixtures present but no trigger expressions parsed+evaluated"
        );
        eprintln!("real-fixture: parsed + evaluated {evaluated} real trigger expressions, all yielded a concrete Value");
    }

    // =====================================================================
    // Proctor (task 4.8): redirection-evaluation gaps — bare-id lowering at
    // eval time, the negative `enemy(n)` form, typed-bank reads and RNG draws
    // through a redirect, bottom inside a redirected sub-expression, lazy
    // redirect branches, and self-vs-target isolation. No impl code changed.
    // =====================================================================

    // ---- AC3: bare-id keyword forms resolve to their lowered targets ----

    #[test]
    fn bare_helper_redirect_evaluates_through_id_zero() {
        // `helper, x` lowers to Helper(0); the eval must resolve that exact target.
        let helper0 = MockContext::new().with_trigger("x", &[], Value::Int(42));
        let ctx = MockContext::new().with_redirect(Redirect::Helper(0), helper0);
        assert_eq!(ev("helper, x", &ctx), Value::Int(42));
    }

    #[test]
    fn bare_target_redirect_evaluates_through_none() {
        // `target, x` lowers to Target(None); `target(0), x` is Target(Some(0)) —
        // two distinct targets that must NOT alias at eval time.
        let any_target = MockContext::new().with_trigger("x", &[], Value::Int(1));
        let target0 = MockContext::new().with_trigger("x", &[], Value::Int(2));
        let ctx = MockContext::new()
            .with_redirect(Redirect::Target(None), any_target)
            .with_redirect(Redirect::Target(Some(0)), target0);
        assert_eq!(ev("target, x", &ctx), Value::Int(1));
        assert_eq!(ev("target(0), x", &ctx), Value::Int(2));
    }

    #[test]
    fn bare_enemynear_redirect_evaluates_through_zero() {
        let near0 = MockContext::new().with_trigger("life", &[], Value::Int(300));
        let ctx = MockContext::new().with_redirect(Redirect::EnemyNear(0), near0);
        assert_eq!(ev("enemynear, life", &ctx), Value::Int(300));
    }

    #[test]
    fn negative_enemy_index_evaluates_through_enemynear_negative() {
        // CB8 lowering carries a negative index: `enemy(-1)` → EnemyNear(-1).
        let near_neg = MockContext::new().with_trigger("life", &[], Value::Int(7));
        let ctx = MockContext::new().with_redirect(Redirect::EnemyNear(-1), near_neg);
        assert_eq!(ev("enemy(-1), life", &ctx), Value::Int(7));
    }

    // ---- AC3: typed variable banks resolve against the REDIRECTED context ----

    #[test]
    fn redirected_var_fvar_sysvar_read_from_target_not_self() {
        // var/fvar/sysvar must route to the redirected entity's banks. Self and the
        // redirected target hold different values for the same index.
        let target = MockContext::new()
            .with_var(0, 111)
            .with_fvar(1, 2.5)
            .with_sysvar(2, 333);
        let ctx = MockContext::new()
            .with_var(0, 1)
            .with_fvar(1, 9.9)
            .with_sysvar(2, 3)
            .with_redirect(Redirect::Root, target);
        // Self reads.
        assert_eq!(ev("var(0)", &ctx), Value::Int(1));
        // Redirected reads.
        assert_eq!(ev("root, var(0)", &ctx), Value::Int(111));
        assert_eq!(ev("root, fvar(1)", &ctx), Value::Float(2.5));
        assert_eq!(ev("root, sysvar(2)", &ctx), Value::Int(333));
    }

    #[test]
    fn redirected_parameterized_trigger_uses_target_and_evaluated_args() {
        // `enemy, AnimElemTime(1 + 1)` — the arg is evaluated (to 2) and the lookup
        // happens against the ENEMY context.
        let enemy =
            MockContext::new().with_trigger("animelemtime", &[Value::Int(2)], Value::Int(5));
        let ctx = MockContext::new().with_redirect(Redirect::Enemy, enemy);
        assert_eq!(ev("enemy, AnimElemTime(1 + 1)", &ctx), Value::Int(5));
    }

    // ---- AC3: the RNG seam is read from the REDIRECTED context ----

    #[test]
    fn redirected_random_draws_from_target_rng_seam() {
        // `enemy, random` must consult the enemy's RNG seam, not self's. The enemy
        // is seeded; self is not. The result must equal a direct draw from an
        // identically-seeded context's random(0,999) form (same single-draw [0,999]
        // formula), proving the draw came from the enemy, deterministically.
        let enemy = MockContext::new().with_seed(7);
        let ctx = MockContext::new().with_redirect(Redirect::Enemy, enemy);
        let drawn = ev("enemy, random", &ctx);
        match drawn {
            Value::Int(i) => assert!((0..=999).contains(&i), "out of [0,999]: {i}"),
            other => panic!("redirected random should be int, got {other:?}"),
        }
        // Same seed, same single-draw formula ⇒ same value.
        let reference = MockContext::new().with_seed(7);
        assert_eq!(drawn, ev("random(0, 999)", &reference));
    }

    // ---- AC3: bottom inside a redirected sub-expression collapses to 0 ----

    #[test]
    fn redirected_bottom_subexpr_collapses_to_default() {
        // The redirect resolves fine, but the sub-expr errors (divide-by-zero,
        // unknown trigger participating in a poisoned op). The result is the safe
        // default 0 at the public boundary — never a panic, never a leaked Bottom.
        let enemy = MockContext::new().with_trigger("life", &[], Value::Int(100));
        let ctx = MockContext::new().with_redirect(Redirect::Enemy, enemy);
        assert_eq!(ev("enemy, life / 0", &ctx), Value::Int(0));
        assert_eq!(ev("enemy, 1 / 0", &ctx), Value::Int(0));
        // A standalone string atom inside the redirect is also bottom → 0.
        assert_eq!(ev("enemy, \"x\"", &ctx), Value::Int(0));
    }

    #[test]
    fn redirected_float_value_flows_through_outer_context_arithmetic() {
        // A redirect binds the WHOLE trailing expression, so any outer arithmetic
        // is also evaluated in the target context. Confirm a redirected float read
        // promotes correctly within that target-context arithmetic.
        let enemy = MockContext::new().with_trigger("posx", &[], Value::Float(-3.5));
        let ctx = MockContext::new().with_redirect(Redirect::Enemy, enemy);
        // `enemy, posx + 1` → -2.5 (posx from enemy, +1 still in enemy ctx).
        assert_eq!(ev("enemy, posx + 1", &ctx), Value::Float(-2.5));
        assert_eq!(ev("enemy, posx < 0", &ctx), Value::Int(1));
    }

    // ---- AC3/AC5: a redirect inside a paren group binds only its sub-expr ----

    #[test]
    fn redirect_in_paren_group_targets_only_inner_then_outer_is_self() {
        // `(enemy, life) + 1` reads life from the ENEMY, but the `+ 1` happens in
        // SELF. With enemy.life = 50, result is 51. Self.life is irrelevant here.
        let enemy = MockContext::new().with_trigger("life", &[], Value::Int(50));
        let ctx = MockContext::new()
            .with_trigger("life", &[], Value::Int(999))
            .with_redirect(Redirect::Enemy, enemy);
        assert_eq!(ev("(enemy, life) + 1", &ctx), Value::Int(51));
    }

    // ---- AC3/AC5: redirect as a lazy cond/&& branch is only evaluated if taken --

    #[test]
    fn redirect_in_untaken_cond_branch_is_not_resolved() {
        // The redirected branch of a cond must not be resolved when not taken.
        // Use a context that counts redirect resolutions to prove laziness.
        struct CountingRedirect {
            resolves: Cell<u32>,
            child: MockContext,
        }
        impl EvalContext for CountingRedirect {
            fn trigger(&self, _name: &str, _args: &[Value]) -> Value {
                Value::Int(7)
            }
            fn redirect(&self, _target: Redirect) -> Option<&dyn EvalContext> {
                self.resolves.set(self.resolves.get() + 1);
                Some(&self.child)
            }
        }
        let ctx = CountingRedirect {
            resolves: Cell::new(0),
            child: MockContext::new().with_trigger("life", &[], Value::Int(1)),
        };
        // cond(0, (enemy,life), 9) → 9; the redirected branch is untaken, so
        // `redirect` is never called.
        assert_eq!(
            eval(&parse_str("cond(0, (enemy, life), 9)").unwrap(), &ctx),
            Value::Int(9)
        );
        assert_eq!(
            ctx.resolves.get(),
            0,
            "untaken redirect branch must not resolve"
        );

        // When taken, it resolves exactly once.
        let ctx2 = CountingRedirect {
            resolves: Cell::new(0),
            child: MockContext::new().with_trigger("life", &[], Value::Int(1)),
        };
        assert_eq!(
            eval(&parse_str("cond(1, (enemy, life), 9)").unwrap(), &ctx2),
            Value::Int(1)
        );
        assert_eq!(
            ctx2.resolves.get(),
            1,
            "taken redirect branch resolves once"
        );
    }

    // ---- AC3: a deep chain that dead-ends mid-way collapses the whole expr ----

    #[test]
    fn deep_redirect_chain_missing_middle_hop_is_default() {
        // `root, parent, enemy, life` — root and parent resolve, but parent has no
        // enemy → the inner redirect is None → bottom → the whole expression is 0.
        let parent = MockContext::new(); // no `enemy` relation
        let root = MockContext::new().with_redirect(Redirect::Parent, parent);
        let ctx = MockContext::new().with_redirect(Redirect::Root, root);
        assert_eq!(ev("root, parent, enemy, life", &ctx), Value::Int(0));

        // Wire the final hop and confirm the same chain now resolves end-to-end.
        let leaf = MockContext::new().with_trigger("life", &[], Value::Int(123));
        let parent2 = MockContext::new().with_redirect(Redirect::Enemy, leaf);
        let root2 = MockContext::new().with_redirect(Redirect::Parent, parent2);
        let ctx2 = MockContext::new().with_redirect(Redirect::Root, root2);
        assert_eq!(ev("root, parent, enemy, life", &ctx2), Value::Int(123));
    }

    // ---- AC3: redirected unknown trigger defaults to 0 (target exists) ----

    #[test]
    fn redirected_unknown_trigger_is_zero_not_self_value() {
        // The target resolves but does not know the trigger → 0, even though SELF
        // does know it. Proves the lookup happens in the target, not a fallback to
        // self.
        let enemy = MockContext::new(); // knows nothing
        let ctx = MockContext::new()
            .with_trigger("secret", &[], Value::Int(555))
            .with_redirect(Redirect::Enemy, enemy);
        assert_eq!(ev("secret", &ctx), Value::Int(555)); // self knows it
        assert_eq!(ev("enemy, secret", &ctx), Value::Int(0)); // enemy does not
    }

    // =====================================================================
    // Task 4.10: real-content trigger support — axis-suffixed component
    // triggers, the AnimElem `= N, op M` tail, dotted call args, and
    // `command = "name"` string equality.
    // =====================================================================

    // ---- Gap 1: axis-suffixed component triggers (`Vel Y`, `Pos X`, …) ----

    #[test]
    fn axis_suffixed_trigger_reads_with_axis_code() {
        // `Vel Y` → trigger("Vel", &[Int(1)]); `Vel X` → trigger("Vel", &[Int(0)]).
        // The axis codes are X=0, Y=1, Z=2 (see `axis_arg_code`).
        let ctx = MockContext::new()
            .with_trigger("Vel", &[Value::Int(0)], Value::Float(3.0)) // Vel X
            .with_trigger("Vel", &[Value::Int(1)], Value::Float(-7.5)); // Vel Y
        assert_eq!(ev("Vel X", &ctx), Value::Float(3.0));
        assert_eq!(ev("Vel Y", &ctx), Value::Float(-7.5));
        // Case-insensitive axis word resolves to the same code.
        assert_eq!(ev("Vel y", &ctx), Value::Float(-7.5));
    }

    #[test]
    fn axis_suffixed_trigger_in_comparison_and_redirect() {
        // A real KFM shape: `Vel Y > 0` and `Pos Y >= -20`.
        let ctx = MockContext::new()
            .with_trigger("Vel", &[Value::Int(1)], Value::Float(2.5))
            .with_trigger("Pos", &[Value::Int(1)], Value::Float(-10.0));
        assert_eq!(ev("Vel Y > 0", &ctx), Value::Int(1));
        assert_eq!(ev("Pos Y >= -20", &ctx), Value::Int(1));
        assert_eq!(ev("(Vel Y > 0) && (Pos Y >= 0)", &ctx), Value::Int(0));

        // Through a redirect: `enemy, P2BodyDist X < 40` (kfm.cns AI logic).
        let enemy =
            MockContext::new().with_trigger("P2BodyDist", &[Value::Int(0)], Value::Float(20.0));
        let ctx = MockContext::new().with_redirect(Redirect::Enemy, enemy);
        assert_eq!(ev("enemy, P2BodyDist X < 40", &ctx), Value::Int(1));
        // The bare (self) form reads the un-redirected context: default 0 here.
        assert_eq!(ev("P2BodyDist X < 40", &MockContext::new()), Value::Int(1));
        // 0 < 40
    }

    #[test]
    fn axis_suffix_x_and_y_are_distinguishable() {
        // The whole point of the int encoding: X and Y must NOT alias. With only
        // Y registered, X reads the default 0.
        let ctx = MockContext::new().with_trigger("Pos", &[Value::Int(1)], Value::Float(99.0));
        assert_eq!(ev("Pos Y", &ctx), Value::Float(99.0));
        assert_eq!(ev("Pos X", &ctx), Value::Int(0)); // unregistered X → default
    }

    // ---- Gap 2: the `AnimElem = N, op M` comparison tail ----

    #[test]
    fn animelem_tail_requires_reached_and_secondary() {
        // `AnimElem = 2, >= 0`: true iff element 2 reached (AnimElemTime(2) >= 0)
        // AND AnimElemTime(2) >= 0. Element reached with time 0 → true.
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(2)], Value::Int(0));
        assert_eq!(ev("AnimElem = 2, >= 0", &ctx), Value::Int(1));

        // Not reached (negative element time) → false even though the secondary
        // comparison alone would be satisfied for a positive M.
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(2)], Value::Int(-1));
        assert_eq!(ev("AnimElem = 2, >= 0", &ctx), Value::Int(0));
        assert_eq!(ev("AnimElem = 2, <= -1", &ctx), Value::Int(0));
    }

    #[test]
    fn animelem_tail_secondary_comparison_is_applied() {
        // Reached (time 5), `, > 3` → true; `, > 9` → false; `, = 5` → true.
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(3)], Value::Int(5));
        assert_eq!(ev("AnimElem = 3, > 3", &ctx), Value::Int(1));
        assert_eq!(ev("AnimElem = 3, > 9", &ctx), Value::Int(0));
        assert_eq!(ev("AnimElem = 3, = 5", &ctx), Value::Int(1));
        // Omitted operator defaults to `=`: `AnimElem = 3, 5` ≡ `, = 5`.
        assert_eq!(ev("AnimElem = 3, 5", &ctx), Value::Int(1));
        assert_eq!(ev("AnimElem = 3, 4", &ctx), Value::Int(0));
    }

    #[test]
    fn animelem_tail_real_kfm_form() {
        // Verbatim kfm.cns line 1703: `AnimElem = 3, -1` (omitted op → `=`). True
        // iff element 3 reached and AnimElemTime(3) == -1 — but "reached" already
        // requires time >= 0, so a -1 secondary can never fire (documented
        // MUGEN-faithful conjunction). With time 0 it is false; the form still
        // evaluates to a concrete value and never panics.
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(3)], Value::Int(0));
        assert_eq!(ev("AnimElem = 3, -1", &ctx), Value::Int(0));
    }

    // ---- Item (a): member-keyed GetHitVar passes the member NAME as the key ----

    #[test]
    fn gethitvar_member_arg_passes_name_through_string_seam() {
        // `GetHitVar(member)` must route through `trigger_str` with the member
        // NAME, not evaluate the member as a nested trigger and pass its value
        // (task 4.11, item a). A context that keys on the member name returns the
        // field's value; distinct member names give distinct values.
        let ctx = MockContext::new()
            .with_member_trigger("GetHitVar", "fall.yvel", Value::Float(-4.5))
            .with_member_trigger("GetHitVar", "xveladd", Value::Int(7));
        assert_eq!(ev("GetHitVar(fall.yvel)", &ctx), Value::Float(-4.5));
        assert_eq!(ev("GetHitVar(xveladd)", &ctx), Value::Int(7));
        // An unknown member → safe default 0, never a panic.
        assert_eq!(ev("GetHitVar(nosuchfield)", &ctx), Value::Int(0));
        assert_eq!(ev("GetHitVar(xveladd)", &MockContext::new()), Value::Int(0));
    }

    #[test]
    fn gethitvar_does_not_evaluate_member_as_nested_trigger() {
        // The lossy "evaluate the member ident as a nested trigger, pass its
        // value" path is GONE: seeding the member name as an ordinary numeric
        // trigger must NOT affect the GetHitVar read (the member is a string key,
        // not a value). Here `fall.yvel` is seeded as a numeric trigger AND as the
        // GetHitVar 0-arg key that the old behavior would have hit — neither must
        // be consulted; only the member-keyed seam is.
        let ctx = MockContext::new()
            .with_trigger("fall.yvel", &[], Value::Int(7))
            .with_trigger("gethitvar", &[Value::Int(7)], Value::Int(99))
            .with_trigger("gethitvar", &[Value::Int(0)], Value::Int(123))
            .with_member_trigger("GetHitVar", "fall.yvel", Value::Int(55));
        // Only the member-keyed value is returned — not 99 (old: member→7→arg) nor
        // 123 (old: unknown member→0→arg).
        assert_eq!(ev("GetHitVar(fall.yvel)", &ctx), Value::Int(55));
    }

    #[test]
    fn gethitvar_multi_segment_member_routes_through_string_seam() {
        // A three-segment dotted member is one identifier; it routes through the
        // string seam by its full name (task 4.11, item a).
        let ctx = MockContext::new().with_member_trigger(
            "GetHitVar",
            "fall.envshake.time",
            Value::Int(5),
        );
        assert_eq!(ev("GetHitVar(fall.envshake.time)", &ctx), Value::Int(5));
    }

    #[test]
    fn gethitvar_through_redirect_uses_target_member_seam() {
        // `enemy, GetHitVar(fall.yvel)` reads the member from the REDIRECTED
        // context's member seam, never the local one.
        let enemy =
            MockContext::new().with_member_trigger("GetHitVar", "fall.yvel", Value::Float(-9.0));
        let ctx = MockContext::new()
            .with_member_trigger("GetHitVar", "fall.yvel", Value::Float(-4.5))
            .with_redirect(Redirect::Enemy, enemy);
        assert_eq!(ev("enemy, GetHitVar(fall.yvel)", &ctx), Value::Float(-9.0));
    }

    // ---- Task 5.6d: member-keyed `const(member)` passes the member NAME ----

    #[test]
    fn const_member_arg_passes_name_through_string_seam() {
        // `const(member)` must route through `trigger_str` with the member NAME
        // (the dotted ident verbatim), exactly like GetHitVar (task 5.6d). This is
        // acceptance criterion 2's core assertion: a context whose `trigger_str`
        // returns a known value for a const member returns it.
        let ctx = MockContext::new()
            .with_member_trigger("const", "velocity.walk.fwd.x", Value::Float(2.4))
            .with_member_trigger("const", "size.ground.front", Value::Int(16))
            .with_member_trigger("const", "movement.yaccel", Value::Float(0.44));
        assert_eq!(ev("const(velocity.walk.fwd.x)", &ctx), Value::Float(2.4));
        assert_eq!(ev("const(size.ground.front)", &ctx), Value::Int(16));
        assert_eq!(ev("const(movement.yaccel)", &ctx), Value::Float(0.44));
        // An unknown member → safe default 0, never a panic.
        assert_eq!(ev("const(no.such.const)", &ctx), Value::Int(0));
    }

    #[test]
    fn const_is_additive_safe_default_zero_with_empty_context() {
        // Acceptance criterion 2 (additive/safe): with a context whose
        // `trigger_str` returns the default (no resolver yet), `const(...)` still
        // evaluates to 0 — no regression.
        assert_eq!(
            ev("const(velocity.walk.fwd.x)", &MockContext::new()),
            Value::Int(0)
        );
        // Even composed in arithmetic / comparison it stays a harmless 0.
        assert_eq!(
            ev("const(movement.yaccel) > 0", &MockContext::new()),
            Value::Int(0)
        );
    }

    #[test]
    fn const_does_not_evaluate_member_as_nested_trigger() {
        // The dotted member must NOT be evaluated as a nested trigger (that path
        // yields 0 — the 5.6c walk-velocity gap). Seeding the member name as an
        // ordinary numeric trigger AND the const numeric-arg forms the old/lossy
        // behavior would hit must NOT be consulted; only the member-keyed seam is.
        let ctx = MockContext::new()
            .with_trigger("velocity.walk.fwd.x", &[], Value::Int(7))
            .with_trigger("const", &[Value::Int(7)], Value::Int(99))
            .with_trigger("const", &[Value::Int(0)], Value::Int(123))
            .with_member_trigger("const", "velocity.walk.fwd.x", Value::Float(2.4));
        // Only the member-keyed value is returned — not 99 (old: member→7→arg) nor
        // 123 (old: unknown member→0→arg).
        assert_eq!(ev("const(velocity.walk.fwd.x)", &ctx), Value::Float(2.4));
    }

    #[test]
    fn const_trigger_name_is_case_insensitive_through_eval() {
        // `const` is matched case-insensitively (MUGEN is case-insensitive); any
        // casing routes through the string seam.
        let ctx =
            MockContext::new().with_member_trigger("const", "movement.yaccel", Value::Float(0.44));
        for src in [
            "const(movement.yaccel)",
            "Const(movement.yaccel)",
            "CONST(movement.yaccel)",
        ] {
            assert_eq!(ev(src, &ctx), Value::Float(0.44), "src = {src}");
        }
    }

    #[test]
    fn const_through_redirect_uses_target_member_seam() {
        // `enemy, const(movement.yaccel)` reads the constant from the REDIRECTED
        // context's member seam, never the local one.
        let enemy =
            MockContext::new().with_member_trigger("const", "movement.yaccel", Value::Float(0.9));
        let ctx = MockContext::new()
            .with_member_trigger("const", "movement.yaccel", Value::Float(0.44))
            .with_redirect(Redirect::Enemy, enemy);
        assert_eq!(ev("enemy, const(movement.yaccel)", &ctx), Value::Float(0.9));
    }

    #[test]
    fn is_member_keyed_trigger_set_is_exactly_gethitvar_and_const() {
        // The documented member-keyed set (single source of truth) is exactly
        // these two names; nothing else routes through the string seam.
        assert!(is_member_keyed_trigger("gethitvar"));
        assert!(is_member_keyed_trigger("const"));
        assert_eq!(MEMBER_KEYED_TRIGGERS, ["gethitvar", "const"]);
        for other in ["var", "fvar", "sysvar", "animelem", "numexplod", ""] {
            assert!(!is_member_keyed_trigger(other), "unexpected: {other}");
        }
    }

    // =====================================================================
    // Proctor (task 5.6d): additional `const(member)` edge / error-path /
    // MUGEN-semantics coverage layered on top of Forge's 5.6d tests above.
    // Mirrors the GetHitVar edge cases (non-ident fallthrough, arity, value
    // type, composition, default seam) for `const` and pins cross-talk and
    // verbatim forwarding. Pure tests; no impl code is modified.
    // =====================================================================

    #[test]
    fn const_non_ident_arg_falls_through_to_numeric_path() {
        // The member seam only fires for a SINGLE BARE-IDENTIFIER argument. A
        // computed / numeric argument (`const(1+1)`, `const(var(0))`) is not a
        // member name, so it falls through to the ordinary numeric `trigger`
        // path with the EVALUATED arg — it must NOT touch `trigger_str`. Seed
        // both seams; only the numeric one may be consulted. (Mirrors the
        // GetHitVar analogue so `const`'s fallthrough is independently pinned.)
        let ctx = MockContext::new()
            .with_var(0, 2)
            .with_trigger("const", &[Value::Int(2)], Value::Int(70))
            .with_member_trigger("const", "2", Value::Int(999)); // must NOT be hit
                                                                 // `const(1+1)` → arg evaluates to 2 → numeric trigger("const",[2]).
        assert_eq!(ev("const(1+1)", &ctx), Value::Int(70));
        // `const(var(0))` → var(0)=2 → same numeric lookup.
        assert_eq!(ev("const(var(0))", &ctx), Value::Int(70));
    }

    #[test]
    fn const_zero_and_multi_arg_forms_use_numeric_path() {
        // Only the single-bare-ident shape is member-keyed. A zero-arg or
        // multi-arg `const` call is not, so it routes numerically and never
        // panics. (The surface syntax is unusual for `const` but legal in the
        // AST; build the forms directly.)
        let ctx = MockContext::new()
            .with_trigger("const", &[], Value::Int(11))
            .with_trigger("const", &[Value::Int(1), Value::Int(2)], Value::Int(22));
        let zero = Expr::Call {
            name: "const".into(),
            args: vec![],
        };
        assert_eq!(eval(&zero, &ctx), Value::Int(11));
        let two = Expr::Call {
            name: "const".into(),
            args: vec![Expr::Int(1), Expr::Int(2)],
        };
        assert_eq!(eval(&two, &ctx), Value::Int(22));
    }

    #[test]
    fn const_member_value_type_is_preserved_through_eval() {
        // The member seam returns whatever Value the authored constant holds: an
        // int constant stays int, a float constant stays float — proving no
        // lossy int-collapse (the whole point of routing through trigger_str).
        let ctx = MockContext::new()
            .with_member_trigger("const", "size.ground.front", Value::Int(16))
            .with_member_trigger("const", "velocity.walk.fwd.x", Value::Float(2.4));
        let int_v = ev("const(size.ground.front)", &ctx);
        assert_eq!(int_v, Value::Int(16));
        assert!(
            int_v.is_int(),
            "an int constant must stay Int, got {int_v:?}"
        );
        let flt_v = ev("const(velocity.walk.fwd.x)", &ctx);
        assert_eq!(flt_v, Value::Float(2.4));
        assert!(
            flt_v.is_float(),
            "a float constant must stay Float, got {flt_v:?}"
        );
    }

    #[test]
    fn const_member_composes_in_arithmetic_and_comparison() {
        // The const read is an ordinary atom in a larger expression — this is the
        // real 5.6c walk-velocity shape, where the authored constant feeds into
        // arithmetic (`const(velocity.walk.fwd.x) * 60` for a per-second speed)
        // and comparisons. With the seam resolving, the value flows through; the
        // 5.6c gap was exactly this composing to 0 because const read 0.
        let ctx = MockContext::new().with_member_trigger(
            "const",
            "velocity.walk.fwd.x",
            Value::Float(2.4),
        );
        assert_eq!(
            ev("const(velocity.walk.fwd.x) * 10", &ctx),
            Value::Float(24.0)
        );
        assert_eq!(ev("const(velocity.walk.fwd.x) > 0", &ctx), Value::Int(1));
        assert_eq!(ev("const(velocity.walk.fwd.x) < 0", &ctx), Value::Int(0));
        // And it short-circuits naturally in a boolean guard.
        assert_eq!(
            ev("const(velocity.walk.fwd.x) > 0 && 1", &ctx),
            Value::Int(1)
        );
    }

    #[test]
    fn const_default_trigger_str_yields_zero_not_panic() {
        // A context that overrides ONLY `trigger` (not `trigger_str`) inherits the
        // trait default (every member → 0). The const member path must use
        // `trigger_str`, so it reads 0 here even though `trigger` would answer
        // nonzero — proving the routing AND the never-panic, additive-safe
        // contract (this is the pre-resolver state that 5.6e will fill).
        struct ConstViaTriggerOnly;
        impl EvalContext for ConstViaTriggerOnly {
            fn trigger(&self, _name: &str, _args: &[Value]) -> Value {
                // If the member were (wrongly) routed numerically this nonzero
                // value would leak through; the member path must not see it.
                Value::Int(42)
            }
        }
        let ctx = ConstViaTriggerOnly;
        assert_eq!(ev("const(velocity.walk.fwd.x)", &ctx), Value::Int(0));
        // Even composed, it stays a harmless 0 and never panics.
        assert_eq!(ev("const(movement.yaccel) || 0", &ctx), Value::Int(0));
    }

    #[test]
    fn const_forwards_member_name_verbatim_to_trigger_str() {
        // The evaluator must pass the member identifier through to `trigger_str`
        // VERBATIM (case preserved, dots intact). A context that records the
        // exact (name, key) it received proves no normalization happens in the
        // evaluator (case-folding is the context's job, per the trait docs).
        struct RecordingCtx {
            seen: Cell<Option<(&'static str, bool)>>,
        }
        impl EvalContext for RecordingCtx {
            fn trigger(&self, _name: &str, _args: &[Value]) -> Value {
                Value::DEFAULT
            }
            fn trigger_str(&self, name: &str, key: &str) -> Value {
                // Record whether the verbatim mixed-case dotted member arrived
                // unchanged and the trigger name is "const".
                let exact = name.eq_ignore_ascii_case("const") && key == "Velocity.Walk.Fwd.X";
                self.seen.set(Some(("const", exact)));
                Value::Int(if exact { 1 } else { 0 })
            }
        }
        let ctx = RecordingCtx {
            seen: Cell::new(None),
        };
        // The member ident keeps its authored casing through to the seam.
        assert_eq!(ev("const(Velocity.Walk.Fwd.X)", &ctx), Value::Int(1));
        assert_eq!(
            ctx.seen.get(),
            Some(("const", true)),
            "trigger_str must receive the dotted member name verbatim"
        );
    }

    #[test]
    fn const_and_gethitvar_member_seams_do_not_cross_talk() {
        // `const` and `GetHitVar` are distinct member-keyed triggers. The SAME
        // member name under each must resolve independently (the seam keys on the
        // trigger NAME as well as the member), so a `const(x)` read never returns
        // a `GetHitVar(x)` value or vice versa.
        let ctx = MockContext::new()
            .with_member_trigger("const", "yaccel", Value::Float(0.44))
            .with_member_trigger("GetHitVar", "yaccel", Value::Float(-9.0));
        assert_eq!(ev("const(yaccel)", &ctx), Value::Float(0.44));
        assert_eq!(ev("GetHitVar(yaccel)", &ctx), Value::Float(-9.0));
    }

    #[test]
    fn const_negative_and_int_member_values_round_trip() {
        // Authored constants can be negative and int-typed (e.g. a backward walk
        // velocity, an int size). Both must survive the seam untouched — no sign
        // loss, no float coercion of an int constant.
        let ctx = MockContext::new()
            .with_member_trigger("const", "velocity.walk.back.x", Value::Float(-2.3))
            .with_member_trigger("const", "size.ground.back", Value::Int(15));
        assert_eq!(ev("const(velocity.walk.back.x)", &ctx), Value::Float(-2.3));
        assert_eq!(ev("const(size.ground.back)", &ctx), Value::Int(15));
        // Negation of a float const stays a float.
        assert_eq!(
            ev("0.0 - const(velocity.walk.back.x)", &ctx),
            Value::Float(2.3)
        );
    }

    // ---- Gap 4: `command = "name"` string equality ----

    #[test]
    fn command_string_equality_consults_command_seam() {
        // A context that reports a specific command active.
        struct CmdCtx(&'static str);
        impl EvalContext for CmdCtx {
            fn trigger(&self, _name: &str, _args: &[Value]) -> Value {
                Value::DEFAULT
            }
            fn command_active(&self, name: &str) -> bool {
                name.eq_ignore_ascii_case(self.0)
            }
        }
        let ctx = CmdCtx("recovery");
        // `command = "recovery"` fires; a different name does not.
        assert_eq!(
            eval(&parse_str("command = \"recovery\"").unwrap(), &ctx),
            Value::Int(1)
        );
        assert_eq!(
            eval(&parse_str("command = \"x\"").unwrap(), &ctx),
            Value::Int(0)
        );
        // Reversed operand order works too: `"recovery" = command`.
        assert_eq!(
            eval(&parse_str("\"recovery\" = command").unwrap(), &ctx),
            Value::Int(1)
        );
        // `!=` negates the seam result.
        assert_eq!(
            eval(&parse_str("command != \"x\"").unwrap(), &ctx),
            Value::Int(1)
        );
        assert_eq!(
            eval(&parse_str("command != \"recovery\"").unwrap(), &ctx),
            Value::Int(0)
        );
        // Case-insensitive `command` keyword.
        assert_eq!(
            eval(&parse_str("Command = \"recovery\"").unwrap(), &ctx),
            Value::Int(1)
        );
    }

    #[test]
    fn command_string_equality_default_context_is_false() {
        // The default `command_active` returns false, so `command = "x"` is 0
        // (never fires) but never panics — the documented safe default.
        let ctx = MockContext::new();
        assert_eq!(ev("command = \"x\"", &ctx), Value::Int(0));
        assert_eq!(ev("command = \"recovery\"", &ctx), Value::Int(0));
        // It composes in a boolean: `command = "x" || 1` falls through to 1.
        assert_eq!(ev("command = \"x\" || 1", &ctx), Value::Int(1));
    }

    #[test]
    fn non_command_string_compare_is_bottom_zero() {
        // A `trigger = "string"` where the trigger is NOT `command` keeps the
        // pre-4.10 behavior: the Str RHS is bottom → the comparison is 0 (never
        // fires), documented and panic-free.
        let ctx = MockContext::new().with_trigger("name", &[], Value::Int(5));
        assert_eq!(ev("name = \"foo\"", &ctx), Value::Int(0));
    }

    // =====================================================================
    // Proctor (task 4.10) — additional evaluator semantics for the four gaps,
    // complementing Forge's coverage with the Z axis, the nested-trigger
    // dotted-arg lowering, command OR-chains, the chained left-assoc edge, and
    // AnimElem-family / float / redirect tail cases.
    // =====================================================================

    /// A context that reports a fixed set of commands active, for command-seam
    /// tests. Everything else defaults safely.
    struct CommandCtx {
        active: Vec<&'static str>,
    }
    impl EvalContext for CommandCtx {
        fn trigger(&self, _name: &str, _args: &[Value]) -> Value {
            Value::DEFAULT
        }
        fn command_active(&self, name: &str) -> bool {
            self.active.iter().any(|c| c.eq_ignore_ascii_case(name))
        }
    }

    // ---- Gap 1: axis suffix — Z axis, redirect eval, composition ----

    #[test]
    fn axis_suffix_z_axis_reads_code_2() {
        // The Z axis encodes to 2 (3-D quantities); confirm it reaches trigger.
        let ctx = MockContext::new().with_trigger("Pos", &[Value::Int(2)], Value::Float(5.5));
        assert_eq!(ev("Pos Z", &ctx), Value::Float(5.5));
        // X (0) and Y (1) are not Z and read default here.
        assert_eq!(ev("Pos X", &ctx), Value::Int(0));
        assert_eq!(ev("Pos Y", &ctx), Value::Int(0));
    }

    #[test]
    fn axis_suffix_value_through_redirect_and_arithmetic() {
        // `root, Vel Y + 1.0` — the axis-folded call evaluates against the
        // redirected context, then composes with arithmetic (float promotion).
        let root = MockContext::new().with_trigger("Vel", &[Value::Int(1)], Value::Float(2.0));
        let ctx = MockContext::new().with_redirect(Redirect::Root, root);
        assert_eq!(ev("root, Vel Y + 1.0", &ctx), Value::Float(3.0));
    }

    #[test]
    fn axis_suffix_unknown_axis_string_maps_to_x_default() {
        // The parser only ever emits X/Y/Z, but the evaluator's axis_arg_code
        // maps any non-XYZ string to 0 (the safe default). Build that pathological
        // AST directly to prove the eval path is panic-free and defaults to 0.
        let ast = Expr::Call {
            name: "Vel".into(),
            args: vec![Expr::Str("W".into())],
        };
        let ctx = MockContext::new().with_trigger("Vel", &[Value::Int(0)], Value::Float(9.0)); // code 0 == X
                                                                                               // "W" → axis_arg_code 0, so it reads the X-coded value.
        assert_eq!(eval(&ast, &ctx), Value::Float(9.0));
    }

    // ---- Gap 2: AnimElem tail — family members, float, redirect ----

    #[test]
    fn animelem_tail_family_members_all_resolve_via_animelemtime() {
        // Both comparison-tail family members lower to a read of AnimElemTime(N);
        // the trigger name on the AST is diagnostic only. After task 4.11 item
        // (b) the family is exactly AnimElem + AnimElemTime (TimeMod / AnimElemNo
        // are no longer mis-folded into this form — see the parser tests).
        for head in ["AnimElem", "AnimElemTime"] {
            let ctx =
                MockContext::new().with_trigger("animelemtime", &[Value::Int(4)], Value::Int(3));
            let src = format!("{head} = 4, >= 0");
            assert_eq!(ev(&src, &ctx), Value::Int(1), "{src}");
            // Secondary that fails → false.
            let src2 = format!("{head} = 4, > 10");
            assert_eq!(ev(&src2, &ctx), Value::Int(0), "{src2}");
        }
    }

    #[test]
    fn animelem_tail_float_element_index_and_operand() {
        // A float element index narrows to int for the AnimElemTime lookup; a
        // float operand compares with float promotion. `AnimElem = 2.0, >= 0.5`.
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(2)], Value::Int(1));
        assert_eq!(ev("AnimElem = 2.0, >= 0.5", &ctx), Value::Int(1));
        assert_eq!(ev("AnimElem = 2.0, >= 1.5", &ctx), Value::Int(0));
    }

    #[test]
    fn animelem_tail_unreached_element_never_fires_regardless_of_secondary() {
        // The conjunction's "reached" guard dominates: a not-yet-reached element
        // (negative time) is always false, even if the secondary would pass.
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(9)], Value::Int(-5));
        assert_eq!(ev("AnimElem = 9, <= 0", &ctx), Value::Int(0));
        assert_eq!(ev("AnimElem = 9, = -5", &ctx), Value::Int(0));
        assert_eq!(ev("AnimElem = 9, < 100", &ctx), Value::Int(0));
    }

    #[test]
    fn animelem_tail_after_redirect_now_composes() {
        // Task A generalized the comma-tail fold to run at every recursion depth,
        // including inside a redirect body. `enemy, AnimElem = 2, >= 0` now parses:
        // the redirect eats the first comma and its body `AnimElem = 2, >= 0` folds
        // into an AnimElemTail (the previously-documented limitation is lifted).
        match parse_str("enemy, AnimElem = 2, >= 0")
            .expect("redirected AnimElem tail should now parse")
        {
            Expr::Redirected { target, expr } => {
                assert_eq!(target, Redirect::Enemy);
                assert!(
                    matches!(*expr, Expr::AnimElemTail { .. }),
                    "redirect body should be the folded AnimElem tail, got {expr:?}"
                );
            }
            other => panic!("expected a redirected AnimElem tail, got {other:?}"),
        }
        // The non-redirected tail still evaluates correctly against a context.
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(2)], Value::Int(0));
        assert_eq!(ev("AnimElem = 2, >= 0", &ctx), Value::Int(1));
    }

    #[test]
    fn animelem_tail_default_context_never_fires_and_never_panics() {
        // With nothing seeded, AnimElemTime(N) reads 0 → "reached" (0 >= 0), then
        // the secondary `>= 0` also holds, so `AnimElem = 2, >= 0` is 1 by the
        // default-zero contract; `, > 0` is 0. Both are concrete, panic-free.
        let ctx = MockContext::new();
        assert_eq!(ev("AnimElem = 2, >= 0", &ctx), Value::Int(1));
        assert_eq!(ev("AnimElem = 2, > 0", &ctx), Value::Int(0));
    }

    // ---- Item (a): member arg passes the member NAME through the string seam ----

    #[test]
    fn member_arg_routes_by_name_not_by_nested_trigger_value() {
        // Task 4.11 item (a): `GetHitVar(fall.yvel)` passes the member NAME
        // ("fall.yvel") through `trigger_str` — it does NOT evaluate `fall.yvel`
        // as a nested numeric trigger and pass the resulting value. To prove the
        // old lossy path is gone, seed `fall.yvel` as a numeric trigger (the old
        // behavior would resolve it to 7 and look up GetHitVar(7)); the result
        // must come from the member seam instead.
        let ctx = MockContext::new()
            .with_trigger("fall.yvel", &[], Value::Int(7))
            .with_trigger("gethitvar", &[Value::Int(7)], Value::Int(99))
            .with_member_trigger("GetHitVar", "fall.yvel", Value::Float(-4.5));
        assert_eq!(ev("GetHitVar(fall.yvel)", &ctx), Value::Float(-4.5));
    }

    #[test]
    fn member_arg_distinct_names_distinct_values() {
        // Distinct member names yield distinct values through the string seam —
        // the property a numeric value-passing path could never provide.
        let ctx = MockContext::new()
            .with_member_trigger("GetHitVar", "animtype", Value::Int(2))
            .with_member_trigger("GetHitVar", "groundtype", Value::Int(1));
        assert_eq!(ev("GetHitVar(animtype)", &ctx), Value::Int(2));
        assert_eq!(ev("GetHitVar(groundtype)", &ctx), Value::Int(1));
    }

    #[test]
    fn member_arg_multi_segment_member_routes_by_name() {
        // A three-segment member is one identifier; it routes by its full name.
        let ctx = MockContext::new().with_member_trigger(
            "GetHitVar",
            "fall.envshake.time",
            Value::Int(5),
        );
        assert_eq!(ev("GetHitVar(fall.envshake.time)", &ctx), Value::Int(5));
        // Unknown member → safe default, never a panic.
        assert_eq!(
            ev("GetHitVar(fall.envshake.time)", &MockContext::new()),
            Value::Int(0)
        );
    }

    // ---- Gap 4: command string equality — OR chains, chained edge, case ----

    #[test]
    fn command_or_chain_fires_when_any_command_active() {
        // Real kfm.cns shape: `Command = "a" || Command = "b"`.
        let ctx = CommandCtx { active: vec!["b"] };
        assert_eq!(
            ev("Command = \"a\" || Command = \"b\"", &ctx),
            Value::Int(1)
        );
        let ctx = CommandCtx { active: vec!["a"] };
        assert_eq!(
            ev("Command = \"a\" || Command = \"b\"", &ctx),
            Value::Int(1)
        );
        let ctx = CommandCtx { active: vec!["c"] };
        assert_eq!(
            ev("Command = \"a\" || Command = \"b\"", &ctx),
            Value::Int(0)
        );
    }

    #[test]
    fn command_string_equality_is_case_insensitive_on_name() {
        // MUGEN command names compare case-insensitively; the seam delegates the
        // match to the context, which uses eq_ignore_ascii_case here.
        let ctx = CommandCtx {
            active: vec!["HoldFwd"],
        };
        assert_eq!(ev("command = \"holdfwd\"", &ctx), Value::Int(1));
        assert_eq!(ev("command = \"HOLDFWD\"", &ctx), Value::Int(1));
    }

    #[test]
    fn chained_var_eq_command_eq_string_does_not_route_through_seam() {
        // `var(2) = command = "holdfwd"` parses left-assoc as
        // `(var(2) = command) = "holdfwd"`. The OUTER `=` has a Binary lhs (not a
        // bare `command` ident), so command_string_compare returns None and it
        // falls to the numeric path: the Str RHS is bottom → the whole thing is 0,
        // REGARDLESS of whether "holdfwd" is active. Pin this so the seam's
        // narrow shape-match is not accidentally widened.
        let ctx = CommandCtx {
            active: vec!["holdfwd"],
        };
        assert_eq!(ev("var(2) = command = \"holdfwd\"", &ctx), Value::Int(0));
    }

    #[test]
    fn command_compare_against_empty_string_consults_seam() {
        // A degenerate `command = ""` still routes through the seam (the RHS is a
        // Str). A context with no "" command reports false → 0, never a panic.
        let ctx = CommandCtx { active: vec!["x"] };
        assert_eq!(ev("command = \"\"", &ctx), Value::Int(0));
        // And if a context did consider "" active, it would fire — proving the
        // empty name is forwarded verbatim.
        let ctx2 = CommandCtx { active: vec![""] };
        assert_eq!(ev("command = \"\"", &ctx2), Value::Int(1));
    }

    #[test]
    fn command_seam_only_triggers_for_eq_and_ne_not_other_ops() {
        // The string-equality routing is only for `=` / `!=`. A `command < "x"`
        // is not a command-seam shape (eval_eq_ne is not reached for `<`); the Str
        // operand is bottom and the comparison is 0. Never panics.
        let ctx = CommandCtx { active: vec!["x"] };
        assert_eq!(ev("command < \"x\"", &ctx), Value::Int(0));
        assert_eq!(ev("command > \"x\"", &ctx), Value::Int(0));
    }

    #[test]
    fn command_string_equality_never_panics_on_default_context() {
        // The default trait `command_active` is false; `command = "x"` is 0 and
        // composes safely. Already covered by Forge for the MockContext; here we
        // also confirm the bare-trait default path via a trigger-only context.
        struct TriggerOnly;
        impl EvalContext for TriggerOnly {
            fn trigger(&self, _n: &str, _a: &[Value]) -> Value {
                Value::DEFAULT
            }
        }
        assert_eq!(ev("command = \"x\"", &TriggerOnly), Value::Int(0));
        assert_eq!(ev("command != \"x\"", &TriggerOnly), Value::Int(1));
    }

    // =====================================================================
    // Proctor (task 4.11) — focused hardening of the three correctness
    // follow-ups, exercised end-to-end through parse->eval where the existing
    // suite only pinned the parse tree or a single eval case. Grouped by item.
    // =====================================================================

    // ---- Item (a): GetHitVar / member-arg STRING key — eval-path edges ----

    #[test]
    fn gethitvar_trigger_name_is_case_insensitive_through_eval() {
        // The member-keyed dispatch keys on the lowercased trigger name, so any
        // casing of `GetHitVar` must route through the string seam (item a). The
        // member name itself is forwarded verbatim and matched per the context's
        // own (case-insensitive) table.
        let ctx =
            MockContext::new().with_member_trigger("GetHitVar", "fall.yvel", Value::Float(-4.5));
        for src in [
            "GetHitVar(fall.yvel)",
            "gethitvar(fall.yvel)",
            "GETHITVAR(fall.yvel)",
            "GetHitVar(FALL.YVEL)",
        ] {
            assert_eq!(ev(src, &ctx), Value::Float(-4.5), "{src}");
        }
    }

    #[test]
    fn gethitvar_non_ident_arg_falls_through_to_numeric_path() {
        // The member seam only fires for a SINGLE BARE-IDENTIFIER argument. A
        // computed / numeric argument (`GetHitVar(1+1)`, `GetHitVar(var(0))`) is
        // not a member name, so it falls through to the ordinary numeric
        // `trigger` path with the EVALUATED arg — it must NOT touch `trigger_str`.
        // Seed both seams; only the numeric one must be consulted.
        let ctx = MockContext::new()
            .with_var(0, 2)
            .with_trigger("GetHitVar", &[Value::Int(2)], Value::Int(70))
            .with_member_trigger("GetHitVar", "2", Value::Int(999)); // must NOT be hit
                                                                     // `GetHitVar(1+1)` → arg evaluates to 2 → numeric trigger("GetHitVar",[2]).
        assert_eq!(ev("GetHitVar(1+1)", &ctx), Value::Int(70));
        // `GetHitVar(var(0))` → var(0)=2 → same numeric lookup.
        assert_eq!(ev("GetHitVar(var(0))", &ctx), Value::Int(70));
    }

    #[test]
    fn gethitvar_zero_and_multi_arg_forms_use_numeric_path() {
        // Only the single-bare-ident shape is member-keyed. A zero-arg or
        // multi-arg call is not, so it routes numerically and never panics.
        let ctx = MockContext::new()
            .with_trigger("GetHitVar", &[], Value::Int(11))
            .with_trigger("GetHitVar", &[Value::Int(1), Value::Int(2)], Value::Int(22));
        // Build the AST forms directly (the surface syntax is unusual but legal).
        let zero = Expr::Call {
            name: "GetHitVar".into(),
            args: vec![],
        };
        assert_eq!(eval(&zero, &ctx), Value::Int(11));
        let two = Expr::Call {
            name: "GetHitVar".into(),
            args: vec![Expr::Int(1), Expr::Int(2)],
        };
        assert_eq!(eval(&two, &ctx), Value::Int(22));
    }

    #[test]
    fn gethitvar_member_value_type_is_preserved_through_eval() {
        // The member seam returns whatever Value the field holds; an int field
        // stays int, a float field stays float — proving no lossy int-collapse.
        let ctx = MockContext::new()
            .with_member_trigger("GetHitVar", "animtype", Value::Int(2))
            .with_member_trigger("GetHitVar", "yvel", Value::Float(-6.25));
        let int_v = ev("GetHitVar(animtype)", &ctx);
        assert_eq!(int_v, Value::Int(2));
        assert!(int_v.is_int());
        let flt_v = ev("GetHitVar(yvel)", &ctx);
        assert_eq!(flt_v, Value::Float(-6.25));
        assert!(flt_v.is_float());
    }

    #[test]
    fn gethitvar_member_composes_in_arithmetic_and_comparison() {
        // The member read is an ordinary atom in a larger expression: it composes
        // in arithmetic (with float promotion) and comparisons, end to end.
        let ctx =
            MockContext::new().with_member_trigger("GetHitVar", "fall.yvel", Value::Float(-4.0));
        assert_eq!(ev("GetHitVar(fall.yvel) + 1.0", &ctx), Value::Float(-3.0));
        assert_eq!(ev("GetHitVar(fall.yvel) < 0", &ctx), Value::Int(1));
        assert_eq!(ev("GetHitVar(fall.yvel) > 0", &ctx), Value::Int(0));
    }

    #[test]
    fn gethitvar_default_trigger_str_yields_zero_not_panic() {
        // A context that does NOT override `trigger_str` inherits the trait
        // default (every member → 0). The eval path must use it and never panic.
        struct NoHitVars;
        impl EvalContext for NoHitVars {
            fn trigger(&self, _n: &str, _a: &[Value]) -> Value {
                // Even if `trigger` would return nonzero, the member path must use
                // `trigger_str` (default 0), proving the routing, not this value.
                Value::Int(42)
            }
        }
        assert_eq!(ev("GetHitVar(fall.yvel)", &NoHitVars), Value::Int(0));
        // And as a condition it never fires.
        assert_eq!(ev("GetHitVar(fall.yvel) || 0", &NoHitVars), Value::Int(0));
    }

    // ---- Item (b): TimeMod / AnimElemNo are NOT AnimElemTime semantics ----

    #[test]
    fn timemod_bare_equality_evaluates_as_ordinary_trigger_not_animelemtime() {
        // `TimeMod = 2` (no comma tail) is a plain equality on the `TimeMod`
        // trigger read — it must NOT be lowered through AnimElemTime. Seed the
        // `TimeMod` trigger and an AnimElemTime that the WRONG (old) path would
        // have hit; only the ordinary `TimeMod` read may be consulted.
        let ctx = MockContext::new()
            .with_trigger("TimeMod", &[], Value::Int(2))
            .with_trigger("animelemtime", &[Value::Int(2)], Value::Int(0)); // decoy
        assert_eq!(ev("TimeMod = 2", &ctx), Value::Int(1));
        let ctx = MockContext::new().with_trigger("TimeMod", &[], Value::Int(5));
        assert_eq!(ev("TimeMod = 2", &ctx), Value::Int(0));
    }

    #[test]
    fn animelemno_bare_equality_evaluates_as_ordinary_trigger() {
        // `AnimElemNo = 3` is an ordinary equality on the `AnimElemNo` read, not a
        // tail form. (Real MUGEN `AnimElemNo` is the parameterized `AnimElemNo(t)`;
        // either way it is not the comparison-tail family — item b.)
        let ctx = MockContext::new().with_trigger("AnimElemNo", &[], Value::Int(3));
        assert_eq!(ev("AnimElemNo = 3", &ctx), Value::Int(1));
        assert_eq!(ev("AnimElemNo = 9", &ctx), Value::Int(0));
    }

    #[test]
    fn timemod_comma_tail_evaluates_modulo_of_time_not_animelem() {
        // Task A: `TimeMod = d, c` is now its own node with the correct
        // modulo-of-time meaning `(Time % d) == c` — it must NOT carry AnimElemTime
        // semantics. Seed `Time` and confirm the modulo decides the result, with
        // `animelemtime` seeded as a decoy that the TimeMod path must ignore.
        // `timemod = 20, 19`: true exactly when Time % 20 == 19.
        let yes = MockContext::new()
            .with_trigger("Time", &[], Value::Int(39)) // 39 % 20 == 19
            .with_trigger("animelemtime", &[Value::Int(20)], Value::Int(0)); // decoy
        assert_eq!(ev("TimeMod = 20, 19", &yes), Value::Int(1));
        let no = MockContext::new().with_trigger("Time", &[], Value::Int(38)); // 38 % 20 == 18
        assert_eq!(ev("TimeMod = 20, 19", &no), Value::Int(0));
        // `TimeMod = 4, 3` (the spaced evilken form) at Time 7 → 7 % 4 == 3 → true.
        let ctx = MockContext::new().with_trigger("Time", &[], Value::Int(7));
        assert_eq!(ev("TimeMod = 4, 3", &ctx), Value::Int(1));
        // A zero divisor never fires (safe default, no panic).
        let ctx = MockContext::new().with_trigger("Time", &[], Value::Int(7));
        assert_eq!(ev("TimeMod = 0, 0", &ctx), Value::Int(0));
        // `AnimElemNo` is still NOT a comma-tail family → recoverable parse error.
        assert!(matches!(
            parse_str("AnimElemNo = 2, >= 0").unwrap_err(),
            ParseError::UnexpectedToken { .. }
        ));
    }

    // ---- Item (c): AnimElem tail binds at relational precedence (eval-path) ----

    #[test]
    fn animelem_tail_trailing_and_is_separate_conjunct_at_eval() {
        // Item (c), end-to-end: `AnimElem = 2, >= 0 && Time > 0` is
        // `(tail) && (Time>0)`, NOT a tail whose operand swallowed `0 && Time>0`.
        // Wire the tail to be TRUE and Time to make the second conjunct FALSE; the
        // AND must then be 0. If the `&&` had been swallowed into the operand the
        // result would differ, so this distinguishes the trees semantically.
        let ctx = MockContext::new()
            .with_trigger("animelemtime", &[Value::Int(2)], Value::Int(0)) // reached, time 0
            .with_trigger("Time", &[], Value::Int(0)); // Time > 0 is FALSE
        assert_eq!(ev("AnimElem = 2, >= 0 && Time > 0", &ctx), Value::Int(0));

        // Now make BOTH conjuncts true → 1.
        let ctx = MockContext::new()
            .with_trigger("animelemtime", &[Value::Int(2)], Value::Int(0))
            .with_trigger("Time", &[], Value::Int(5));
        assert_eq!(ev("AnimElem = 2, >= 0 && Time > 0", &ctx), Value::Int(1));

        // Tail FALSE, second conjunct TRUE → 0 (the && short-circuits on the tail).
        let ctx = MockContext::new()
            .with_trigger("animelemtime", &[Value::Int(2)], Value::Int(-1)) // not reached
            .with_trigger("Time", &[], Value::Int(5));
        assert_eq!(ev("AnimElem = 2, >= 0 && Time > 0", &ctx), Value::Int(0));
    }

    #[test]
    fn animelem_tail_trailing_or_is_separate_disjunct_at_eval() {
        // The `||` variant: `AnimElem = 2, > 5 || Time > 0`. Tail FALSE (time 0 is
        // not > 5) but Time>0 TRUE → the OR is 1, proving the disjunct is NOT
        // swallowed into the tail operand.
        let ctx = MockContext::new()
            .with_trigger("animelemtime", &[Value::Int(2)], Value::Int(0))
            .with_trigger("Time", &[], Value::Int(5));
        assert_eq!(ev("AnimElem = 2, > 5 || Time > 0", &ctx), Value::Int(1));
        // Both false → 0.
        let ctx = MockContext::new()
            .with_trigger("animelemtime", &[Value::Int(2)], Value::Int(0))
            .with_trigger("Time", &[], Value::Int(0));
        assert_eq!(ev("AnimElem = 2, > 5 || Time > 0", &ctx), Value::Int(0));
    }

    #[test]
    fn animelem_tail_operand_additive_is_absorbed_at_eval() {
        // The operand DOES absorb additive (`+ -`): `AnimElem = 2, >= 1 + 1`
        // compares element-time against 2. With time 2 it holds; with time 1 it
        // does not — proving `1 + 1` is the single operand `2`.
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(2)], Value::Int(2));
        assert_eq!(ev("AnimElem = 2, >= 1 + 1", &ctx), Value::Int(1));
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(2)], Value::Int(1));
        assert_eq!(ev("AnimElem = 2, >= 1 + 1", &ctx), Value::Int(0));
    }

    #[test]
    fn animelem_tail_trailing_relational_binds_tail_as_left_operand() {
        // A trailing relational (`= 1`) also binds the folded tail, not the
        // operand: `AnimElem = 2, >= 0 = 1` is `(tail) = 1`. The tail is 1 when
        // reached, so `(1) = 1` → 1; flip the tail to 0 and `(0) = 1` → 0.
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(2)], Value::Int(0));
        assert_eq!(ev("AnimElem = 2, >= 0 = 1", &ctx), Value::Int(1));
        let ctx = MockContext::new().with_trigger("animelemtime", &[Value::Int(2)], Value::Int(-1));
        assert_eq!(ev("AnimElem = 2, >= 0 = 1", &ctx), Value::Int(0));
    }

    #[test]
    fn animelem_tail_real_combo_shape_evaluates_correctly() {
        // A realistic combined shape mixing the tail with a parenthesized guard:
        // `(AnimElem = 2, >= 0) && Time > 0`. The parenthesized tail does NOT fold
        // (documented degrade), so this specific paren form is a parse error —
        // pin it, and confirm the UN-parenthesized equivalent evaluates correctly.
        assert!(matches!(
            parse_str("(AnimElem = 2, >= 0) && Time > 0"),
            Err(ParseError::UnexpectedToken { .. })
        ));
        let ctx = MockContext::new()
            .with_trigger("animelemtime", &[Value::Int(2)], Value::Int(0))
            .with_trigger("Time", &[], Value::Int(3));
        assert_eq!(ev("AnimElem = 2, >= 0 && Time > 0", &ctx), Value::Int(1));
    }

    // =====================================================================
    // Task A: TimeMod / HitDefAttr / Proj* two-argument trigger eval. These
    // pin the evaluation of the new comma-tail nodes for the evilken forms.
    // =====================================================================

    #[test]
    fn timemod_tail_evaluates_time_modulo_remainder() {
        // `timemod = 20, 19` is true exactly when `Time % 20 == 19`.
        for (time, expect) in [(19, 1), (39, 1), (59, 1), (18, 0), (20, 0), (0, 0)] {
            let ctx = MockContext::new().with_trigger("Time", &[], Value::Int(time));
            assert_eq!(
                ev("timemod = 20, 19", &ctx),
                Value::Int(expect),
                "Time={time}"
            );
        }
        // The spaced evilken form `TimeMod = 4, 3`.
        let ctx = MockContext::new().with_trigger("Time", &[], Value::Int(7));
        assert_eq!(ev("TimeMod = 4, 3", &ctx), Value::Int(1)); // 7 % 4 == 3
        let ctx = MockContext::new().with_trigger("Time", &[], Value::Int(6));
        assert_eq!(ev("TimeMod = 4, 3", &ctx), Value::Int(0)); // 6 % 4 == 2
    }

    #[test]
    fn timemod_tail_zero_divisor_never_fires_no_panic() {
        // A zero divisor is modulo-by-zero → the trigger never fires (safe default),
        // and crucially does not panic.
        let ctx = MockContext::new().with_trigger("Time", &[], Value::Int(5));
        assert_eq!(ev("timemod = 0, 0", &ctx), Value::Int(0));
    }

    #[test]
    fn timemod_tail_default_context_is_zero_remainder() {
        // With no Time seeded the context reads 0, so `timemod = d, 0` fires
        // (0 % d == 0) and `timemod = d, 1` does not. Concrete, panic-free.
        let ctx = MockContext::new();
        assert_eq!(ev("timemod = 9, 0", &ctx), Value::Int(1));
        assert_eq!(ev("timemod = 9, 8", &ctx), Value::Int(0));
    }

    #[test]
    fn compound_var_timemod_time_expression_evaluates_via_timemod_gate() {
        // The full evilken compound: only the TimeMod conjunct varies here. With the
        // other conjuncts satisfied, the expression fires iff `Time % 2 == 1`.
        let base = |time: i32| {
            MockContext::new()
                // `var(30)` routes through the typed `var` bank, not `trigger`.
                .with_var(30, 59)
                .with_trigger("p2life", &[], Value::Int(100))
                .with_trigger("Time", &[], Value::Int(time))
        };
        let src = "Var(30) = 59 && p2life > 0 && timemod = 2,1 && time > 2";
        // Time=5: var ok, p2life>0, 5%2==1 ok, 5>2 ok → fires.
        assert_eq!(ev(src, &base(5)), Value::Int(1));
        // Time=4: 4%2==0 != 1 → the TimeMod gate blocks it → 0.
        assert_eq!(ev(src, &base(4)), Value::Int(0));
        // Time=1: TimeMod ok (1%2==1) but `time > 2` fails → 0.
        assert_eq!(ev(src, &base(1)), Value::Int(0));
    }

    /// A context that reports a `HitDefAttr` match for a fixed standtype + code,
    /// to exercise the `hitdef_attr_matches` seam from the evaluator.
    struct HitDefCtx {
        standtype: &'static str,
        code: &'static str,
    }
    impl EvalContext for HitDefCtx {
        fn trigger(&self, _name: &str, _args: &[Value]) -> Value {
            Value::DEFAULT
        }
        fn hitdef_attr_matches(&self, standtype: &str, attr_codes: &[String]) -> bool {
            standtype.eq_ignore_ascii_case(self.standtype)
                && attr_codes.iter().any(|c| c.eq_ignore_ascii_case(self.code))
        }
    }

    #[test]
    fn hitdefattr_tail_routes_through_seam_and_composes_with_and() {
        // `hitdefattr = C, NA && movecontact`: the HitDefAttr tail routes through
        // the `hitdef_attr_matches` seam, and the `&& movecontact` survives.
        let ctx = HitDefCtx {
            standtype: "C",
            code: "NA",
        };
        // The seam matches → tail is 1; `movecontact` is unseeded (0) so the AND is
        // 0 — but the point is it EVALUATES rather than collapsing to const 0.
        assert_eq!(ev("hitdefattr = C, NA && movecontact", &ctx), Value::Int(0));
        // The bare tail is 1 (the seam matches C, NA).
        assert_eq!(ev("hitdefattr = C, NA", &ctx), Value::Int(1));
        // A different standtype / code does not match → 0.
        assert_eq!(ev("hitdefattr = S, NA", &ctx), Value::Int(0));
        assert_eq!(ev("hitdefattr = C, SA", &ctx), Value::Int(0));
        // A multi-code list fires if ANY code matches.
        assert_eq!(ev("hitdefattr = C, SA, NA", &ctx), Value::Int(1));
    }

    #[test]
    fn hitdefattr_tail_default_context_is_zero_no_panic() {
        // The default `hitdef_attr_matches` (no HitDef modeled) is false, so the
        // tail is 0 and the surrounding boolean simply does not fire — never panics.
        let ctx = MockContext::new();
        assert_eq!(ev("hitdefattr = C, NA", &ctx), Value::Int(0));
        assert_eq!(ev("hitdefattr = C, NA && movecontact", &ctx), Value::Int(0));
    }

    #[test]
    fn proj_tail_always_zero_projectiles_unimplemented() {
        // The projectile-info tail parses but always evaluates to 0 (projectiles
        // are unimplemented), so it never fires and never panics.
        let ctx = MockContext::new();
        assert_eq!(ev("projcontact2000 = 1, < 20", &ctx), Value::Int(0));
        assert_eq!(ev("ProjHit1000 = 1, > 5", &ctx), Value::Int(0));
        // It still composes in a boolean without collapsing the whole expression.
        let ctx = MockContext::new().with_trigger("Time", &[], Value::Int(5));
        assert_eq!(
            ev("projcontact2000 = 1, < 20 || time > 2", &ctx),
            Value::Int(1)
        );
    }
}
