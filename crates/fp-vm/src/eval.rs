//! # Runtime value model and evaluation context
//!
//! This module is the bridge between the parsed [`Expr`](crate::parser::Expr)
//! AST and a live game entity. It is stage 3's *foundation* (task 4.3): it
//! defines the runtime [`Value`] type, the set of [redirection targets](Redirect)
//! a trigger expression can hop through, and the [`EvalContext`] trait the
//! tree-walk evaluator (task 4.4) queries for trigger and variable values. The
//! evaluator itself lives in task 4.4 — this module deliberately ships only the
//! model, the trait, and a deterministic `MockContext` test helper (an
//! in-memory [`EvalContext`] living in this module's `#[cfg(test)]` block).
//!
//! ## Value model
//!
//! Per [`docs/knowledge-base/07-evaluator-semantics.md`][kb] §1, MUGEN values are
//! dynamically **int** (`i32`) or **float** (`f32`); there is no declared type.
//! [`Value`] is a tagged union of exactly those two, with the coercion helpers
//! the evaluator needs:
//!
//! - [`Value::as_bool`] — "nonzero is true" (negative ints and tiny nonzero
//!   floats are true; `0` / `0.0` / `-0.0` are false).
//! - [`Value::to_float`] — widen to `f32` for the float arithmetic path.
//! - [`Value::to_int`] — **saturating** `f32`-to-`i32` narrowing (out-of-range
//!   clamps to [`i32::MIN`]/[`i32::MAX`], `NaN` becomes `0`) per §4 / CB4.
//!
//! The error sentinel "bottom" from the spec is *not* modeled as a [`Value`]
//! variant here: trigger/variable lookups that fail return the safe default `0`
//! (`Value::Int(0)`) directly, in keeping with the engine-wide "never crash on
//! bad content" rule and the documented "an expression that generates an error
//! will never cause a trigger to fire" behavior. Modeling an explicit `Bottom`
//! within arithmetic is left to the evaluator (task 4.4).
//!
//! ## Redirection
//!
//! A MUGEN trigger may be prefixed with a *redirection* — `parent,life`,
//! `enemy,pos x`, `helper(1234),stateno` — which evaluates the trailing trigger
//! against a *different* entity. [`Redirect`] enumerates the standard targets,
//! and [`EvalContext::redirect`] resolves one to another `&dyn EvalContext`
//! (or [`None`] when the target does not exist — e.g. there is no parent, or the
//! requested helper id is absent).
//!
//! ## Trigger lookup convention
//!
//! The evaluator walks an [`Expr`](crate::parser::Expr) and, on reaching an
//! [`Expr::Ident`](crate::parser::Expr::Ident) or
//! [`Expr::Call`](crate::parser::Expr::Call), looks the name up via
//! [`EvalContext::trigger`], passing the (already-evaluated) call arguments as a
//! `&[Value]` slice. A bare trigger like `Time` is looked up with an empty
//! argument slice; a parameterized trigger like `var(0)` is looked up with one
//! argument. Names are matched **case-insensitively** (MUGEN is case-insensitive);
//! implementations should compare with [`str::eq_ignore_ascii_case`].
//! Convenience methods [`EvalContext::var`], [`EvalContext::fvar`], and
//! [`EvalContext::sysvar`] exist for the integer/float/system variable banks,
//! which the evaluator accesses often enough to warrant a typed path.
//!
//! Unknown trigger names, out-of-range variable indices, and unresolved
//! redirections **never panic**: they yield `Value::Int(0)` (or `None` for a
//! redirect), matching the spec's safe-default contract.
//!
//! ## String-keyed queries (`command = "name"`, `GetHitVar(member)`)
//!
//! Most triggers are numeric, but two MUGEN forms are keyed by a *name*, not a
//! number, and [`Value`] has no string variant — so each gets its own seam:
//!
//! - **`command = "name"`** — the `command` trigger is compared against a quoted
//!   *command name* (`command = "fwd"`). The evaluator routes `command = "name"`
//!   (in either operand order) through [`EvalContext::command_active`], a boolean
//!   string-keyed seam, rather than the numeric [`EvalContext::trigger`] path.
//! - **`GetHitVar(member)` / `const(member)`** — `GetHitVar` selects a hit field
//!   *by name* (`GetHitVar(fall.yvel)`, `GetHitVar(xveladd)`) and `const` reads a
//!   character's authored constant *by name* (`const(velocity.walk.fwd.x)`,
//!   `const(movement.yaccel)`). The member name is an arbitrary label, so the
//!   evaluator routes these calls through [`EvalContext::trigger_str`], passing
//!   the member name verbatim as the key rather than collapsing it to a number.
//!   See that method and the [`evaluator`](crate::evaluator) module (its private
//!   `MEMBER_KEYED_TRIGGERS` set) for the full member-keyed function set.
//!
//! [kb]: ../../../docs/knowledge-base/07-evaluator-semantics.md

use std::fmt;

/// A runtime MUGEN expression value.
///
/// MUGEN values are dynamically one of two numeric types (see
/// [`docs/knowledge-base/07-evaluator-semantics.md`][kb] §1):
///
/// - [`Value::Int`] — a 32-bit signed integer (`i32`). Literals without a
///   decimal point (`7`, `-3`, `0`) and the result of comparisons / logical /
///   bitwise operators.
/// - [`Value::Float`] — a single-precision float (`f32`). Literals with a
///   decimal point (`7.0`, `.5`, `3.14`) and any arithmetic where one operand is
///   already a float.
///
/// The error sentinel "bottom" is intentionally not represented here; failed
/// lookups substitute `Value::Int(0)` (see the [module docs](self)).
///
/// [kb]: ../../../docs/knowledge-base/07-evaluator-semantics.md
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// A 32-bit signed integer value.
    Int(i32),
    /// A single-precision floating-point value.
    Float(f32),
}

impl Value {
    /// The safe-default value substituted for unknown triggers, bad variable
    /// indices, and other recoverable lookup failures: integer `0`.
    ///
    /// Using a named constant keeps the "never crash → 0" contract explicit at
    /// every call site.
    pub const DEFAULT: Value = Value::Int(0);

    /// Returns `true` if this value is "truthy" by MUGEN's nonzero rule.
    ///
    /// Any nonzero value is true — including negative integers and tiny nonzero
    /// floats. Exactly `0`, `0.0`, and `-0.0` are false (per
    /// [`07-evaluator-semantics.md`][kb] §5; note `0.0 == -0.0` in IEEE-754, so
    /// both compare false). A `NaN` float is treated as false: `NaN != 0.0`
    /// in IEEE-754 would make it true, but a `NaN` here represents an error
    /// (bottom), and "an expression that generates an error will never cause a
    /// trigger to fire", so it must read as false.
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_vm::eval::Value;
    ///
    /// assert!(Value::Int(1).as_bool());
    /// assert!(Value::Int(-3).as_bool());
    /// assert!(Value::Float(0.0001).as_bool());
    /// assert!(!Value::Int(0).as_bool());
    /// assert!(!Value::Float(0.0).as_bool());
    /// assert!(!Value::Float(-0.0).as_bool());
    /// assert!(!Value::Float(f32::NAN).as_bool());
    /// ```
    ///
    /// [kb]: ../../../docs/knowledge-base/07-evaluator-semantics.md
    #[must_use]
    pub fn as_bool(self) -> bool {
        match self {
            Value::Int(i) => i != 0,
            // `NaN != 0.0` is `true` in IEEE-754, so guard NaN explicitly so it
            // reads as false (an error value never fires a trigger).
            Value::Float(f) => f != 0.0 && !f.is_nan(),
        }
    }

    /// Widens this value to an `f32`, promoting an integer if necessary.
    ///
    /// This is the float-promotion path: when either operand of an arithmetic /
    /// comparison op is a float, the other is promoted with `to_float` and the
    /// operation runs in `f32` (per [`07-evaluator-semantics.md`][kb] §1).
    ///
    /// Note that `i32`-to-`f32` can lose precision for magnitudes above
    /// 2^24; this mirrors MUGEN/Ikemen, which also computes the float path in
    /// single precision.
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_vm::eval::Value;
    ///
    /// assert_eq!(Value::Int(7).to_float(), 7.0);
    /// assert_eq!(Value::Float(3.5).to_float(), 3.5);
    /// ```
    ///
    /// [kb]: ../../../docs/knowledge-base/07-evaluator-semantics.md
    #[must_use]
    pub fn to_float(self) -> f32 {
        match self {
            Value::Int(i) => i as f32,
            Value::Float(f) => f,
        }
    }

    /// Narrows this value to an `i32` with **saturating** float-to-int rounding.
    ///
    /// An integer is returned unchanged. A float is narrowed per the CB4 rule
    /// ([`07-evaluator-semantics.md`][kb] §4):
    ///
    /// - truncation is **toward zero** (`3.9` → `3`, `-3.9` → `-3`);
    /// - out-of-range magnitudes **saturate** to [`i32::MIN`] / [`i32::MAX`]
    ///   rather than wrapping (`2147483648.0` → `i32::MAX`, `+∞` → `i32::MAX`,
    ///   `-∞` → `i32::MIN`);
    /// - `NaN` (an error / bottom value) narrows to `0`.
    ///
    /// Rust's `as i32` cast has saturated since Rust 1.45 and maps `NaN` to `0`,
    /// so the cast already satisfies the contract; it is used directly here, and
    /// the unit tests pin the saturation and `NaN → 0` behavior so a future
    /// refactor cannot silently regress it.
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_vm::eval::Value;
    ///
    /// assert_eq!(Value::Int(42).to_int(), 42);
    /// assert_eq!(Value::Float(3.9).to_int(), 3);
    /// assert_eq!(Value::Float(-3.9).to_int(), -3);
    /// assert_eq!(Value::Float(3e9).to_int(), i32::MAX); // saturates
    /// assert_eq!(Value::Float(-3e9).to_int(), i32::MIN);
    /// assert_eq!(Value::Float(f32::INFINITY).to_int(), i32::MAX);
    /// assert_eq!(Value::Float(f32::NAN).to_int(), 0);
    /// ```
    ///
    /// [kb]: ../../../docs/knowledge-base/07-evaluator-semantics.md
    #[must_use]
    pub fn to_int(self) -> i32 {
        match self {
            Value::Int(i) => i,
            // Rust's float-to-int `as` cast saturates to MIN/MAX for
            // out-of-range values and yields 0 for NaN (stable since 1.45),
            // which is exactly the documented CB4 narrowing contract.
            Value::Float(f) => f as i32,
        }
    }

    /// Returns `true` if this value is the [`Value::Int`] variant.
    #[must_use]
    pub fn is_int(self) -> bool {
        matches!(self, Value::Int(_))
    }

    /// Returns `true` if this value is the [`Value::Float`] variant.
    #[must_use]
    pub fn is_float(self) -> bool {
        matches!(self, Value::Float(_))
    }
}

impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Int(i)
    }
}

impl From<f32> for Value {
    fn from(f: f32) -> Self {
        Value::Float(f)
    }
}

impl From<bool> for Value {
    /// A boolean becomes the MUGEN int `1` (true) or `0` (false); comparisons
    /// and logical operators yield int `1`/`0`.
    fn from(b: bool) -> Self {
        Value::Int(i32::from(b))
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
        }
    }
}

/// A redirection target: which *other* entity a trigger should be evaluated
/// against.
///
/// In MUGEN, a trigger can be prefixed with a redirection that retargets the
/// rest of the expression at a related entity, e.g. `parent,life`,
/// `enemy,pos x`, or `helper(1234),stateno`. The evaluator resolves a `Redirect`
/// via [`EvalContext::redirect`], obtaining the target's `&dyn EvalContext`
/// (or [`None`] if the target does not exist), and continues evaluating the
/// trailing trigger against it.
///
/// See [`07-evaluator-semantics.md`][kb] and the MUGEN redirection keyword list
/// ([trigger.html]). Targets that take an optional id (e.g. `target`,
/// `playerid`, `helper`) carry it in the variant.
///
/// # CB8: how `enemy(n)` is represented
///
/// MUGEN's `enemy` redirect accepts an optional index — `enemy`, `enemy(0)`,
/// `enemy(1)`, … — where `enemy(n)` selects the n-th nearest enemy and `enemy`
/// is shorthand for `enemy(0)` (the nearest). `enemynear(n)` has the *same*
/// "n-th nearest enemy" meaning. Rather than add a separate index to
/// [`Redirect::Enemy`] and duplicate that selection logic, the parser **lowers**
/// the indexed form: a positive index `enemy(n)` (`n > 0`) becomes
/// [`Redirect::EnemyNear(n)`](Redirect::EnemyNear), while bare `enemy` and the
/// degenerate `enemy(0)` both become [`Redirect::Enemy`] (≡ `enemynear(0)`,
/// the nearest enemy). The index is therefore never silently dropped — it is
/// carried on the `EnemyNear` variant. [`Redirect::Enemy`] is kept as its own
/// variant (distinct from `EnemyNear(0)`) because it is the overwhelmingly
/// common written form and a concrete entity may resolve "the enemy" without
/// computing a full nearest-ordering.
///
/// [kb]: ../../../docs/knowledge-base/07-evaluator-semantics.md
/// [trigger.html]: https://www.elecbyte.com/mugendocs/trigger.html
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Redirect {
    /// `parent` — the helper's immediate creator. Errors (→ `None`) if the
    /// current entity is a root player rather than a helper.
    Parent,
    /// `root` — the root player at the top of the helper chain.
    Root,
    /// `helper(id)` — a helper owned by the current entity with the given
    /// helper id.
    Helper(i32),
    /// `target` / `target(id)` — a player currently being hit by this entity.
    /// `None` selects "any/most recent" target; `Some(id)` selects the target
    /// with the matching `targetid`.
    Target(Option<i32>),
    /// `enemy` — the opposing player (equivalent to `enemy(0)`, the nearest
    /// enemy in standard 1v1 play). The parser lowers bare `enemy` and
    /// `enemy(0)` to this variant; see the [type-level CB8 note](Redirect).
    Enemy,
    /// `enemynear(n)` — the n-th nearest enemy (`enemynear(0)` is the closest).
    ///
    /// The parser also lowers the positive-index `enemy(n)` (`n > 0`) form to
    /// this variant, since `enemy(n)` and `enemynear(n)` share the "n-th nearest
    /// enemy" meaning (see the [type-level CB8 note](Redirect)).
    EnemyNear(i32),
    /// `partner` — the teammate of the current entity (tag / simul modes).
    Partner,
    /// `playerid(id)` — the player whose unique runtime id matches `id`.
    PlayerId(i32),
}

impl fmt::Display for Redirect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Redirect::Parent => f.write_str("parent"),
            Redirect::Root => f.write_str("root"),
            Redirect::Helper(id) => write!(f, "helper({id})"),
            Redirect::Target(None) => f.write_str("target"),
            Redirect::Target(Some(id)) => write!(f, "target({id})"),
            Redirect::Enemy => f.write_str("enemy"),
            Redirect::EnemyNear(n) => write!(f, "enemynear({n})"),
            Redirect::Partner => f.write_str("partner"),
            Redirect::PlayerId(id) => write!(f, "playerid({id})"),
        }
    }
}

/// Which variable bank an in-expression assignment (`:=`) writes to.
///
/// MUGEN's assignment-in-expression operator targets one of the four indexed
/// variable banks — `var(n) := e`, `fvar(n) := e`, `sysvar(n) := e`, and
/// `sysfvar(n) := e` (see [`EvalContext::assign`] and
/// [`Expr::Assign`](crate::parser::Expr::Assign)). The bank fixes the element
/// type the assigned value is coerced to: the integer banks store an `i32`, the
/// float banks an `f32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AssignBank {
    /// `var(n)` — the integer variable bank.
    Var,
    /// `fvar(n)` — the float variable bank.
    FVar,
    /// `sysvar(n)` — the system integer variable bank.
    SysVar,
    /// `sysfvar(n)` — the system float variable bank.
    SysFVar,
}

impl fmt::Display for AssignBank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AssignBank::Var => "var",
            AssignBank::FVar => "fvar",
            AssignBank::SysVar => "sysvar",
            AssignBank::SysFVar => "sysfvar",
        })
    }
}

/// The interface the tree-walk evaluator (task 4.4) queries to read values from
/// a live game entity.
///
/// `EvalContext` is an **abstract** interface — it is deliberately decoupled
/// from any concrete entity type (`fp-character` does not exist yet). A real
/// character, a helper, an explod, or the `MockContext` test double all
/// implement it. The evaluator walks an [`Expr`](crate::parser::Expr) and, for
/// each leaf, calls one of the read methods below; for a redirected
/// sub-expression it first calls [`redirect`](EvalContext::redirect) to obtain
/// the target context and recurses against that.
///
/// ## Contract: never panic, default to `0`
///
/// Every method here is infallible by design. An unknown trigger name, an
/// out-of-range variable index, or a redirection to a non-existent entity must
/// **not** panic: trigger / variable reads return [`Value::DEFAULT`]
/// (`Value::Int(0)`) and [`redirect`](EvalContext::redirect) returns [`None`].
/// This matches the engine-wide "never crash on bad content" rule and MUGEN's
/// "an erroring expression never fires a trigger" behavior.
///
/// ## Trigger / variable lookup convention
///
/// - [`trigger`](EvalContext::trigger) takes a case-insensitive name and an
///   already-evaluated argument slice. Bare triggers (`Time`) pass `&[]`;
///   parameterized triggers (`AnimElem` → `AnimElem = 2`, `var(0)`) pass their
///   arguments. Implementations should match names with
///   [`str::eq_ignore_ascii_case`].
/// - [`var`](EvalContext::var) / [`fvar`](EvalContext::fvar) /
///   [`sysvar`](EvalContext::sysvar) are typed fast paths for the integer,
///   float, and system variable banks. The default implementations route
///   through [`trigger`](EvalContext::trigger) (`"var"` / `"fvar"` / `"sysvar"`)
///   so a context that only overrides `trigger` still works; a concrete entity
///   may override them to read its variable arrays directly.
/// - [`trigger_str`](EvalContext::trigger_str) is the **member-keyed** path for
///   triggers whose argument is a named field rather than a number —
///   `GetHitVar(member)` and `const(member)`. The member name is passed through
///   verbatim so the context can identify *which* hit field or authored constant
///   was requested (see that method).
pub trait EvalContext {
    /// Reads a trigger or state value by (case-insensitive) name, given its
    /// already-evaluated arguments.
    ///
    /// `args` is empty for a bare trigger (`Time`, `StateNo`) and holds the
    /// evaluated call / parameterized-trigger arguments otherwise (`var(0)` →
    /// `[Value::Int(0)]`, `AnimElem` with `= 2` is evaluated by the comparison
    /// op, not passed here).
    ///
    /// Returns [`Value::DEFAULT`] (`0`) for any unrecognized name or invalid
    /// argument — it must never panic.
    fn trigger(&self, name: &str, args: &[Value]) -> Value;

    /// Reads a trigger whose argument is a **named field key** rather than a
    /// numeric value, given the (case-insensitive) trigger name and the
    /// (case-preserved) member key.
    ///
    /// ## Why a separate, string-keyed path exists
    ///
    /// A handful of MUGEN triggers are parameterized by a *named field*, not a
    /// number. The canonical cases are `GetHitVar` and `const`:
    /// `GetHitVar(fall.yvel)`, `GetHitVar(xveladd)`, `GetHitVar(animtype)` each
    /// select a distinct field of the most-recent hit, and
    /// `const(velocity.walk.fwd.x)`, `const(size.ground.front)`,
    /// `const(movement.yaccel)` each read a distinct authored character constant
    /// — all identified **by name**. The member name is an arbitrary, often
    /// dotted label (`fall.yvel`, `velocity.walk.fwd.x`), not an index, so it
    /// cannot be carried through the numeric [`trigger`](EvalContext::trigger)
    /// path: [`Value`] has no string variant, so collapsing the member to a
    /// number would lose *which* field was asked for. This method is the seam
    /// that preserves the member name end-to-end.
    ///
    /// The evaluator routes a member-arg call — a call whose single argument is a
    /// bare identifier, for the recognized member-keyed trigger names
    /// (`GetHitVar` and `const`; see the [`evaluator`](crate::evaluator) module's
    /// private `MEMBER_KEYED_TRIGGERS` set) — here, passing the identifier
    /// verbatim as `key`. All other calls keep using the numeric
    /// [`trigger`](EvalContext::trigger) path with evaluated [`Value`] arguments.
    ///
    /// ## Contract
    ///
    /// Like every method here it is infallible: an unrecognized trigger name or
    /// member key returns [`Value::DEFAULT`] (`0`), never a panic. The trigger
    /// name should be matched case-insensitively (MUGEN is case-insensitive);
    /// MUGEN's `GetHitVar` and `const` member names are conventionally compared
    /// case-insensitively too, so an implementation should use
    /// [`str::eq_ignore_ascii_case`] against its field / constant tables.
    ///
    /// The default implementation returns [`Value::DEFAULT`] for every key, so a
    /// context that does not model hit vars or character constants reports `0`
    /// for every member read — making the routing additive and regression-free
    /// until a resolver lands. A concrete entity (task Phase 5, `fp-character`)
    /// overrides this to read its `GetHitVar` field table and its authored
    /// `const(...)` constant table.
    fn trigger_str(&self, name: &str, key: &str) -> Value {
        let _ = (name, key);
        Value::DEFAULT
    }

    /// Reads integer variable `var(index)`.
    ///
    /// Returns [`Value::Int`]. An out-of-range index yields `Value::Int(0)`.
    /// The default implementation forwards to
    /// [`trigger`](EvalContext::trigger) with name `"var"`.
    fn var(&self, index: i32) -> Value {
        self.trigger("var", &[Value::Int(index)])
    }

    /// Reads float variable `fvar(index)`.
    ///
    /// Returns [`Value::Float`]. An out-of-range index yields `Value::Int(0)`
    /// (the safe default) — callers that need a float default can coerce with
    /// [`Value::to_float`]. The default implementation forwards to
    /// [`trigger`](EvalContext::trigger) with name `"fvar"`.
    fn fvar(&self, index: i32) -> Value {
        self.trigger("fvar", &[Value::Int(index)])
    }

    /// Reads system integer variable `sysvar(index)`.
    ///
    /// Returns [`Value::Int`]. An out-of-range index yields `Value::Int(0)`.
    /// The default implementation forwards to
    /// [`trigger`](EvalContext::trigger) with name `"sysvar"`.
    fn sysvar(&self, index: i32) -> Value {
        self.trigger("sysvar", &[Value::Int(index)])
    }

    /// Resolves a [`Redirect`] to the target entity's evaluation context.
    ///
    /// Returns `Some(&dyn EvalContext)` for the redirected entity, or [`None`]
    /// if the target does not exist (no parent, missing helper id, no current
    /// target, etc.). The evaluator treats `None` as "the whole redirected
    /// sub-expression is bottom / `0`".
    ///
    /// The default implementation returns [`None`] for every target, which is
    /// the correct behavior for a leaf entity with no relations (and for a test
    /// double that does not model redirection). Concrete entities override this
    /// to walk their relationship graph.
    fn redirect(&self, target: Redirect) -> Option<&dyn EvalContext> {
        let _ = target;
        None
    }

    /// Answers a string-keyed query: whether the named *command* (or, more
    /// generally, the named string-valued condition) is currently active.
    ///
    /// This is the seam for MUGEN's `command = "name"` trigger (task 4.10, gap
    /// 4). MUGEN's `command` trigger is special: it is compared against a quoted
    /// *command name* (`command = "fwd"`, `command = "x"`), and the comparison is
    /// true iff that named command fired this tick. Because the right-hand side
    /// is a string — which has no numeric [`Value`] — the evaluator cannot model
    /// it as an ordinary numeric trigger read; instead, when it sees
    /// `command = "name"` (in either operand order) it calls this method with the
    /// (case-sensitively-preserved) command name and yields int `1`/`0`.
    ///
    /// The default implementation returns `false` for every name, so a context
    /// that does not model commands (a leaf entity, the test doubles) reports no
    /// command as active — i.e. `command = "x"` evaluates to `0` rather than
    /// firing. A concrete entity overrides this to consult its command buffer.
    /// Like every other method here it is infallible and must never panic.
    ///
    /// Names are MUGEN command labels; matching is left to the implementation
    /// (MUGEN command names are conventionally compared case-insensitively, so an
    /// implementation should use [`str::eq_ignore_ascii_case`] against its
    /// command table).
    fn command_active(&self, name: &str) -> bool {
        let _ = name;
        false
    }

    /// Answers the two-argument `HitDefAttr = <standtype>, <attr-list>` trigger
    /// (Task A): whether the character's currently-active `HitDef` has a stand-type
    /// matching `standtype` **and** an attack-attribute code present in
    /// `attr_codes`.
    ///
    /// This is the seam for MUGEN's `HitDefAttr` trigger, which tests the active
    /// `HitDef`'s `attr` (e.g. `S, NA`). `standtype` is a single upper-cased letter
    /// (`"S"` standing / `"C"` crouching / `"A"` air); `attr_codes` is a list of
    /// upper-cased 2-char attack codes (`"NA"`, `"SA"`, `"HA"`, `"NT"`, `"ST"`,
    /// `"HT"`, `"NP"`, `"SP"`, `"HP"`) of which the active `HitDef`'s code must
    /// match at least one. Both parts are pre-normalized to upper-case by the
    /// parser ([`Expr::HitDefAttrTail`](crate::parser::Expr::HitDefAttrTail)).
    ///
    /// The default implementation returns `false` for every query, so a context
    /// that does not model a `HitDef` (a leaf entity, the test doubles) reports no
    /// match — i.e. `HitDefAttr = C, NA` evaluates to `0` rather than firing. A
    /// concrete entity overrides this to test its active `HitDef`'s attribute. Like
    /// every other method here it is infallible and must never panic.
    fn hitdef_attr_matches(&self, standtype: &str, attr_codes: &[String]) -> bool {
        let _ = (standtype, attr_codes);
        false
    }

    /// Draws one deterministic pseudo-random value for the `random` trigger.
    ///
    /// This is the **RNG seam**. The evaluator never owns RNG state — for
    /// frame-perfect netplay rollback and replay the random seed must live with
    /// the game entity and be part of saved / rolled-back state (see
    /// [`07-evaluator-semantics.md`][kb] §11 and
    /// [`Rng`](crate::evaluator::Rng)). A concrete entity overrides this to
    /// advance its own [`Rng`](crate::evaluator::Rng) (the Park–Miller
    /// minimal-standard LCG) and return the next raw draw in `0..=2^31-2`; the
    /// evaluator then maps it to the requested range (`random` → `[0,999]`,
    /// `random(lo,hi)` → inclusive `[lo,hi]`).
    ///
    /// The default implementation returns `0`, which keeps a context that does
    /// not model randomness deterministic (every `random` reads `0`). It is
    /// **not** suitable for actual gameplay — override it. Returning a fixed
    /// value rather than touching the OS RNG preserves the never-panic,
    /// deterministic contract.
    ///
    /// [kb]: ../../../docs/knowledge-base/07-evaluator-semantics.md
    fn random(&self) -> i32 {
        0
    }

    /// Performs MUGEN's in-expression assignment `var(n) := e` (and the `fvar` /
    /// `sysvar` / `sysfvar` bank variants), writing `value` to slot `index` of
    /// `bank` and returning the value the *assignment expression* evaluates to.
    ///
    /// This is the **assignment hook** that makes the otherwise read-only
    /// `EvalContext` trigger interface able to back MUGEN's `:=` operator (see
    /// [`Expr::Assign`](crate::parser::Expr::Assign) and its evaluation in the
    /// [`evaluator`](crate::evaluator)). The evaluator routes a parsed
    /// `var(5) := 8000` here as `assign(AssignBank::Var, 5, Value::Int(8000))`,
    /// and the returned [`Value`] becomes the value of the whole assignment
    /// sub-expression — so `-1 + 0 * (var(31) := 2)` both sets `var(31)` and
    /// evaluates the surrounding arithmetic.
    ///
    /// Because the trait is queried through a shared `&self`, a concrete entity
    /// that actually persists the write must do so through interior mutability
    /// (or buffer it and flush after the eval). The method returns the value that
    /// a subsequent read of the same slot would yield, so the integer banks
    /// return the narrowed [`Value::Int`] and the float banks the widened
    /// [`Value::Float`].
    ///
    /// ## Contract
    ///
    /// Like every other method here it is infallible and must never panic. The
    /// default implementation is a **no-op write** that returns the value coerced
    /// to the bank's element type, so a context that does not model variables
    /// (a leaf entity, a read-only test double) still lets a `:=` expression parse
    /// and evaluate to a sensible value without storing anything. A concrete
    /// entity overrides this to write its variable arrays. An out-of-range
    /// `index` should be a silent no-op (returning the would-be value), matching
    /// the engine-wide "bad input → safe default, never crash" rule.
    fn assign(&self, bank: AssignBank, index: i32, value: Value) -> Value {
        let _ = index;
        // Coerce to the bank's element type so the returned value matches a later
        // read, even though this default does not persist the write.
        match bank {
            AssignBank::Var | AssignBank::SysVar => Value::Int(value.to_int()),
            AssignBank::FVar | AssignBank::SysFVar => Value::Float(value.to_float()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    // ---- Value::as_bool — nonzero is true ----

    #[test]
    fn as_bool_int_nonzero_is_true() {
        assert!(Value::Int(1).as_bool());
        assert!(Value::Int(-1).as_bool());
        assert!(Value::Int(i32::MAX).as_bool());
        assert!(Value::Int(i32::MIN).as_bool());
        assert!(!Value::Int(0).as_bool());
    }

    #[test]
    fn as_bool_float_nonzero_is_true() {
        assert!(Value::Float(0.0001).as_bool());
        assert!(Value::Float(-0.0001).as_bool());
        assert!(!Value::Float(0.0).as_bool());
        // -0.0 == 0.0 in IEEE-754, so negative zero is also false.
        assert!(!Value::Float(-0.0).as_bool());
    }

    #[test]
    fn as_bool_nan_is_false() {
        // A NaN represents an error value; an erroring expr never fires.
        assert!(!Value::Float(f32::NAN).as_bool());
    }

    // ---- Value::to_float — promotion ----

    #[test]
    fn to_float_promotes_int() {
        assert_eq!(Value::Int(7).to_float(), 7.0_f32);
        assert_eq!(Value::Int(-3).to_float(), -3.0_f32);
        assert_eq!(Value::Float(3.5).to_float(), 3.5_f32);
    }

    // ---- Value::to_int — saturating float->int narrowing (CB4) ----

    #[test]
    fn to_int_truncates_toward_zero() {
        assert_eq!(Value::Float(3.9).to_int(), 3);
        assert_eq!(Value::Float(-3.9).to_int(), -3);
        assert_eq!(Value::Float(0.0).to_int(), 0);
        assert_eq!(Value::Int(42).to_int(), 42);
    }

    #[test]
    fn to_int_saturates_on_overflow() {
        assert_eq!(Value::Float(3e9).to_int(), i32::MAX);
        assert_eq!(Value::Float(-3e9).to_int(), i32::MIN);
        // Just past i32::MAX as f32 magnitudes.
        assert_eq!(Value::Float(2_147_483_648.0).to_int(), i32::MAX);
        assert_eq!(Value::Float(-2_147_483_904.0).to_int(), i32::MIN);
    }

    #[test]
    fn to_int_infinities_saturate() {
        assert_eq!(Value::Float(f32::INFINITY).to_int(), i32::MAX);
        assert_eq!(Value::Float(f32::NEG_INFINITY).to_int(), i32::MIN);
    }

    #[test]
    fn to_int_nan_is_zero() {
        // The critical CB4 case: NaN/bottom narrows to 0, never to a garbage int.
        assert_eq!(Value::Float(f32::NAN).to_int(), 0);
    }

    // ---- Conversions & predicates ----

    #[test]
    fn from_conversions() {
        assert_eq!(Value::from(5_i32), Value::Int(5));
        assert_eq!(Value::from(2.5_f32), Value::Float(2.5));
        assert_eq!(Value::from(true), Value::Int(1));
        assert_eq!(Value::from(false), Value::Int(0));
    }

    #[test]
    fn variant_predicates() {
        assert!(Value::Int(0).is_int());
        assert!(!Value::Int(0).is_float());
        assert!(Value::Float(0.0).is_float());
        assert!(!Value::Float(0.0).is_int());
    }

    #[test]
    fn value_display() {
        assert_eq!(Value::Int(-7).to_string(), "-7");
        assert_eq!(Value::Float(1.5).to_string(), "1.5");
    }

    #[test]
    fn default_is_int_zero() {
        assert_eq!(Value::DEFAULT, Value::Int(0));
    }

    // ---- Redirect display ----

    #[test]
    fn redirect_display() {
        assert_eq!(Redirect::Parent.to_string(), "parent");
        assert_eq!(Redirect::Root.to_string(), "root");
        assert_eq!(Redirect::Helper(1234).to_string(), "helper(1234)");
        assert_eq!(Redirect::Target(None).to_string(), "target");
        assert_eq!(Redirect::Target(Some(2)).to_string(), "target(2)");
        assert_eq!(Redirect::Enemy.to_string(), "enemy");
        assert_eq!(Redirect::EnemyNear(1).to_string(), "enemynear(1)");
        assert_eq!(Redirect::Partner.to_string(), "partner");
        assert_eq!(Redirect::PlayerId(7).to_string(), "playerid(7)");
    }

    // =====================================================================
    // MockContext — a deterministic in-memory EvalContext for evaluator tests
    // (used by task 4.4). Lives in the test module so it never ships in the
    // non-test binary, while still being exercised here.
    // =====================================================================

    /// Renders `(name, args)` into a stable, case-insensitive lookup key.
    ///
    /// `Value` deliberately is not `Hash`/`Eq` (it carries an `f32`), so the
    /// mock keys triggers on a rendered string instead of `Vec<Value>`. The
    /// name is lowercased for MUGEN's case-insensitive matching and the args are
    /// rendered via [`Value`]'s `Display`, which is exact for the small integer
    /// args real triggers use.
    fn trigger_key(name: &str, args: &[Value]) -> String {
        let mut key = name.to_ascii_lowercase();
        for arg in args {
            key.push('|');
            key.push_str(&arg.to_string());
        }
        key
    }

    /// Renders `(name, member)` into a stable, case-insensitive lookup key for
    /// the member-keyed trigger path (`GetHitVar(member)`). Both the trigger name
    /// and the member are lowercased so the lookup is case-insensitive.
    fn member_key(name: &str, member: &str) -> String {
        format!(
            "{}#{}",
            name.to_ascii_lowercase(),
            member.to_ascii_lowercase()
        )
    }

    /// A simple in-memory [`EvalContext`] backed by lookup tables, for
    /// deterministic evaluation tests.
    ///
    /// Triggers are keyed by a rendered `(lowercased name, args)` string (see
    /// [`trigger_key`]); variable banks are keyed by index; and redirections map
    /// a [`Redirect`] to a boxed child `MockContext`. Anything not present
    /// resolves to the safe default (`Value::Int(0)` / `None`), so tests only
    /// populate what they assert on.
    #[derive(Default)]
    struct MockContext {
        /// Trigger values keyed by the rendered (name, args) string.
        triggers: HashMap<String, Value>,
        /// Member-keyed trigger values (e.g. `GetHitVar(member)`), keyed by the
        /// rendered `(lowercased name, lowercased member)` string.
        member_triggers: HashMap<String, Value>,
        /// Integer variable bank (`var(i)`). Interior-mutable so the assignment
        /// hook (`var(i) := e`) can persist a write through the shared `&self`.
        vars: RefCell<HashMap<i32, i32>>,
        /// Float variable bank (`fvar(i)`), interior-mutable for assignment.
        fvars: RefCell<HashMap<i32, f32>>,
        /// System integer variable bank (`sysvar(i)`), interior-mutable for
        /// assignment.
        sysvars: RefCell<HashMap<i32, i32>>,
        /// System float variable bank (`sysfvar(i)`), interior-mutable for
        /// assignment.
        sysfvars: RefCell<HashMap<i32, f32>>,
        /// Redirection targets.
        redirects: HashMap<Redirect, Box<MockContext>>,
    }

    impl MockContext {
        fn new() -> Self {
            Self::default()
        }

        /// Registers a trigger value under the given name + args.
        fn with_trigger(mut self, name: &str, args: &[Value], value: Value) -> Self {
            self.triggers.insert(trigger_key(name, args), value);
            self
        }

        /// Registers a member-keyed trigger value (e.g. `GetHitVar(member)`),
        /// keyed case-insensitively on both the trigger name and the member.
        fn with_member_trigger(mut self, name: &str, member: &str, value: Value) -> Self {
            self.member_triggers.insert(member_key(name, member), value);
            self
        }

        fn with_var(self, index: i32, value: i32) -> Self {
            self.vars.borrow_mut().insert(index, value);
            self
        }

        fn with_fvar(self, index: i32, value: f32) -> Self {
            self.fvars.borrow_mut().insert(index, value);
            self
        }

        fn with_sysvar(self, index: i32, value: i32) -> Self {
            self.sysvars.borrow_mut().insert(index, value);
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
                .borrow()
                .get(&index)
                .copied()
                .map_or(Value::DEFAULT, Value::Int)
        }

        fn fvar(&self, index: i32) -> Value {
            self.fvars
                .borrow()
                .get(&index)
                .copied()
                .map_or(Value::DEFAULT, Value::Float)
        }

        fn sysvar(&self, index: i32) -> Value {
            self.sysvars
                .borrow()
                .get(&index)
                .copied()
                .map_or(Value::DEFAULT, Value::Int)
        }

        fn assign(&self, bank: AssignBank, index: i32, value: Value) -> Value {
            // Persist the write through interior mutability and return the value
            // the bank would read back (narrowed/widened to the bank type).
            match bank {
                AssignBank::Var => {
                    let v = value.to_int();
                    self.vars.borrow_mut().insert(index, v);
                    Value::Int(v)
                }
                AssignBank::FVar => {
                    let v = value.to_float();
                    self.fvars.borrow_mut().insert(index, v);
                    Value::Float(v)
                }
                AssignBank::SysVar => {
                    let v = value.to_int();
                    self.sysvars.borrow_mut().insert(index, v);
                    Value::Int(v)
                }
                AssignBank::SysFVar => {
                    let v = value.to_float();
                    self.sysfvars.borrow_mut().insert(index, v);
                    Value::Float(v)
                }
            }
        }

        fn redirect(&self, target: Redirect) -> Option<&dyn EvalContext> {
            self.redirects
                .get(&target)
                .map(|boxed| boxed.as_ref() as &dyn EvalContext)
        }
    }

    // ---- MockContext lookups ----

    #[test]
    fn mock_trigger_lookup_hit_and_default() {
        let ctx = MockContext::new()
            .with_trigger("Time", &[], Value::Int(30))
            .with_trigger("StateNo", &[], Value::Int(200));
        assert_eq!(ctx.trigger("Time", &[]), Value::Int(30));
        assert_eq!(ctx.trigger("StateNo", &[]), Value::Int(200));
        // Unknown trigger → safe default 0, never a panic.
        assert_eq!(ctx.trigger("NoSuchTrigger", &[]), Value::Int(0));
    }

    #[test]
    fn mock_trigger_is_case_insensitive() {
        let ctx = MockContext::new().with_trigger("Time", &[], Value::Int(30));
        // MUGEN is case-insensitive; lookups must match regardless of case.
        assert_eq!(ctx.trigger("time", &[]), Value::Int(30));
        assert_eq!(ctx.trigger("TIME", &[]), Value::Int(30));
        assert_eq!(ctx.trigger("TiMe", &[]), Value::Int(30));
    }

    #[test]
    fn mock_parameterized_trigger_uses_args() {
        let ctx = MockContext::new()
            .with_trigger("animelemtime", &[Value::Int(2)], Value::Int(5))
            .with_trigger("animelemtime", &[Value::Int(3)], Value::Int(-1));
        assert_eq!(ctx.trigger("AnimElemTime", &[Value::Int(2)]), Value::Int(5));
        assert_eq!(
            ctx.trigger("AnimElemTime", &[Value::Int(3)]),
            Value::Int(-1)
        );
        // Different arg → not found → default.
        assert_eq!(ctx.trigger("AnimElemTime", &[Value::Int(9)]), Value::Int(0));
    }

    #[test]
    fn mock_trigger_str_distinguishes_member_names() {
        // The member-keyed seam (`GetHitVar(member)`) must return distinct values
        // for distinct member NAMES — that is the whole point of the string key
        // (the member name cannot be recovered from a numeric Value).
        let ctx = MockContext::new()
            .with_member_trigger("GetHitVar", "fall.yvel", Value::Float(-4.5))
            .with_member_trigger("GetHitVar", "xveladd", Value::Int(7))
            .with_member_trigger("GetHitVar", "animtype", Value::Int(2));
        assert_eq!(
            ctx.trigger_str("GetHitVar", "fall.yvel"),
            Value::Float(-4.5)
        );
        assert_eq!(ctx.trigger_str("GetHitVar", "xveladd"), Value::Int(7));
        assert_eq!(ctx.trigger_str("GetHitVar", "animtype"), Value::Int(2));
        // Case-insensitive on both name and member.
        assert_eq!(
            ctx.trigger_str("gethitvar", "FALL.YVEL"),
            Value::Float(-4.5)
        );
        // Unknown member → safe default 0, never a panic.
        assert_eq!(ctx.trigger_str("GetHitVar", "nosuchfield"), Value::DEFAULT);
    }

    #[test]
    fn default_trigger_str_is_safe_default() {
        // A context that does not model hit vars inherits the default
        // `trigger_str`, returning 0 for every member key (never a panic).
        struct TriggerOnly;
        impl EvalContext for TriggerOnly {
            fn trigger(&self, _name: &str, _args: &[Value]) -> Value {
                Value::Int(1)
            }
        }
        let ctx = TriggerOnly;
        assert_eq!(ctx.trigger_str("GetHitVar", "fall.yvel"), Value::DEFAULT);
        assert_eq!(ctx.trigger_str("GetHitVar", ""), Value::DEFAULT);
    }

    #[test]
    fn mock_variable_banks() {
        let ctx = MockContext::new()
            .with_var(0, 42)
            .with_fvar(1, 3.5)
            .with_sysvar(2, 7);
        assert_eq!(ctx.var(0), Value::Int(42));
        assert_eq!(ctx.fvar(1), Value::Float(3.5));
        assert_eq!(ctx.sysvar(2), Value::Int(7));
        // Unset indices → safe default 0.
        assert_eq!(ctx.var(99), Value::Int(0));
        assert_eq!(ctx.fvar(99), Value::Int(0));
        assert_eq!(ctx.sysvar(99), Value::Int(0));
    }

    #[test]
    fn default_var_methods_route_through_trigger() {
        // A context that only implements `trigger` still answers var/fvar/sysvar
        // via the default trait methods.
        struct TriggerOnly;
        impl EvalContext for TriggerOnly {
            fn trigger(&self, name: &str, args: &[Value]) -> Value {
                match (name, args) {
                    ("var", [Value::Int(0)]) => Value::Int(11),
                    ("fvar", [Value::Int(0)]) => Value::Float(2.5),
                    ("sysvar", [Value::Int(0)]) => Value::Int(99),
                    _ => Value::DEFAULT,
                }
            }
        }
        let ctx = TriggerOnly;
        assert_eq!(ctx.var(0), Value::Int(11));
        assert_eq!(ctx.fvar(0), Value::Float(2.5));
        assert_eq!(ctx.sysvar(0), Value::Int(99));
        assert_eq!(ctx.var(1), Value::Int(0));
    }

    // ---- Redirection resolution ----

    #[test]
    fn mock_redirect_resolves_to_child_context() {
        // root,life — the redirected context answers its own triggers.
        let root = MockContext::new().with_trigger("Life", &[], Value::Int(1000));
        let ctx = MockContext::new()
            .with_trigger("Life", &[], Value::Int(400))
            .with_redirect(Redirect::Root, root);

        // Self vs. redirected reads differ.
        assert_eq!(ctx.trigger("Life", &[]), Value::Int(400));
        let redirected = ctx.redirect(Redirect::Root).expect("root exists");
        assert_eq!(redirected.trigger("Life", &[]), Value::Int(1000));
    }

    #[test]
    fn mock_redirect_missing_target_is_none() {
        let ctx = MockContext::new();
        // No relations registered → every redirect is None (never a panic).
        assert!(ctx.redirect(Redirect::Parent).is_none());
        assert!(ctx.redirect(Redirect::Root).is_none());
        assert!(ctx.redirect(Redirect::Helper(1)).is_none());
        assert!(ctx.redirect(Redirect::Enemy).is_none());
        assert!(ctx.redirect(Redirect::Target(None)).is_none());
    }

    #[test]
    fn mock_redirect_distinguishes_ids() {
        let h1 = MockContext::new().with_trigger("StateNo", &[], Value::Int(1));
        let h2 = MockContext::new().with_trigger("StateNo", &[], Value::Int(2));
        let ctx = MockContext::new()
            .with_redirect(Redirect::Helper(1), h1)
            .with_redirect(Redirect::Helper(2), h2);

        assert_eq!(
            ctx.redirect(Redirect::Helper(1))
                .map(|c| c.trigger("StateNo", &[])),
            Some(Value::Int(1))
        );
        assert_eq!(
            ctx.redirect(Redirect::Helper(2))
                .map(|c| c.trigger("StateNo", &[])),
            Some(Value::Int(2))
        );
        // An unregistered id is None.
        assert!(ctx.redirect(Redirect::Helper(3)).is_none());
    }

    #[test]
    fn mock_nested_redirect_chain() {
        // enemy,root,life — a redirect target can itself redirect.
        let enemy_root = MockContext::new().with_trigger("Life", &[], Value::Int(750));
        let enemy = MockContext::new().with_redirect(Redirect::Root, enemy_root);
        let ctx = MockContext::new().with_redirect(Redirect::Enemy, enemy);

        let life = ctx
            .redirect(Redirect::Enemy)
            .and_then(|e| e.redirect(Redirect::Root))
            .map(|r| r.trigger("Life", &[]));
        assert_eq!(life, Some(Value::Int(750)));
    }

    #[test]
    fn eval_context_is_object_safe() {
        // The evaluator holds `&dyn EvalContext`; confirm the trait is object
        // safe by coercing a concrete impl to a trait object and using it.
        let ctx = MockContext::new().with_trigger("Time", &[], Value::Int(5));
        let dynamic: &dyn EvalContext = &ctx;
        assert_eq!(dynamic.trigger("Time", &[]), Value::Int(5));
        assert_eq!(dynamic.var(0), Value::Int(0));
    }

    // =====================================================================
    // Proctor (task 4.3): additional edge-case / error-path / MUGEN-semantics
    // coverage layered on top of Forge's tests. Grouped by acceptance
    // criterion so each AC is demonstrably exercised.
    // =====================================================================

    // ---- AC1: Value::as_bool — finer-grained MUGEN "nonzero is true" cases ----

    #[test]
    fn as_bool_subnormal_float_is_true() {
        // A subnormal (denormalized) float is still nonzero, hence true.
        assert!(Value::Float(f32::MIN_POSITIVE).as_bool());
        assert!(Value::Float(-f32::MIN_POSITIVE).as_bool());
        // The very smallest positive subnormal is also nonzero → true.
        assert!(Value::Float(f32::from_bits(1)).as_bool());
    }

    #[test]
    fn as_bool_infinities_are_true() {
        // ±Inf are nonzero and not NaN, so they read as true (only an explicit
        // NaN/bottom is forced false).
        assert!(Value::Float(f32::INFINITY).as_bool());
        assert!(Value::Float(f32::NEG_INFINITY).as_bool());
    }

    #[test]
    fn as_bool_extreme_ints_are_true() {
        assert!(Value::Int(i32::MAX).as_bool());
        assert!(Value::Int(i32::MIN).as_bool());
    }

    // ---- AC1: Value::to_float — promotion edge cases & precision note ----

    #[test]
    fn to_float_extreme_ints() {
        // i32::MAX/MIN promote to their nearest-representable f32.
        assert_eq!(Value::Int(i32::MAX).to_float(), i32::MAX as f32);
        assert_eq!(Value::Int(i32::MIN).to_float(), i32::MIN as f32);
        // i32::MIN is exactly representable (it's -2^31, a power of two).
        assert_eq!(Value::Int(i32::MIN).to_float(), -2_147_483_648.0_f32);
    }

    #[test]
    fn to_float_loses_precision_above_2_pow_24() {
        // Documented behavior: i32→f32 loses precision past 2^24 (matches
        // MUGEN/Ikemen single-precision float path). 16_777_217 (2^24 + 1) is
        // not representable as f32 and rounds to 16_777_216.
        assert_eq!(Value::Int(16_777_217).to_float(), 16_777_216.0_f32);
        // Below 2^24 the conversion is exact.
        assert_eq!(Value::Int(16_777_215).to_float(), 16_777_215.0_f32);
    }

    #[test]
    fn to_float_preserves_nan() {
        // A float value carries through to_float as-is; NaN stays NaN (it is
        // as_bool/to_int that special-case it, not the widening path).
        assert!(Value::Float(f32::NAN).to_float().is_nan());
    }

    // ---- AC1: Value::to_int — saturating CB4 narrowing, boundary cases ----

    #[test]
    fn to_int_tiny_floats_truncate_to_zero() {
        // Magnitudes below 1 truncate toward zero → 0 (not rounded away).
        assert_eq!(Value::Float(0.9).to_int(), 0);
        assert_eq!(Value::Float(-0.9).to_int(), 0);
        assert_eq!(Value::Float(0.4999).to_int(), 0);
        assert_eq!(Value::Float(-0.0).to_int(), 0);
        assert_eq!(Value::Float(f32::MIN_POSITIVE).to_int(), 0);
    }

    #[test]
    fn to_int_exact_boundary_floats() {
        // The largest f32 strictly below 2^31 rounds down to a representable
        // value (2^31 - 128 = 2_147_483_520) and must NOT saturate.
        let just_under = 2_147_483_520.0_f32;
        assert_eq!(Value::Float(just_under).to_int(), 2_147_483_520);
        // i32::MIN (-2^31) is exactly representable as f32 and must round-trip,
        // not saturate to MIN by accident of clamping.
        assert_eq!(Value::Float(i32::MIN as f32).to_int(), i32::MIN);
    }

    #[test]
    fn to_int_int_variant_is_identity_at_extremes() {
        // An Int value passes through to_int untouched at the boundaries — no
        // float round-trip that could perturb i32::MAX/MIN.
        assert_eq!(Value::Int(i32::MAX).to_int(), i32::MAX);
        assert_eq!(Value::Int(i32::MIN).to_int(), i32::MIN);
    }

    #[test]
    fn to_int_large_finite_floats_saturate_both_signs() {
        assert_eq!(Value::Float(f32::MAX).to_int(), i32::MAX);
        assert_eq!(Value::Float(f32::MIN).to_int(), i32::MIN); // f32::MIN is the most-negative finite
    }

    #[test]
    fn coercion_round_trip_int_through_float() {
        // Small ints survive an int→float→int round trip exactly.
        for i in [-1000, -1, 0, 1, 1000, 1 << 20] {
            assert_eq!(Value::Float(Value::Int(i).to_float()).to_int(), i);
        }
    }

    // ---- AC1: From / Display edge cases ----

    #[test]
    fn from_bool_is_always_int_variant() {
        // Booleans must become the int 1/0, never a float — comparisons and
        // logical ops yield int per §5.
        assert!(Value::from(true).is_int());
        assert!(Value::from(false).is_int());
        assert_eq!(Value::from(true), Value::Int(1));
    }

    #[test]
    fn display_extremes_and_negative_zero() {
        assert_eq!(Value::Int(i32::MIN).to_string(), "-2147483648");
        assert_eq!(Value::Int(i32::MAX).to_string(), "2147483647");
        // Rust formats -0.0 with the sign; just confirm it does not panic and is
        // a stable rendering (used by the mock's trigger_key).
        assert_eq!(Value::Float(0.0).to_string(), "0");
    }

    #[test]
    fn partial_eq_distinguishes_int_and_float_zero() {
        // Int(0) and Float(0.0) are different *values* even though both are
        // falsey — the tagged union must not conflate them.
        assert_ne!(Value::Int(0), Value::Float(0.0));
        assert_ne!(Value::Int(1), Value::Float(1.0));
        assert_eq!(Value::Int(7), Value::Int(7));
        assert_eq!(Value::Float(7.0), Value::Float(7.0));
    }

    // ---- AC2: Redirect — distinct variants are distinct map keys ----

    #[test]
    fn redirect_variants_are_distinct_keys() {
        // Eq/Hash must treat each redirection (and its id) as a distinct target,
        // so the evaluator can store/resolve them in a map without collision.
        let mut map = HashMap::new();
        for (i, r) in [
            Redirect::Parent,
            Redirect::Root,
            Redirect::Helper(1),
            Redirect::Helper(2),
            Redirect::Target(None),
            Redirect::Target(Some(0)),
            Redirect::Target(Some(5)),
            Redirect::Enemy,
            Redirect::EnemyNear(0),
            Redirect::EnemyNear(1),
            Redirect::Partner,
            Redirect::PlayerId(7),
        ]
        .into_iter()
        .enumerate()
        {
            map.insert(r, i);
        }
        // All twelve are distinct keys.
        assert_eq!(map.len(), 12);
        // Equal variants collide (same key overwrites), proving Eq is value-based.
        map.insert(Redirect::Helper(1), 99);
        assert_eq!(map.get(&Redirect::Helper(1)), Some(&99));
        assert_eq!(map.len(), 12);
    }

    #[test]
    fn redirect_enemy_distinct_from_enemynear_zero() {
        // `enemy` and `enemynear(0)` are conceptually "nearest enemy" but are
        // modeled as distinct variants; confirm they do not compare/hash equal.
        assert_ne!(Redirect::Enemy, Redirect::EnemyNear(0));
        let mut map = HashMap::new();
        map.insert(Redirect::Enemy, 1);
        map.insert(Redirect::EnemyNear(0), 2);
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn redirect_target_none_distinct_from_some() {
        assert_ne!(Redirect::Target(None), Redirect::Target(Some(0)));
        assert_eq!(Redirect::Target(Some(3)), Redirect::Target(Some(3)));
    }

    // ---- AC3/AC5: trigger lookup — argument arity & robustness ----

    #[test]
    fn trigger_arity_is_significant() {
        // Same name with different arg counts is a different lookup: a bare
        // `Foo` and `Foo(0)` must not alias.
        let ctx = MockContext::new()
            .with_trigger("Foo", &[], Value::Int(1))
            .with_trigger("Foo", &[Value::Int(0)], Value::Int(2));
        assert_eq!(ctx.trigger("Foo", &[]), Value::Int(1));
        assert_eq!(ctx.trigger("Foo", &[Value::Int(0)]), Value::Int(2));
        // An unseen arity falls back to the safe default.
        assert_eq!(
            ctx.trigger("Foo", &[Value::Int(0), Value::Int(1)]),
            Value::DEFAULT
        );
    }

    #[test]
    fn trigger_float_arg_is_keyed_distinctly_from_int_arg() {
        // The mock keys triggers on the Display rendering of (name, args), so
        // args that *render differently* are distinct lookups. Note the
        // intentional caveat: `Int(0)` and `Float(0.0)` BOTH render as "0" and
        // therefore collide in the mock's key space (see
        // `mock_int_and_float_arg_render_to_same_key`). Use a non-integral float
        // here so the two keys genuinely differ.
        let ctx = MockContext::new()
            .with_trigger("p", &[Value::Int(0)], Value::Int(10))
            .with_trigger("p", &[Value::Float(0.5)], Value::Int(20));
        assert_eq!(ctx.trigger("p", &[Value::Int(0)]), Value::Int(10));
        assert_eq!(ctx.trigger("p", &[Value::Float(0.5)]), Value::Int(20));
    }

    #[test]
    fn mock_int_and_float_arg_render_to_same_key() {
        // Documents a *known limitation of the test helper* (not the model):
        // because `trigger_key` renders args via Display and `0.0_f32` prints as
        // "0", `Int(0)` and `Float(0.0)` map to the same mock key, so the second
        // insert overwrites the first. The real evaluator (4.4) compares
        // evaluated arg Values directly and is unaffected; this test simply pins
        // the mock's behavior so a future MockContext change is a conscious one.
        let ctx = MockContext::new()
            .with_trigger("q", &[Value::Int(0)], Value::Int(10))
            .with_trigger("q", &[Value::Float(0.0)], Value::Int(20));
        // Both forms read back the last-written value (collision).
        assert_eq!(ctx.trigger("q", &[Value::Int(0)]), Value::Int(20));
        assert_eq!(ctx.trigger("q", &[Value::Float(0.0)]), Value::Int(20));
    }

    #[test]
    fn trigger_empty_name_and_odd_args_never_panic() {
        // Pathological inputs must return the safe default, never panic.
        let ctx = MockContext::new();
        assert_eq!(ctx.trigger("", &[]), Value::DEFAULT);
        assert_eq!(ctx.trigger("   ", &[]), Value::DEFAULT);
        let many: Vec<Value> = (0..256).map(Value::Int).collect();
        assert_eq!(ctx.trigger("whatever", &many), Value::DEFAULT);
        // Mixed-type and NaN args do not panic during key rendering.
        assert_eq!(
            ctx.trigger("mix", &[Value::Float(f32::NAN), Value::Int(-1)]),
            Value::DEFAULT
        );
    }

    #[test]
    fn trigger_stored_float_value_is_returned_as_float() {
        // A trigger may legitimately hold a float (e.g. PosX); the value type is
        // preserved through lookup, it is not coerced to int.
        let ctx = MockContext::new().with_trigger("PosX", &[], Value::Float(-3.25));
        let v = ctx.trigger("PosX", &[]);
        assert_eq!(v, Value::Float(-3.25));
        assert!(v.is_float());
    }

    // ---- AC3: variable banks — negative indices, type guarantees ----

    #[test]
    fn variable_negative_and_extreme_indices_default_safely() {
        let ctx = MockContext::new().with_var(0, 5);
        // Negative / extreme indices are not registered → safe default 0.
        assert_eq!(ctx.var(-1), Value::DEFAULT);
        assert_eq!(ctx.var(i32::MIN), Value::DEFAULT);
        assert_eq!(ctx.var(i32::MAX), Value::DEFAULT);
        assert_eq!(ctx.fvar(-1), Value::DEFAULT);
        assert_eq!(ctx.sysvar(-1), Value::DEFAULT);
        // The registered one still reads back.
        assert_eq!(ctx.var(0), Value::Int(5));
    }

    #[test]
    fn variable_banks_preserve_their_types() {
        // var/sysvar are int banks; fvar is a float bank. A hit returns the
        // correctly-typed variant.
        let ctx = MockContext::new()
            .with_var(3, -7)
            .with_fvar(3, -7.5)
            .with_sysvar(3, 12);
        assert!(ctx.var(3).is_int());
        assert!(ctx.fvar(3).is_float());
        assert!(ctx.sysvar(3).is_int());
        assert_eq!(ctx.var(3), Value::Int(-7));
        assert_eq!(ctx.fvar(3), Value::Float(-7.5));
        assert_eq!(ctx.sysvar(3), Value::Int(12));
    }

    #[test]
    fn fvar_default_is_int_zero_per_safe_default_contract() {
        // The documented contract: an unset fvar yields the safe default
        // Value::Int(0) (callers coerce with to_float if they need a float).
        let ctx = MockContext::new();
        let d = ctx.fvar(0);
        assert_eq!(d, Value::Int(0));
        assert!(d.is_int());
        // ...and it coerces to 0.0 cleanly for a float-expecting caller.
        assert_eq!(d.to_float(), 0.0_f32);
    }

    // ---- AC3/AC5: default trait redirect returns None ----

    #[test]
    fn default_trait_redirect_is_none_for_leaf_entity() {
        // A context that overrides only `trigger` inherits the default
        // `redirect` returning None for every target — never a panic.
        struct LeafOnly;
        impl EvalContext for LeafOnly {
            fn trigger(&self, _name: &str, _args: &[Value]) -> Value {
                Value::Int(1)
            }
        }
        let ctx = LeafOnly;
        assert!(ctx.redirect(Redirect::Parent).is_none());
        assert!(ctx.redirect(Redirect::Root).is_none());
        assert!(ctx.redirect(Redirect::Helper(5)).is_none());
        assert!(ctx.redirect(Redirect::Target(Some(2))).is_none());
        assert!(ctx.redirect(Redirect::PlayerId(99)).is_none());
        // But its trigger still answers.
        assert_eq!(ctx.trigger("anything", &[]), Value::Int(1));
    }

    // ---- AC2/AC3: redirection resolution — all standard targets ----

    #[test]
    fn redirect_all_standard_targets_resolve() {
        // Build a context wired to every Redirect variant and confirm each one
        // resolves to a distinct child whose own trigger reads back. This pins
        // the full standard-target set required by AC2.
        let targets = [
            (Redirect::Parent, 1),
            (Redirect::Root, 2),
            (Redirect::Helper(1234), 3),
            (Redirect::Target(None), 4),
            (Redirect::Target(Some(0)), 5),
            (Redirect::Enemy, 6),
            (Redirect::EnemyNear(0), 7),
            (Redirect::Partner, 8),
            (Redirect::PlayerId(42), 9),
        ];
        let mut ctx = MockContext::new();
        for (t, n) in targets {
            ctx = ctx.with_redirect(t, MockContext::new().with_trigger("Id", &[], Value::Int(n)));
        }
        for (t, n) in targets {
            let child = ctx.redirect(t).expect("target wired above must resolve");
            assert_eq!(child.trigger("Id", &[]), Value::Int(n));
        }
    }

    #[test]
    fn redirect_child_default_redirect_is_none() {
        // A redirected child built without further relations cannot redirect on
        // — `parent,helper(1)` where the parent has no such helper → None.
        let parent = MockContext::new().with_trigger("Life", &[], Value::Int(900));
        let ctx = MockContext::new().with_redirect(Redirect::Parent, parent);
        let p = ctx.redirect(Redirect::Parent).expect("parent exists");
        assert_eq!(p.trigger("Life", &[]), Value::Int(900));
        assert!(p.redirect(Redirect::Helper(1)).is_none());
    }

    #[test]
    fn redirect_then_trigger_default_through_object_chain() {
        // enemy,SomeUnknownTrigger → enemy exists but the trigger does not, so
        // the read defaults to 0 (the evaluator's "redirect ok, value bottom"
        // path). Exercised entirely through &dyn EvalContext.
        let enemy = MockContext::new().with_trigger("Life", &[], Value::Int(1));
        let ctx = MockContext::new().with_redirect(Redirect::Enemy, enemy);
        let dynamic: &dyn EvalContext = &ctx;
        let e = dynamic.redirect(Redirect::Enemy).expect("enemy exists");
        assert_eq!(e.trigger("NoSuch", &[]), Value::DEFAULT);
        assert_eq!(e.trigger("Life", &[]), Value::Int(1));
    }

    #[test]
    fn deep_redirect_chain_resolves_and_dead_ends_to_none() {
        // root,parent,enemy,Life across four hops, then one hop too far → None.
        let leaf = MockContext::new().with_trigger("Life", &[], Value::Int(123));
        let enemy = MockContext::new().with_redirect(Redirect::Enemy, leaf);
        let parent = MockContext::new().with_redirect(Redirect::Parent, enemy);
        let ctx = MockContext::new().with_redirect(Redirect::Root, parent);

        let life = ctx
            .redirect(Redirect::Root)
            .and_then(|r| r.redirect(Redirect::Parent))
            .and_then(|p| p.redirect(Redirect::Enemy))
            .map(|e| e.trigger("Life", &[]));
        assert_eq!(life, Some(Value::Int(123)));

        // One hop past the leaf is a dead end.
        let dead = ctx
            .redirect(Redirect::Root)
            .and_then(|r| r.redirect(Redirect::Parent))
            .and_then(|p| p.redirect(Redirect::Enemy))
            .and_then(|e| e.redirect(Redirect::Root));
        assert!(dead.is_none());
    }

    // =====================================================================
    // AC6 (real fixture): walk trigger references out of the real KFM CNS and
    // confirm every name resolves *safely* (default 0, never a panic) through
    // an EvalContext. There is no evaluator yet (task 4.4), so this validates
    // the model + trait contract — name lookup never crashes on real content —
    // rather than computed values. Gated on test-assets/ so the default
    // `cargo test` still passes when the fixtures are absent.
    // =====================================================================

    #[test]
    fn real_kfm_trigger_names_resolve_safely_through_context() {
        use std::path::Path;

        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = [
            manifest.join("../../test-assets/kfm/kfm.cns"),
            manifest.join("../../test-assets/kfm/common1.cns"),
        ];

        // An empty context: nothing is registered, so EVERY real trigger name
        // must come back as the safe default 0 — the "unknown trigger → 0,
        // never crash" contract, exercised against production content.
        let ctx = MockContext::new();

        let mut names_seen = 0usize;
        let mut any_file = false;

        for path in &files {
            if !path.exists() {
                eprintln!("skipping (absent): {path:?}");
                continue;
            }
            any_file = true;
            let text = std::fs::read_to_string(path).expect("read cns fixture");
            for line in text.lines() {
                // Pull out leading identifier-ish words from each line and probe
                // them as bare trigger names. We are not parsing — just
                // harvesting plausible trigger tokens to stress the lookup path.
                for word in line.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
                    let w = word.trim();
                    // Heuristic: a trigger name starts with a letter. This skips
                    // pure numbers and empty fragments.
                    if w.is_empty() || !w.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                        continue;
                    }
                    names_seen += 1;
                    // Unknown name → safe default, no panic. Probe both bare and
                    // single-int-arg forms (e.g. var(0)-style).
                    assert_eq!(ctx.trigger(w, &[]), Value::DEFAULT);
                    assert_eq!(ctx.trigger(w, &[Value::Int(0)]), Value::DEFAULT);
                }
            }
        }

        if !any_file {
            eprintln!("skipping real_kfm_trigger_names_resolve_safely_through_context: no fixtures present");
            return;
        }

        assert!(
            names_seen > 0,
            "fixtures present but no trigger-like names found"
        );
        eprintln!(
            "real-fixture: probed {names_seen} trigger-like names; all resolved to safe default 0"
        );
    }

    // ---- T036: the in-expression assignment (`:=`) hook ----

    #[test]
    fn assign_bank_display() {
        assert_eq!(AssignBank::Var.to_string(), "var");
        assert_eq!(AssignBank::FVar.to_string(), "fvar");
        assert_eq!(AssignBank::SysVar.to_string(), "sysvar");
        assert_eq!(AssignBank::SysFVar.to_string(), "sysfvar");
    }

    #[test]
    fn default_assign_hook_is_noop_but_coerces_return() {
        // A context that does not model variables inherits the default `assign`,
        // which does NOT persist but still returns the value coerced to the bank's
        // element type (so a `:=` expression evaluates sensibly).
        struct ReadOnly;
        impl EvalContext for ReadOnly {
            fn trigger(&self, _name: &str, _args: &[Value]) -> Value {
                Value::DEFAULT
            }
        }
        let ctx = ReadOnly;
        // Integer banks narrow the return value.
        assert_eq!(
            ctx.assign(AssignBank::Var, 0, Value::Float(3.9)),
            Value::Int(3)
        );
        assert_eq!(
            ctx.assign(AssignBank::SysVar, 0, Value::Int(7)),
            Value::Int(7)
        );
        // Float banks widen the return value.
        assert_eq!(
            ctx.assign(AssignBank::FVar, 0, Value::Int(2)),
            Value::Float(2.0)
        );
        assert_eq!(
            ctx.assign(AssignBank::SysFVar, 0, Value::Float(1.5)),
            Value::Float(1.5)
        );
        // The write did not persist (read still default), and it did not panic.
        assert_eq!(ctx.var(0), Value::DEFAULT);
    }

    #[test]
    fn mock_assign_persists_and_is_read_back_per_bank() {
        // The interior-mutable MockContext actually stores the write, so a later
        // read returns the assigned value — covering var/fvar/sysvar/sysfvar.
        let ctx = MockContext::new();
        assert_eq!(
            ctx.assign(AssignBank::Var, 5, Value::Int(8000)),
            Value::Int(8000)
        );
        assert_eq!(ctx.var(5), Value::Int(8000));

        assert_eq!(
            ctx.assign(AssignBank::FVar, 2, Value::Float(1.25)),
            Value::Float(1.25)
        );
        assert_eq!(ctx.fvar(2), Value::Float(1.25));

        assert_eq!(
            ctx.assign(AssignBank::SysVar, 1, Value::Int(-3)),
            Value::Int(-3)
        );
        assert_eq!(ctx.sysvar(1), Value::Int(-3));

        // sysfvar has no typed read method on the trait; assign coerces & stores.
        assert_eq!(
            ctx.assign(AssignBank::SysFVar, 0, Value::Int(4)),
            Value::Float(4.0)
        );
    }
}
