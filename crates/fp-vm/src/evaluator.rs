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
    }
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

/// Evaluates `=` / `!=`, special-casing a range literal on the right (§6).
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
    let l = eval_inner(lhs, ctx);
    let r = eval_inner(rhs, ctx);
    eval_binary_values(op, l, r)
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
fn arith(l: Eval, r: Eval, int_op: fn(i32, i32) -> i32, float_op: fn(f32, f32) -> f32) -> Eval {
    if Eval::either_bottom(l, r) {
        return Eval::Bottom;
    }
    if l.is_float() || r.is_float() {
        Eval::Float(float_op(l.to_float(), r.to_float()))
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
        "min" if args.len() == 2 => minmax(
            eval_inner(&args[0], ctx),
            eval_inner(&args[1], ctx),
            true,
        ),
        "max" if args.len() == 2 => minmax(
            eval_inner(&args[0], ctx),
            eval_inner(&args[1], ctx),
            false,
        ),
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
        // ---- Otherwise: a parameterized trigger ----
        _ => {
            // Evaluate the arguments left-to-right into Values for the lookup.
            let mut evaluated = Vec::with_capacity(args.len());
            for a in args {
                evaluated.push(eval_inner(a, ctx).into_value());
            }
            Eval::from_value(ctx.trigger(name, &evaluated))
        }
    }
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
    use crate::parser::parse_str;
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

    /// An in-memory [`EvalContext`] for evaluator tests, with a deterministic RNG
    /// seam so `random` is reproducible.
    #[derive(Default)]
    struct MockContext {
        triggers: HashMap<String, Value>,
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
        fn var(&self, index: i32) -> Value {
            self.vars.get(&index).copied().map_or(Value::DEFAULT, Value::Int)
        }
        fn fvar(&self, index: i32) -> Value {
            self.fvars.get(&index).copied().map_or(Value::DEFAULT, Value::Float)
        }
        fn sysvar(&self, index: i32) -> Value {
            self.sysvars.get(&index).copied().map_or(Value::DEFAULT, Value::Int)
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
        let ctx = CountingCtx { reads: Cell::new(0) };
        assert_eq!(eval(&parse_str("0 && probe").unwrap(), &ctx), Value::Int(0));
        assert_eq!(ctx.reads.get(), 0, "&& should short-circuit and not read probe");

        // `1 || probe` must NOT read `probe`.
        let ctx2 = CountingCtx { reads: Cell::new(0) };
        assert_eq!(eval(&parse_str("1 || probe").unwrap(), &ctx2), Value::Int(1));
        assert_eq!(ctx2.reads.get(), 0, "|| should short-circuit and not read probe");

        // `1 && probe` MUST read it once.
        let ctx3 = CountingCtx { reads: Cell::new(0) };
        assert_eq!(eval(&parse_str("1 && probe").unwrap(), &ctx3), Value::Int(1));
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
        let ctx = CountingCtx { reads: Cell::new(0) };
        assert_eq!(eval(&parse_str("cond(1, 42, untaken)").unwrap(), &ctx), Value::Int(42));
        assert_eq!(ctx.reads.get(), 0, "cond must not evaluate the untaken branch");

        // cond(0, untaken, 7) → 7, never touching `untaken`.
        let ctx2 = CountingCtx { reads: Cell::new(0) };
        assert_eq!(eval(&parse_str("cond(0, untaken, 7)").unwrap(), &ctx2), Value::Int(7));
        assert_eq!(ctx2.reads.get(), 0, "cond must not evaluate the untaken branch");
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
        let ctx = MockContext::new()
            .with_trigger("animelemtime", &[Value::Int(2)], Value::Int(5));
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
            .with_redirect(Redirect::Enemy, MockContext::new().with_trigger("X", &[], Value::Int(10)))
            .with_redirect(Redirect::EnemyNear(2), MockContext::new().with_trigger("X", &[], Value::Int(20)));
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
            "1 / 0", "5 % 0", "ln(0)", "(0 - 1) ** 0.5", "\"x\"", "(1,2)",
            "5.0 % 2", "floor(ln(0))", "(1 / 0) + 1", "abs(ln(0))",
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
        let ctx = CountingCtx { reads: Cell::new(0) };
        // `(1/0)` is Bottom → falsey → short-circuit.
        assert_eq!(eval(&parse_str("(1 / 0) && probe").unwrap(), &ctx), Value::Int(0));
        assert_eq!(ctx.reads.get(), 0, "&& must not read RHS when LHS is bottom/false");
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
        let ctx = MockContext::new().with_trigger("cond", &[Value::Int(1), Value::Int(2)], Value::Int(77));
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
        assert_eq!(eval(&parse_str("var(0)").unwrap(), &TriggerOnly), Value::Int(11));
        assert_eq!(eval(&parse_str("fvar(0)").unwrap(), &TriggerOnly), Value::Float(2.5));
        assert_eq!(eval(&parse_str("sysvar(0)").unwrap(), &TriggerOnly), Value::Int(99));
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
        let ctx = MockContext::new()
            .with_trigger("two", &[Value::Int(3), Value::Int(8)], Value::Int(55));
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
        assert_ne!(a, b, "successive random(..) draws should advance the RNG state");
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
        let ctx_a = MockContext::new().with_seed(2024).with_var(0, 7).with_fvar(0, 1.5);
        let ctx_b = MockContext::new().with_seed(2024).with_var(0, 7).with_fvar(0, 1.5);
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
            ev("(animtype = [4,5]) && SelfAnimExist(5047 + animtype)", &ctx2),
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
        let enemy = MockContext::new()
            .with_trigger("animelemtime", &[Value::Int(2)], Value::Int(5));
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
        assert_eq!(ctx.resolves.get(), 0, "untaken redirect branch must not resolve");

        // When taken, it resolves exactly once.
        let ctx2 = CountingRedirect {
            resolves: Cell::new(0),
            child: MockContext::new().with_trigger("life", &[], Value::Int(1)),
        };
        assert_eq!(
            eval(&parse_str("cond(1, (enemy, life), 9)").unwrap(), &ctx2),
            Value::Int(1)
        );
        assert_eq!(ctx2.resolves.get(), 1, "taken redirect branch resolves once");
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
}
