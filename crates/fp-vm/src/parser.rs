//! # Expression parser
//!
//! Turns the flat [`Token`] stream produced by [`crate::lexer::tokenize`] into an
//! [`Expr`] abstract syntax tree with the full MUGEN operator precedence. This is
//! stage 2 of the `fp-vm` pipeline; the evaluator (task 4.4) walks the resulting
//! AST against an evaluation context.
//!
//! ## Precedence
//!
//! MUGEN's operator precedence, from **lowest** binding to **highest**
//! ([trigger.html](https://www.elecbyte.com/mugendocs/trigger.html)):
//!
//! | Level | Operators                       | Associativity |
//! |-------|---------------------------------|---------------|
//! | 1     | `||`                            | left          |
//! | 2     | `&&`                            | left          |
//! | 3     | `|` `^` `&` (bitwise)           | left          |
//! | 4     | `=` `==` `!=` `<` `<=` `>` `>=` | left          |
//! | 5     | `+` `-`                         | left          |
//! | 6     | `*` `/` `%`                     | left          |
//! | 7     | `!` `-` `~` (unary prefix)      | right (prefix)|
//! | 8     | `**` (exponent)                 | **right**     |
//! | 9     | parens / calls / atoms          | —             |
//!
//! The parser is a [precedence-climbing](https://en.wikipedia.org/wiki/Operator-precedence_parser)
//! (Pratt-style) recursive descent parser.
//!
//! ## Redirection
//!
//! A trigger expression may be prefixed with a *redirection* —
//! `enemy, P2BodyDistX`, `root, var(0)`, `helper(1234), stateno` — which
//! evaluates the trailing sub-expression against another entity (see
//! [`Expr::Redirected`]). A redirection binds **looser** than every operator in
//! the table above, so `enemy, life + 1` redirects the whole `life + 1`. It is
//! recognized at the start of any full expression — the top level and each call
//! argument / range bound — so redirections nest (`enemy, helper(1), x`) yet a
//! comma that merely separates call arguments (`cond(a, b, c)`) is never
//! mistaken for one.
//!
//! ## Tolerance
//!
//! In keeping with the engine-wide "never crash on bad content" rule (see
//! `CLAUDE.md`), the parser **never panics**. A [`TokenKind::Unknown`] token, an
//! unexpected token, a missing operand, a malformed range, or a malformed
//! redirection all produce a recoverable [`ParseError`]. The CNS parser (task
//! 4.x) maps that error to the constant `0` with a [`tracing::warn!`], mirroring
//! how the lexer substitutes
//! safe defaults.
//!
//! # Example
//!
//! ```
//! use fp_vm::parser::{parse_str, Expr, BinaryOp};
//!
//! // `1 + 2 * 3` parses as `1 + (2 * 3)` because `*` binds tighter than `+`.
//! let ast = parse_str("1 + 2 * 3").unwrap();
//! assert_eq!(
//!     ast,
//!     Expr::Binary {
//!         op: BinaryOp::Add,
//!         lhs: Box::new(Expr::Int(1)),
//!         rhs: Box::new(Expr::Binary {
//!             op: BinaryOp::Mul,
//!             lhs: Box::new(Expr::Int(2)),
//!             rhs: Box::new(Expr::Int(3)),
//!         }),
//!     }
//! );
//! ```

use std::fmt;

use crate::eval::Redirect;
use crate::lexer::{tokenize, Token, TokenKind};

/// A binary operator in the MUGEN expression grammar.
///
/// Each variant maps to exactly one source operator. Equality is represented by
/// a single [`BinaryOp::Eq`] regardless of whether the source used `=` or its
/// `==` alias, so the evaluator does not have to distinguish them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `||` — logical OR.
    Or,
    /// `&&` — logical AND.
    And,
    /// `|` — bitwise OR.
    BitOr,
    /// `^` — bitwise XOR.
    BitXor,
    /// `&` — bitwise AND.
    BitAnd,
    /// `=` (or its `==` alias) — equality.
    Eq,
    /// `!=` — inequality.
    Ne,
    /// `<` — less than.
    Lt,
    /// `<=` — less than or equal.
    Le,
    /// `>` — greater than.
    Gt,
    /// `>=` — greater than or equal.
    Ge,
    /// `+` — addition.
    Add,
    /// `-` — subtraction.
    Sub,
    /// `*` — multiplication.
    Mul,
    /// `/` — division.
    Div,
    /// `%` — modulo.
    Mod,
    /// `**` — exponentiation (right-associative).
    Pow,
}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BinaryOp::Or => "||",
            BinaryOp::And => "&&",
            BinaryOp::BitOr => "|",
            BinaryOp::BitXor => "^",
            BinaryOp::BitAnd => "&",
            BinaryOp::Eq => "=",
            BinaryOp::Ne => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Pow => "**",
        };
        f.write_str(s)
    }
}

/// A unary prefix operator in the MUGEN expression grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `!` — logical NOT.
    Not,
    /// `-` — arithmetic negation.
    Neg,
    /// `~` — bitwise NOT.
    BitNot,
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            UnaryOp::Not => "!",
            UnaryOp::Neg => "-",
            UnaryOp::BitNot => "~",
        };
        f.write_str(s)
    }
}

/// Whether a range bound is inclusive (`[`/`]`) or exclusive (`(`/`)`).
///
/// MUGEN ranges can mix bound kinds, e.g. `(0,10]` is "greater than 0 and at
/// most 10".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// Inclusive bound, written with a square bracket (`[` or `]`).
    Inclusive,
    /// Exclusive bound, written with a parenthesis (`(` or `)`).
    Exclusive,
}

/// A parsed MUGEN expression.
///
/// This is the output of [`parse`] / [`parse_str`] and the input to the
/// evaluator (task 4.4). Sub-expressions are boxed so the enum stays a fixed
/// size.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// An integer literal, e.g. `42`.
    Int(i64),
    /// A floating-point literal, e.g. `3.14`.
    Float(f64),
    /// A double-quoted string literal (quotes already removed), e.g. `"x"`.
    Str(String),
    /// A bare identifier: a trigger name or keyword, e.g. `Time` or `AnimElem`.
    ///
    /// Parameterized triggers such as `var(0)` parse as [`Expr::Call`] instead.
    Ident(String),
    /// A unary prefix operation, e.g. `-x`, `!flag`, `~bits`.
    Unary {
        /// The operator applied.
        op: UnaryOp,
        /// The operand.
        operand: Box<Expr>,
    },
    /// A binary operation, e.g. `a + b` or `Time >= 30`.
    Binary {
        /// The operator applied.
        op: BinaryOp,
        /// The left-hand operand.
        lhs: Box<Expr>,
        /// The right-hand operand.
        rhs: Box<Expr>,
    },
    /// A function call or parameterized trigger, e.g. `cond(c, t, f)`,
    /// `abs(x)`, or `var(0)`.
    ///
    /// The callee name is stored as written (case preserved); case-insensitive
    /// matching is left to the evaluator.
    ///
    /// ## Axis-suffixed component triggers (task 4.10, gap 1)
    ///
    /// MUGEN writes several vector triggers with a trailing, space-separated
    /// *axis word* — `Vel Y`, `Pos X`, `P2Dist X`, `P2BodyDist X`,
    /// `ScreenPos Y`, … — selecting one component of a 2-D (occasionally 3-D)
    /// quantity. There is no operator between the two words; they are two
    /// adjacent identifiers in the token stream. The parser lowers this form to
    /// a single-argument `Call` whose **argument is the axis as a string
    /// literal**: `Vel Y` becomes `Call { name: "Vel", args: [Str("Y")] }`,
    /// `P2BodyDist X` becomes `Call { name: "P2BodyDist", args: [Str("X")] }`.
    /// The axis word is normalized to an upper-case single letter (`"X"` /
    /// `"Y"` / `"Z"`). This reuses the ordinary `Call` →
    /// [`EvalContext::trigger`](crate::eval::EvalContext::trigger) path: the
    /// evaluator passes the axis through as the (string) argument, so a context
    /// answers `Vel Y` by reading `trigger("Vel", &[Value from "Y"])`. A bare,
    /// non-suffixed `Vel` (no following axis word) still parses as a plain
    /// [`Expr::Ident`].
    Call {
        /// The function / trigger name.
        name: String,
        /// The argument expressions, in source order.
        args: Vec<Expr>,
    },
    /// A range literal usable as the right-hand side of `=` / `!=`, e.g.
    /// `[6,9]`, `(0,1)`, `[0,10)`.
    Range {
        /// Whether the lower bound is inclusive or exclusive.
        lower_bound: Bound,
        /// The lower bound expression.
        lower: Box<Expr>,
        /// The upper bound expression.
        upper: Box<Expr>,
        /// Whether the upper bound is inclusive or exclusive.
        upper_bound: Bound,
    },
    /// A redirected expression: a trigger evaluated against *another* entity,
    /// written `redirect, expr` — e.g. `enemy, P2BodyDistX`, `root, var(0)`,
    /// `helper(1234), stateno`, `enemynear(1), life`, `parent, animtime`.
    ///
    /// The leading [`Redirect`] keyword (optionally with an `(id)`) retargets
    /// the entire trailing sub-expression at the related entity; the evaluator
    /// resolves the [`Redirect`] via
    /// [`EvalContext::redirect`](crate::eval::EvalContext::redirect) and
    /// evaluates `expr` against the resulting context. Redirection binds looser
    /// than every operator, so `enemy, life + 1` redirects the whole `life + 1`.
    /// Redirections nest, so `enemy, helper(1), x` parses as
    /// `Redirected { Enemy, Redirected { Helper(1), x } }`.
    Redirected {
        /// Which entity the sub-expression is evaluated against.
        target: Redirect,
        /// The sub-expression to evaluate in the redirected context.
        expr: Box<Expr>,
    },
    /// The two-parameter *element-time comparison* form of the `AnimElem` /
    /// `AnimElemTime` trigger family (task 4.10, gap 2), written
    /// `AnimElem = N, op M` — for example `AnimElem = 2, >= 0` or the shorthand
    /// `AnimElem = 2, 1` (an omitted operator means `=`).
    ///
    /// MUGEN reads this as **"the animation has reached element `N`, *and* the
    /// time relative to that element satisfies `op M`"**. It is therefore the
    /// conjunction of two checks: the element has been reached
    /// (`AnimElemTime(N) >= 0`) **and** the secondary comparison
    /// (`AnimElemTime(N) op M`). The evaluator computes exactly that against
    /// [`EvalContext::trigger`](crate::eval::EvalContext::trigger) with the
    /// trigger name `"AnimElemTime"` and the (evaluated) element number as the
    /// argument; see [the evaluator](crate::evaluator) for the precise lowering.
    ///
    /// The bare `AnimElem = N` form (no comma tail) keeps parsing as an ordinary
    /// [`Expr::Binary`] equality — this variant is produced *only* when the
    /// comma tail is present.
    AnimElemTail {
        /// The trigger name as written (case preserved): `AnimElem`,
        /// `AnimElemTime`, `TimeMod`, etc.
        name: String,
        /// The element number `N` (the right-hand side of the leading `= N`).
        element: Box<Expr>,
        /// The comparison operator applied to the element time. An omitted
        /// operator in the source (`AnimElem = 2, 1`) defaults to
        /// [`BinaryOp::Eq`].
        op: BinaryOp,
        /// The secondary operand `M` the element time is compared against.
        operand: Box<Expr>,
    },
}

/// A recoverable parse failure.
///
/// The parser never panics; every malformed input surfaces as one of these
/// variants. The CNS parser maps a `ParseError` to the constant `0` with a
/// warning, matching the engine's "bad expression -> 0" default. The error
/// carries the offending token's character column where one is available, for
/// diagnostics.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ParseError {
    /// The lexer emitted a [`TokenKind::Unknown`] token (an unrecognized source
    /// character), which can never be part of a valid expression.
    #[error("unknown character {ch:?} at column {column}")]
    UnknownToken {
        /// The offending character.
        ch: char,
        /// 0-based character column of the token.
        column: usize,
    },

    /// A token appeared where it is not allowed (e.g. a stray operator, a `)`
    /// with no matching `(`, or a literal where an operator was expected).
    #[error("unexpected token `{token}` at column {column}")]
    UnexpectedToken {
        /// Textual form of the offending token.
        token: String,
        /// 0-based character column of the token.
        column: usize,
    },

    /// An operand or sub-expression was required but the input ended first
    /// (e.g. a trailing `+`, or an unclosed `(`).
    #[error("unexpected end of input: {expected}")]
    UnexpectedEof {
        /// What the parser was expecting when input ran out.
        expected: String,
    },

    /// A delimiter was expected but a different token (or end of input) was
    /// found, e.g. a missing `)` after a call's arguments.
    #[error("expected `{expected}` but found `{found}` at column {column}")]
    ExpectedDelimiter {
        /// The delimiter that was required.
        expected: String,
        /// What was found instead.
        found: String,
        /// 0-based character column where the delimiter was expected.
        column: usize,
    },

    /// A redirection prefix was syntactically present (a known redirection
    /// keyword followed by a top-level `,`) but malformed — e.g. the comma had
    /// no following sub-expression (`enemy,`), or an `(id)` was opened but not a
    /// valid integer / closed (`helper(,`). Mapped, like every other variant, to
    /// the bad-expression → `0` contract rather than a panic.
    #[error("malformed redirection at column {column}: {reason}")]
    MalformedRedirect {
        /// What was wrong with the redirection.
        reason: String,
        /// 0-based character column of the offending redirection keyword.
        column: usize,
    },

    /// The input was empty (or only whitespace/comments), so there is no
    /// expression to parse.
    #[error("empty expression")]
    Empty,
}

/// Parses a token slice into an [`Expr`] AST.
///
/// This is the primary entry point when you already have tokens from
/// [`crate::lexer::tokenize`]. It is **panic-free**: any malformed input yields
/// a [`ParseError`] (which the caller maps to a safe default), never a panic.
///
/// All tokens must be consumed; trailing tokens after a complete expression are
/// reported as [`ParseError::UnexpectedToken`].
///
/// # Example
///
/// ```
/// use fp_vm::lexer::tokenize;
/// use fp_vm::parser::{parse, Expr};
///
/// let tokens = tokenize("Time");
/// assert_eq!(parse(&tokens).unwrap(), Expr::Ident("Time".into()));
/// ```
///
/// # Errors
///
/// Returns a [`ParseError`] if the token stream is empty, contains an unknown
/// token, is syntactically malformed, or has leftover tokens.
pub fn parse(tokens: &[Token]) -> Result<Expr, ParseError> {
    let mut p = Parser::new(tokens);
    if p.is_at_end() {
        return Err(ParseError::Empty);
    }
    let mut expr = p.parse_expr(0)?;
    // The `AnimElem = N, op M` two-parameter comparison form (task 4.10, gap 2)
    // leaves a top-level `,` after the leading `= N` equality. Try to fold it
    // into an `AnimElemTail`; a non-matching shape leaves `expr` and the comma
    // untouched, so the trailing-token check below still rejects a stray comma.
    if p.peek().map(|t| &t.kind) == Some(&TokenKind::Comma) {
        if let Some(folded) = p.try_fold_animelem_tail(&expr)? {
            expr = folded;
        }
    }
    // Reject trailing tokens (e.g. `1 2` or `(a) b`).
    if let Some(tok) = p.peek() {
        return Err(ParseError::UnexpectedToken {
            token: tok.kind.to_string(),
            column: tok.column,
        });
    }
    Ok(expr)
}

/// Tokenizes `src` and parses it into an [`Expr`] AST in one step.
///
/// A convenience wrapper around [`crate::lexer::tokenize`] followed by [`parse`].
/// Like [`parse`], it never panics.
///
/// # Example
///
/// ```
/// use fp_vm::parser::{parse_str, Expr, BinaryOp};
///
/// // `2 ** 3 ** 2` is right-associative: `2 ** (3 ** 2)` = 512.
/// let ast = parse_str("2 ** 3 ** 2").unwrap();
/// assert_eq!(
///     ast,
///     Expr::Binary {
///         op: BinaryOp::Pow,
///         lhs: Box::new(Expr::Int(2)),
///         rhs: Box::new(Expr::Binary {
///             op: BinaryOp::Pow,
///             lhs: Box::new(Expr::Int(3)),
///             rhs: Box::new(Expr::Int(2)),
///         }),
///     }
/// );
/// ```
///
/// # Errors
///
/// Returns a [`ParseError`] for the same reasons as [`parse`].
pub fn parse_str(src: &str) -> Result<Expr, ParseError> {
    let tokens = tokenize(src);
    parse(&tokens)
}

/// Binding power (precedence) of a binary operator.
///
/// Higher binds tighter. The `**` exponent operator is handled separately
/// because it is right-associative and binds tighter than unary prefixes.
fn binary_binding_power(kind: &TokenKind) -> Option<(u8, BinaryOp)> {
    let pair = match kind {
        TokenKind::OrOr => (1, BinaryOp::Or),
        TokenKind::AndAnd => (2, BinaryOp::And),
        // Bitwise share one level (left-to-right). MUGEN groups `| ^ &` together.
        TokenKind::Pipe => (3, BinaryOp::BitOr),
        TokenKind::Caret => (3, BinaryOp::BitXor),
        TokenKind::Amp => (3, BinaryOp::BitAnd),
        // Relational.
        TokenKind::Eq | TokenKind::EqEq => (4, BinaryOp::Eq),
        TokenKind::NotEq => (4, BinaryOp::Ne),
        TokenKind::Lt => (4, BinaryOp::Lt),
        TokenKind::Le => (4, BinaryOp::Le),
        TokenKind::Gt => (4, BinaryOp::Gt),
        TokenKind::Ge => (4, BinaryOp::Ge),
        // Additive.
        TokenKind::Plus => (5, BinaryOp::Add),
        TokenKind::Minus => (5, BinaryOp::Sub),
        // Multiplicative.
        TokenKind::Star => (6, BinaryOp::Mul),
        TokenKind::Slash => (6, BinaryOp::Div),
        TokenKind::Percent => (6, BinaryOp::Mod),
        _ => return None,
    };
    Some(pair)
}

/// Maps a relational-operator token to its [`BinaryOp`], or [`None`] if the
/// token is not a comparison operator. Used to read the optional operator in the
/// `AnimElem = N, op M` comma-tail form (task 4.10, gap 2).
fn relational_op(kind: &TokenKind) -> Option<BinaryOp> {
    Some(match kind {
        TokenKind::Eq | TokenKind::EqEq => BinaryOp::Eq,
        TokenKind::NotEq => BinaryOp::Ne,
        TokenKind::Lt => BinaryOp::Lt,
        TokenKind::Le => BinaryOp::Le,
        TokenKind::Gt => BinaryOp::Gt,
        TokenKind::Ge => BinaryOp::Ge,
        _ => return None,
    })
}

/// Returns whether `name` (matched case-insensitively) is a member of the
/// `AnimElem` trigger family that accepts the two-parameter
/// `= N, op M` comparison tail (task 4.10, gap 2).
fn is_animelem_family(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "animelem" | "animelemtime" | "animelemno" | "timemod"
    )
}

// Note on the two highest precedence levels: unary prefixes conceptually sit at
// level 7 and the exponent `**` at level 8 (above every infix operator listed in
// `binary_binding_power`). Both are handled structurally — unary in
// [`Parser::parse_prefix`], `**` in [`Parser::parse_power`] — rather than through
// the main left-fold loop, so they need no entry in the binding-power table.

/// A recognized redirection keyword, before its (optional) `(id)` is validated.
///
/// This is an internal classification step: [`RedirectKeyword::from_name`] maps
/// a (case-insensitive) identifier to one of these, and
/// [`RedirectKeyword::into_redirect`] then combines it with the parsed `(id)` to
/// build the public [`Redirect`], rejecting id/keyword mismatches as a
/// [`ParseError::MalformedRedirect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedirectKeyword {
    /// `parent` — takes no id.
    Parent,
    /// `root` — takes no id.
    Root,
    /// `partner` — takes no id.
    Partner,
    /// `enemy` / `enemy(n)` — optional id (lowered per CB8, see [`Redirect`]).
    Enemy,
    /// `enemynear` / `enemynear(n)` — optional id (defaults to nearest, `0`).
    EnemyNear,
    /// `target` / `target(id)` — optional id.
    Target,
    /// `helper` / `helper(id)` — optional id (defaults to "any" helper).
    Helper,
    /// `playerid(id)` — id required.
    PlayerId,
}

impl RedirectKeyword {
    /// Maps a (case-insensitive) identifier to a redirection keyword, or
    /// [`None`] if the name is not a redirection keyword.
    fn from_name(name: &str) -> Option<Self> {
        // Compare lowercased: MUGEN keywords are case-insensitive.
        let kw = match name.to_ascii_lowercase().as_str() {
            "parent" => RedirectKeyword::Parent,
            "root" => RedirectKeyword::Root,
            "partner" => RedirectKeyword::Partner,
            "enemy" => RedirectKeyword::Enemy,
            "enemynear" => RedirectKeyword::EnemyNear,
            "target" => RedirectKeyword::Target,
            "helper" => RedirectKeyword::Helper,
            "playerid" => RedirectKeyword::PlayerId,
            _ => return None,
        };
        Some(kw)
    }

    /// Combines the keyword with its parsed `(id)` (if any) into a [`Redirect`].
    ///
    /// CB8: `enemy(n)` with `n > 0` lowers to [`Redirect::EnemyNear(n)`], while
    /// bare `enemy` and `enemy(0)` become [`Redirect::Enemy`] — the index is
    /// preserved on `EnemyNear`, never dropped.
    ///
    /// `column` is the source column of the keyword, used in the error.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::MalformedRedirect`] when an id is supplied to a
    /// keyword that takes none (`parent(1)`), or omitted from one that requires
    /// it (`playerid` with no id).
    fn into_redirect(self, id: Option<i32>, column: usize) -> Result<Redirect, ParseError> {
        let no_id = |reason: &str| ParseError::MalformedRedirect {
            reason: reason.to_string(),
            column,
        };
        match self {
            // Keywords that never take an id.
            RedirectKeyword::Parent => match id {
                None => Ok(Redirect::Parent),
                Some(_) => Err(no_id("`parent` does not take an id")),
            },
            RedirectKeyword::Root => match id {
                None => Ok(Redirect::Root),
                Some(_) => Err(no_id("`root` does not take an id")),
            },
            RedirectKeyword::Partner => match id {
                None => Ok(Redirect::Partner),
                Some(_) => Err(no_id("`partner` does not take an id")),
            },
            // `enemy(n)`: lower per CB8 (see `Redirect` docs).
            RedirectKeyword::Enemy => match id {
                None | Some(0) => Ok(Redirect::Enemy),
                Some(n) => Ok(Redirect::EnemyNear(n)),
            },
            // `enemynear(n)`: nearest (`0`) when no id is given.
            RedirectKeyword::EnemyNear => Ok(Redirect::EnemyNear(id.unwrap_or(0))),
            // `target` / `target(id)`: id is optional.
            RedirectKeyword::Target => Ok(Redirect::Target(id)),
            // `helper` / `helper(id)`: a bare `helper` selects "any" helper,
            // modeled as id 0 (the engine's default), so the id is never lost.
            RedirectKeyword::Helper => Ok(Redirect::Helper(id.unwrap_or(0))),
            // `playerid(id)`: the id is mandatory.
            RedirectKeyword::PlayerId => match id {
                Some(n) => Ok(Redirect::PlayerId(n)),
                None => Err(no_id("`playerid` requires an id")),
            },
        }
    }
}

/// Recursive-descent / precedence-climbing parser over a borrowed token slice.
struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Returns the current token without consuming it.
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.pos)
    }

    /// Consumes and returns the current token.
    fn advance(&mut self) -> Option<&'a Token> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    /// Whether the parser has consumed every token.
    fn is_at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// Parses an expression whose operators all bind at least as tightly as
    /// `min_bp` (precedence climbing).
    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr, ParseError> {
        // Redirection (`redirect, expr`) binds looser than every operator, so it
        // is only recognized at the start of a full expression (`min_bp == 0`):
        // the top level and each call argument / range bound, which all recurse
        // through `parse_expr(0)`. This lets a redirect target itself redirect
        // (`enemy, helper(1), x`) and keeps a comma inside a higher-precedence
        // context (e.g. `cond(a, b, c)`'s arg separators) from being mistaken
        // for a redirect.
        if min_bp == 0 {
            if let Some(redirected) = self.try_parse_redirect()? {
                return Ok(redirected);
            }
        }

        let mut lhs = self.parse_prefix()?;

        // Left-fold binary operators with binding power >= min_bp.
        while let Some(tok) = self.peek() {
            let Some((bp, op)) = binary_binding_power(&tok.kind) else {
                break;
            };
            if bp < min_bp {
                break;
            }
            self.advance(); // consume the operator
            // Left-associative: parse the RHS with a higher threshold so equal
            // precedence binds to the left.
            let rhs = self.parse_expr(bp + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }

        Ok(lhs)
    }

    /// Attempts to parse a redirection prefix (`redirect, expr`) at the current
    /// position.
    ///
    /// Returns `Ok(Some(Expr::Redirected { .. }))` when the upcoming tokens form
    /// `keyword [optional (id)] , <expr>` (the tokens are consumed); `Ok(None)`
    /// when the current token does **not** begin a redirection (nothing is
    /// consumed, so the caller parses it as an ordinary expression); and `Err`
    /// for a redirection that committed (keyword + comma seen) but is malformed
    /// (e.g. `enemy,` with no following sub-expression).
    ///
    /// The lookahead only *commits* once it has confirmed a redirection keyword
    /// followed (after an optional `(id)`) by a top-level `,`. A redirection
    /// keyword that is **not** followed by a comma — e.g. `helper(1)` used as a
    /// bare value, or an identifier like `parent` that happens to share a name —
    /// is left untouched for the normal atom path, so non-redirection
    /// expressions are unchanged.
    fn try_parse_redirect(&mut self) -> Result<Option<Expr>, ParseError> {
        // The current token must be an identifier naming a redirection keyword.
        let Some(tok) = self.peek() else {
            return Ok(None);
        };
        let TokenKind::Ident(name) = &tok.kind else {
            return Ok(None);
        };
        let Some(kw) = RedirectKeyword::from_name(name) else {
            return Ok(None);
        };
        let column = tok.column;

        // Probe the id and the following comma WITHOUT consuming, so a keyword
        // that is not actually a redirect (no trailing comma) falls through to
        // the ordinary atom parser untouched.
        let mut probe = self.pos + 1; // position just after the keyword

        // Optional `(id)` — an integer (optionally negated) in parentheses.
        let id = if self.kind_at(probe) == Some(&TokenKind::LParen) {
            let (parsed_id, next) = match self.scan_redirect_id(probe) {
                Some(pair) => pair,
                // `(` opened but not a valid `(int)` — not a redirect id; leave
                // for the normal path (it may be e.g. a call we do not handle).
                None => return Ok(None),
            };
            probe = next;
            Some(parsed_id)
        } else {
            None
        };

        // The next token must be a top-level comma for this to be a redirect.
        if self.kind_at(probe) != Some(&TokenKind::Comma) {
            return Ok(None);
        }

        // Commit: build the redirect target (this is the only place an `(id)` is
        // validated against the keyword), consume through the comma, then parse
        // the redirected sub-expression.
        let target = kw.into_redirect(id, column)?;
        self.pos = probe + 1; // skip keyword (+id) and the comma

        if self.is_at_end() {
            return Err(ParseError::MalformedRedirect {
                reason: "redirection `,` has no following expression".to_string(),
                column,
            });
        }
        let expr = self.parse_expr(0)?;
        Ok(Some(Expr::Redirected {
            target,
            expr: Box::new(expr),
        }))
    }

    /// Returns the [`TokenKind`] at absolute position `idx`, if any.
    fn kind_at(&self, idx: usize) -> Option<&'a TokenKind> {
        self.tokens.get(idx).map(|t| &t.kind)
    }

    /// If the current token is a bare axis word (`X` / `Y` / `Z`,
    /// case-insensitive), returns it normalized to an upper-case single-letter
    /// string; otherwise [`None`]. Used to fold a space-separated component
    /// trigger (`Vel Y`, `Pos X`) into a one-argument call (task 4.10, gap 1).
    ///
    /// The token is **not** consumed — the caller decides whether to commit. The
    /// match is exact (`x`, `y`, `z`), so a trigger or identifier that merely
    /// starts with one of those letters (`yaccel`, `xvel`) is never mistaken for
    /// an axis word.
    fn peek_axis_word(&self) -> Option<String> {
        let TokenKind::Ident(word) = &self.peek()?.kind else {
            return None;
        };
        match word.as_str() {
            "x" | "X" => Some("X".to_string()),
            "y" | "Y" => Some("Y".to_string()),
            "z" | "Z" => Some("Z".to_string()),
            _ => None,
        }
    }

    /// Tries to fold an already-parsed `<AnimElem-family> = N` equality plus a
    /// trailing `, [op] M` into an [`Expr::AnimElemTail`] (task 4.10, gap 2).
    ///
    /// The current token must be the top-level `,`. The supplied `expr` must be
    /// an equality `Binary { op: Eq, lhs, rhs }` whose `lhs` names a member of
    /// the `AnimElem` trigger family (`AnimElem`, `AnimElemTime`, `TimeMod`,
    /// `AnimElemNo`); the `rhs` is the element number `N`. On a match this
    /// consumes the comma and the tail (`[op] M`, where an omitted operator
    /// defaults to `=`) and returns the folded node. On any non-match it returns
    /// `Ok(None)` **without consuming** the comma, so the caller's
    /// trailing-token check still rejects an unrelated stray comma (e.g.
    /// `foo, bar`).
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] only when the comma *was* consumed (the shape
    /// matched) but the tail is malformed — e.g. `AnimElem = 2,` with nothing
    /// after the comma. That keeps the never-panic / recoverable-error contract.
    fn try_fold_animelem_tail(&mut self, expr: &Expr) -> Result<Option<Expr>, ParseError> {
        // The leading clause must be `<family-trigger> = N`.
        let Expr::Binary {
            op: BinaryOp::Eq,
            lhs,
            rhs,
        } = expr
        else {
            return Ok(None);
        };
        let Expr::Ident(name) = lhs.as_ref() else {
            return Ok(None);
        };
        if !is_animelem_family(name) {
            return Ok(None);
        }

        // Commit: consume the top-level comma.
        let comma_col = self.peek().map_or(0, |t| t.column);
        self.advance();

        // Optional comparison operator; an omitted operator means `=`.
        let op = match self.peek().map(|t| &t.kind) {
            Some(kind) => relational_op(kind).unwrap_or(BinaryOp::Eq),
            None => {
                return Err(ParseError::UnexpectedEof {
                    expected: "a comparison operand after `,` in the AnimElem tail".to_string(),
                });
            }
        };
        // Consume the operator token if one was actually present.
        if self
            .peek()
            .map(|t| &t.kind)
            .and_then(relational_op)
            .is_some()
        {
            self.advance();
        }

        // The secondary operand `M` is a full expression.
        if self.is_at_end() {
            return Err(ParseError::UnexpectedEof {
                expected: "a comparison operand after the AnimElem tail operator".to_string(),
            });
        }
        // Guard against an immediate second comma (`AnimElem = 2,,`) producing a
        // confusing error: parse_expr will report the comma as unexpected.
        let _ = comma_col;
        let operand = self.parse_expr(0)?;

        Ok(Some(Expr::AnimElemTail {
            name: name.clone(),
            element: rhs.clone(),
            op,
            operand: Box::new(operand),
        }))
    }

    /// Scans an `(id)` redirect index starting at `open` (which must index the
    /// `(`), returning the integer value and the position just past the `)`.
    ///
    /// Accepts `(int)` and `(-int)` only; returns [`None`] for anything else
    /// (e.g. `(expr)`, `()`, or an unclosed paren), so the caller can decline to
    /// treat the keyword as a redirect.
    fn scan_redirect_id(&self, open: usize) -> Option<(i32, usize)> {
        debug_assert_eq!(self.kind_at(open), Some(&TokenKind::LParen));
        let mut idx = open + 1;
        let negate = if self.kind_at(idx) == Some(&TokenKind::Minus) {
            idx += 1;
            true
        } else {
            false
        };
        let Some(TokenKind::Int(n)) = self.kind_at(idx) else {
            return None;
        };
        idx += 1;
        if self.kind_at(idx) != Some(&TokenKind::RParen) {
            return None;
        }
        // Saturate the i64 lexer literal into i32 (CB4 narrowing); ids are small
        // in practice, so this only guards pathological input.
        let value = (*n).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        let value = if negate { value.wrapping_neg() } else { value };
        Some((value, idx + 1))
    }

    /// Parses a prefix expression: unary operators, then a power expression.
    ///
    /// Unary prefixes (`! - ~`) bind tighter than every binary operator below
    /// `**` and are right-recursive, so `--x` and `!!x` chain naturally.
    fn parse_prefix(&mut self) -> Result<Expr, ParseError> {
        if let Some(tok) = self.peek() {
            let unary = match tok.kind {
                TokenKind::Not => Some(UnaryOp::Not),
                TokenKind::Minus => Some(UnaryOp::Neg),
                TokenKind::Tilde => Some(UnaryOp::BitNot),
                _ => None,
            };
            if let Some(op) = unary {
                self.advance(); // consume the prefix operator
                let operand = self.parse_prefix()?;
                return Ok(Expr::Unary {
                    op,
                    operand: Box::new(operand),
                });
            }
        }
        self.parse_power()
    }

    /// Parses a power (`**`) expression. `**` is right-associative and binds
    /// tighter than the unary prefixes that called into here.
    fn parse_power(&mut self) -> Result<Expr, ParseError> {
        let base = self.parse_atom()?;
        if let Some(tok) = self.peek() {
            if tok.kind == TokenKind::StarStar {
                self.advance(); // consume `**`
                // Right-associative: the exponent itself may carry a prefix
                // (e.g. `2 ** -3`), so recurse through `parse_prefix`, which in
                // turn handles the next `**` at the same level.
                let exponent = self.parse_prefix()?;
                return Ok(Expr::Binary {
                    op: BinaryOp::Pow,
                    lhs: Box::new(base),
                    rhs: Box::new(exponent),
                });
            }
        }
        Ok(base)
    }

    /// Parses an atom: a literal, identifier, function call, parenthesized
    /// expression, or range literal.
    fn parse_atom(&mut self) -> Result<Expr, ParseError> {
        let tok = self.advance().ok_or_else(|| ParseError::UnexpectedEof {
            expected: "an expression".to_string(),
        })?;

        match &tok.kind {
            TokenKind::Int(n) => Ok(Expr::Int(*n)),
            TokenKind::Float(n) => Ok(Expr::Float(*n)),
            TokenKind::Str(s) => Ok(Expr::Str(s.clone())),
            TokenKind::Ident(name) => {
                // A `(` immediately after an identifier makes it a call /
                // parameterized trigger, e.g. `var(0)` or `cond(...)`.
                if self.peek().map(|t| &t.kind) == Some(&TokenKind::LParen) {
                    self.advance(); // consume `(`
                    let args = self.parse_call_args()?;
                    Ok(Expr::Call {
                        name: name.clone(),
                        args,
                    })
                } else if let Some(axis) = self.peek_axis_word() {
                    // Axis-suffixed component trigger (task 4.10, gap 1):
                    // `Vel Y`, `Pos X`, `P2BodyDist X`, `ScreenPos Y`, … — a
                    // trigger name followed by a bare axis word. Lower it to a
                    // call carrying the axis as a string argument, so it reuses
                    // the ordinary `Call` → `trigger` path; see `Expr::Call`'s
                    // axis-suffix note.
                    self.advance(); // consume the axis word
                    Ok(Expr::Call {
                        name: name.clone(),
                        args: vec![Expr::Str(axis)],
                    })
                } else {
                    Ok(Expr::Ident(name.clone()))
                }
            }
            // `(` begins either a grouped expression or an exclusive-lower range.
            TokenKind::LParen => self.parse_paren_or_range(Bound::Exclusive),
            // `[` always begins an inclusive-lower range.
            TokenKind::LBracket => self.parse_range(Bound::Inclusive),
            TokenKind::Unknown(ch) => Err(ParseError::UnknownToken {
                ch: *ch,
                column: tok.column,
            }),
            _ => Err(ParseError::UnexpectedToken {
                token: tok.kind.to_string(),
                column: tok.column,
            }),
        }
    }

    /// Parses the argument list of a call after the opening `(` has been
    /// consumed, up to and including the closing `)`. Handles zero args
    /// (`random`-style calls written `f()`).
    fn parse_call_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        let mut args = Vec::new();

        // Empty argument list: `f()`.
        if self.peek().map(|t| &t.kind) == Some(&TokenKind::RParen) {
            self.advance(); // consume `)`
            return Ok(args);
        }

        loop {
            args.push(self.parse_expr(0)?);
            match self.peek().map(|t| &t.kind) {
                Some(TokenKind::Comma) => {
                    self.advance(); // consume `,` and continue
                }
                Some(TokenKind::RParen) => {
                    self.advance(); // consume `)` and finish
                    break;
                }
                _ => return Err(self.delimiter_error(") or ,")),
            }
        }
        Ok(args)
    }

    /// Handles a token sequence that opened with `(`: it is either a grouped
    /// expression `( expr )` or an exclusive-lower range `( a , b )` / `( a , b ]`.
    ///
    /// The two are distinguished by whether a comma follows the first
    /// sub-expression.
    fn parse_paren_or_range(&mut self, lower_bound: Bound) -> Result<Expr, ParseError> {
        let first = self.parse_expr(0)?;
        match self.peek().map(|t| &t.kind) {
            Some(TokenKind::Comma) => {
                self.advance(); // consume `,`
                self.finish_range(lower_bound, first)
            }
            Some(TokenKind::RParen) => {
                self.advance(); // consume `)` — plain grouping
                Ok(first)
            }
            _ => Err(self.delimiter_error(") or ,")),
        }
    }

    /// Parses a range that opened with `[` (inclusive lower bound). The opening
    /// bracket has already been consumed.
    fn parse_range(&mut self, lower_bound: Bound) -> Result<Expr, ParseError> {
        let lower = self.parse_expr(0)?;
        match self.peek().map(|t| &t.kind) {
            Some(TokenKind::Comma) => {
                self.advance(); // consume `,`
                self.finish_range(lower_bound, lower)
            }
            _ => Err(self.delimiter_error(",")),
        }
    }

    /// Finishes parsing a range after the lower bound and separating comma have
    /// been consumed: reads the upper bound and the closing delimiter, which
    /// determines the upper bound's inclusivity (`]` inclusive, `)` exclusive).
    fn finish_range(&mut self, lower_bound: Bound, lower: Expr) -> Result<Expr, ParseError> {
        let upper = self.parse_expr(0)?;
        let upper_bound = match self.peek().map(|t| &t.kind) {
            Some(TokenKind::RBracket) => {
                self.advance();
                Bound::Inclusive
            }
            Some(TokenKind::RParen) => {
                self.advance();
                Bound::Exclusive
            }
            _ => return Err(self.delimiter_error("] or )")),
        };
        Ok(Expr::Range {
            lower_bound,
            lower: Box::new(lower),
            upper: Box::new(upper),
            upper_bound,
        })
    }

    /// Builds a delimiter error against the current token (or end of input).
    fn delimiter_error(&self, expected: &str) -> ParseError {
        match self.peek() {
            Some(tok) => ParseError::ExpectedDelimiter {
                expected: expected.to_string(),
                found: tok.kind.to_string(),
                column: tok.column,
            },
            None => ParseError::UnexpectedEof {
                expected: expected.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand constructors keep the expected trees readable.
    fn int(n: i64) -> Expr {
        Expr::Int(n)
    }
    fn ident(s: &str) -> Expr {
        Expr::Ident(s.into())
    }
    fn bin(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
        Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }
    fn un(op: UnaryOp, operand: Expr) -> Expr {
        Expr::Unary {
            op,
            operand: Box::new(operand),
        }
    }

    // ---- Literals & atoms ----

    #[test]
    fn parses_int_float_string_ident() {
        assert_eq!(parse_str("42").unwrap(), int(42));
        assert_eq!(parse_str("3.5").unwrap(), Expr::Float(3.5));
        assert_eq!(parse_str("\"hi\"").unwrap(), Expr::Str("hi".into()));
        assert_eq!(parse_str("Time").unwrap(), ident("Time"));
    }

    // ---- Precedence ----

    #[test]
    fn mul_binds_tighter_than_add() {
        // 1 + 2 * 3  ==  1 + (2 * 3)
        assert_eq!(
            parse_str("1 + 2 * 3").unwrap(),
            bin(BinaryOp::Add, int(1), bin(BinaryOp::Mul, int(2), int(3)))
        );
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // a || b && c  ==  a || (b && c)
        assert_eq!(
            parse_str("a || b && c").unwrap(),
            bin(
                BinaryOp::Or,
                ident("a"),
                bin(BinaryOp::And, ident("b"), ident("c"))
            )
        );
    }

    #[test]
    fn relational_binds_tighter_than_logical() {
        // Time >= 30 && AnimElem = 2
        assert_eq!(
            parse_str("Time >= 30 && AnimElem = 2").unwrap(),
            bin(
                BinaryOp::And,
                bin(BinaryOp::Ge, ident("Time"), int(30)),
                bin(BinaryOp::Eq, ident("AnimElem"), int(2)),
            )
        );
    }

    #[test]
    fn additive_is_left_associative() {
        // 1 - 2 - 3  ==  (1 - 2) - 3
        assert_eq!(
            parse_str("1 - 2 - 3").unwrap(),
            bin(BinaryOp::Sub, bin(BinaryOp::Sub, int(1), int(2)), int(3))
        );
    }

    #[test]
    fn relational_binds_tighter_than_bitwise() {
        // Per MUGEN precedence (low->high: `|| && | ^ & relational ...`),
        // relational binds tighter than bitwise, so `a & b = c` parses as
        // `a & (b = c)`.
        assert_eq!(
            parse_str("a & b = c").unwrap(),
            bin(
                BinaryOp::BitAnd,
                ident("a"),
                bin(BinaryOp::Eq, ident("b"), ident("c")),
            )
        );
    }

    #[test]
    fn bitwise_or_below_logical_and() {
        // `a && b | c` -> `a && (b | c)` because bitwise binds tighter than &&.
        assert_eq!(
            parse_str("a && b | c").unwrap(),
            bin(
                BinaryOp::And,
                ident("a"),
                bin(BinaryOp::BitOr, ident("b"), ident("c")),
            )
        );
    }

    // ---- Exponent: right-associative, binds above unary ----

    #[test]
    fn pow_is_right_associative() {
        // 2 ** 3 ** 2  ==  2 ** (3 ** 2)
        assert_eq!(
            parse_str("2 ** 3 ** 2").unwrap(),
            bin(BinaryOp::Pow, int(2), bin(BinaryOp::Pow, int(3), int(2)))
        );
    }

    #[test]
    fn pow_binds_tighter_than_mul() {
        // 2 * 3 ** 2  ==  2 * (3 ** 2)
        assert_eq!(
            parse_str("2 * 3 ** 2").unwrap(),
            bin(BinaryOp::Mul, int(2), bin(BinaryOp::Pow, int(3), int(2)))
        );
    }

    #[test]
    fn unary_minus_then_pow() {
        // -2 ** 2 : unary binds looser than `**`, so this is -(2 ** 2).
        assert_eq!(
            parse_str("-2 ** 2").unwrap(),
            un(UnaryOp::Neg, bin(BinaryOp::Pow, int(2), int(2)))
        );
    }

    // ---- Unary ----

    #[test]
    fn unary_operators() {
        assert_eq!(parse_str("-5").unwrap(), un(UnaryOp::Neg, int(5)));
        assert_eq!(parse_str("!a").unwrap(), un(UnaryOp::Not, ident("a")));
        assert_eq!(parse_str("~bits").unwrap(), un(UnaryOp::BitNot, ident("bits")));
        // Chained prefixes.
        assert_eq!(
            parse_str("!!a").unwrap(),
            un(UnaryOp::Not, un(UnaryOp::Not, ident("a")))
        );
    }

    #[test]
    fn unary_in_arithmetic() {
        // 1 + -2  ==  1 + (-2)
        assert_eq!(
            parse_str("1 + -2").unwrap(),
            bin(BinaryOp::Add, int(1), un(UnaryOp::Neg, int(2)))
        );
    }

    // ---- Parens ----

    #[test]
    fn parens_override_precedence() {
        // (1 + 2) * 3
        assert_eq!(
            parse_str("(1 + 2) * 3").unwrap(),
            bin(BinaryOp::Mul, bin(BinaryOp::Add, int(1), int(2)), int(3))
        );
    }

    // ---- Calls ----

    #[test]
    fn function_call_three_args() {
        // cond(a, b, c)
        assert_eq!(
            parse_str("cond(a, b, c)").unwrap(),
            Expr::Call {
                name: "cond".into(),
                args: vec![ident("a"), ident("b"), ident("c")],
            }
        );
    }

    #[test]
    fn parameterized_trigger_var() {
        // var(0) is a call with one int arg.
        assert_eq!(
            parse_str("var(0)").unwrap(),
            Expr::Call {
                name: "var".into(),
                args: vec![int(0)],
            }
        );
    }

    #[test]
    fn nested_call_with_arithmetic() {
        // cond(var(0) > 0, life - 10, life)
        assert_eq!(
            parse_str("cond(var(0) > 0, life - 10, life)").unwrap(),
            Expr::Call {
                name: "cond".into(),
                args: vec![
                    bin(
                        BinaryOp::Gt,
                        Expr::Call {
                            name: "var".into(),
                            args: vec![int(0)],
                        },
                        int(0),
                    ),
                    bin(BinaryOp::Sub, ident("life"), int(10)),
                    ident("life"),
                ],
            }
        );
    }

    #[test]
    fn zero_arg_call() {
        // `random()` written with empty parens.
        assert_eq!(
            parse_str("random()").unwrap(),
            Expr::Call {
                name: "random".into(),
                args: vec![],
            }
        );
    }

    #[test]
    fn min_max_two_args() {
        assert_eq!(
            parse_str("max(a, b)").unwrap(),
            Expr::Call {
                name: "max".into(),
                args: vec![ident("a"), ident("b")],
            }
        );
    }

    // ---- Ranges ----

    #[test]
    fn inclusive_range_rhs_of_eq() {
        // AnimElem = [6,9]
        assert_eq!(
            parse_str("AnimElem = [6,9]").unwrap(),
            bin(
                BinaryOp::Eq,
                ident("AnimElem"),
                Expr::Range {
                    lower_bound: Bound::Inclusive,
                    lower: Box::new(int(6)),
                    upper: Box::new(int(9)),
                    upper_bound: Bound::Inclusive,
                },
            )
        );
    }

    #[test]
    fn all_four_range_bound_combinations() {
        let mk = |lb, ub| Expr::Range {
            lower_bound: lb,
            lower: Box::new(int(1)),
            upper: Box::new(int(2)),
            upper_bound: ub,
        };
        assert_eq!(
            parse_str("x = [1,2]").unwrap(),
            bin(BinaryOp::Eq, ident("x"), mk(Bound::Inclusive, Bound::Inclusive))
        );
        assert_eq!(
            parse_str("x = (1,2]").unwrap(),
            bin(BinaryOp::Eq, ident("x"), mk(Bound::Exclusive, Bound::Inclusive))
        );
        assert_eq!(
            parse_str("x = [1,2)").unwrap(),
            bin(BinaryOp::Eq, ident("x"), mk(Bound::Inclusive, Bound::Exclusive))
        );
        assert_eq!(
            parse_str("x = (1,2)").unwrap(),
            bin(BinaryOp::Eq, ident("x"), mk(Bound::Exclusive, Bound::Exclusive))
        );
    }

    #[test]
    fn range_with_ne() {
        // AnimElem != [1,3]
        assert_eq!(
            parse_str("AnimElem != [1,3]").unwrap(),
            bin(
                BinaryOp::Ne,
                ident("AnimElem"),
                Expr::Range {
                    lower_bound: Bound::Inclusive,
                    lower: Box::new(int(1)),
                    upper: Box::new(int(3)),
                    upper_bound: Bound::Inclusive,
                },
            )
        );
    }

    #[test]
    fn paren_grouping_is_not_a_range_without_comma() {
        // (a + b) is plain grouping, not a range.
        assert_eq!(
            parse_str("(a + b)").unwrap(),
            bin(BinaryOp::Add, ident("a"), ident("b"))
        );
    }

    // ---- Equality alias ----

    #[test]
    fn eq_and_eqeq_produce_same_op() {
        let with_single = parse_str("a = b").unwrap();
        let with_double = parse_str("a == b").unwrap();
        assert_eq!(with_single, bin(BinaryOp::Eq, ident("a"), ident("b")));
        assert_eq!(with_single, with_double);
    }

    // ---- Error / Unknown cases (must be recoverable, never panic) ----

    #[test]
    fn unknown_token_is_recoverable_error() {
        // `@` lexes as Unknown; parsing must surface a ParseError, not panic.
        let err = parse_str("1 @ 2").unwrap_err();
        // `1` parses as the expression; `@` is then a trailing/unknown token.
        assert!(
            matches!(
                err,
                ParseError::UnknownToken { ch: '@', .. } | ParseError::UnexpectedToken { .. }
            ),
            "unexpected error variant: {err:?}"
        );
    }

    #[test]
    fn lone_unknown_token() {
        let err = parse_str("@").unwrap_err();
        assert_eq!(err, ParseError::UnknownToken { ch: '@', column: 0 });
    }

    #[test]
    fn empty_input_is_error() {
        assert_eq!(parse_str("").unwrap_err(), ParseError::Empty);
        assert_eq!(parse_str("   ; comment only").unwrap_err(), ParseError::Empty);
    }

    #[test]
    fn trailing_operator_is_eof_error() {
        let err = parse_str("1 +").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedEof { .. }), "{err:?}");
    }

    #[test]
    fn unclosed_paren_is_error() {
        let err = parse_str("(1 + 2").unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::ExpectedDelimiter { .. } | ParseError::UnexpectedEof { .. }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn unclosed_call_is_error() {
        let err = parse_str("cond(a, b").unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::ExpectedDelimiter { .. } | ParseError::UnexpectedEof { .. }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn trailing_tokens_are_error() {
        // `1 2` — a complete expr `1` followed by a stray `2`.
        let err = parse_str("1 2").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn stray_close_paren_is_error() {
        let err = parse_str(")").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn range_missing_comma_is_error() {
        // `[1 2]` — missing the separating comma.
        let err = parse_str("x = [1 2]").unwrap_err();
        assert!(
            matches!(err, ParseError::ExpectedDelimiter { .. }),
            "{err:?}"
        );
    }

    // ---- Display impls ----

    #[test]
    fn operator_display() {
        assert_eq!(BinaryOp::Pow.to_string(), "**");
        assert_eq!(BinaryOp::Eq.to_string(), "=");
        assert_eq!(BinaryOp::Ne.to_string(), "!=");
        assert_eq!(UnaryOp::Neg.to_string(), "-");
        assert_eq!(UnaryOp::BitNot.to_string(), "~");
    }

    // ---- Realistic MUGEN triggers ----

    #[test]
    fn realistic_trigger_with_redirection_call() {
        // !gethitvar(isbound)
        assert_eq!(
            parse_str("!gethitvar(isbound)").unwrap(),
            un(
                UnaryOp::Not,
                Expr::Call {
                    name: "gethitvar".into(),
                    args: vec![ident("isbound")],
                }
            )
        );
    }

    // ---- Redirection (task 4.8) ----

    /// Shorthand for a redirected expression.
    fn redirected(target: Redirect, expr: Expr) -> Expr {
        Expr::Redirected {
            target,
            expr: Box::new(expr),
        }
    }

    #[test]
    fn redirect_keywords_without_id_parse() {
        // Each id-less keyword followed by `, expr` redirects the sub-expression.
        assert_eq!(
            parse_str("enemy, P2BodyDistX").unwrap(),
            redirected(Redirect::Enemy, ident("P2BodyDistX"))
        );
        assert_eq!(
            parse_str("parent, animtime").unwrap(),
            redirected(Redirect::Parent, ident("animtime"))
        );
        assert_eq!(
            parse_str("root, var(0)").unwrap(),
            redirected(Redirect::Root, Expr::Call { name: "var".into(), args: vec![int(0)] })
        );
        assert_eq!(
            parse_str("partner, life").unwrap(),
            redirected(Redirect::Partner, ident("life"))
        );
    }

    #[test]
    fn redirect_keywords_with_id_parse() {
        assert_eq!(
            parse_str("helper(1234), stateno").unwrap(),
            redirected(Redirect::Helper(1234), ident("stateno"))
        );
        assert_eq!(
            parse_str("enemynear(1), life").unwrap(),
            redirected(Redirect::EnemyNear(1), ident("life"))
        );
        assert_eq!(
            parse_str("target(2), gethitvar(xveladd)").unwrap(),
            redirected(
                Redirect::Target(Some(2)),
                call("gethitvar", vec![ident("xveladd")]),
            )
        );
        assert_eq!(
            parse_str("playerid(7), stateno").unwrap(),
            redirected(Redirect::PlayerId(7), ident("stateno"))
        );
    }

    #[test]
    fn redirect_keywords_are_case_insensitive() {
        assert_eq!(
            parse_str("ENEMY, life").unwrap(),
            redirected(Redirect::Enemy, ident("life"))
        );
        assert_eq!(
            parse_str("Helper(1), life").unwrap(),
            redirected(Redirect::Helper(1), ident("life"))
        );
    }

    #[test]
    fn cb8_enemy_index_lowers_to_enemynear() {
        // CB8: bare `enemy` and `enemy(0)` → Redirect::Enemy; `enemy(n)` (n>0) →
        // Redirect::EnemyNear(n). The index is preserved, never dropped.
        assert_eq!(
            parse_str("enemy, life").unwrap(),
            redirected(Redirect::Enemy, ident("life"))
        );
        assert_eq!(
            parse_str("enemy(0), life").unwrap(),
            redirected(Redirect::Enemy, ident("life"))
        );
        assert_eq!(
            parse_str("enemy(2), life").unwrap(),
            redirected(Redirect::EnemyNear(2), ident("life"))
        );
    }

    #[test]
    fn redirect_applies_to_whole_trailing_expression() {
        // Redirection binds looser than every operator: `enemy, life + 1`
        // redirects the entire `life + 1`, not just `life`.
        assert_eq!(
            parse_str("enemy, life + 1").unwrap(),
            redirected(Redirect::Enemy, bin(BinaryOp::Add, ident("life"), int(1)))
        );
        assert_eq!(
            parse_str("root, var(0) = 5 && time > 0").unwrap(),
            redirected(
                Redirect::Root,
                bin(
                    BinaryOp::And,
                    bin(BinaryOp::Eq, call("var", vec![int(0)]), int(5)),
                    bin(BinaryOp::Gt, ident("time"), int(0)),
                ),
            )
        );
    }

    #[test]
    fn nested_redirect_chains_parse() {
        // `enemy, helper(1), x` → Redirected(Enemy, Redirected(Helper(1), x)).
        assert_eq!(
            parse_str("enemy, helper(1), x").unwrap(),
            redirected(
                Redirect::Enemy,
                redirected(Redirect::Helper(1), ident("x")),
            )
        );
    }

    #[test]
    fn redirect_keyword_without_comma_is_ordinary_expression() {
        // A redirection keyword NOT followed by a comma is left untouched: a
        // standalone `helper(1234)` is a plain call, and `root` alone is a bare
        // ident — non-redirection expressions are unchanged.
        assert_eq!(
            parse_str("helper(1234)").unwrap(),
            call("helper", vec![int(1234)])
        );
        assert_eq!(parse_str("root").unwrap(), ident("root"));
        // And one used as an operand stays a normal call.
        assert_eq!(
            parse_str("helper(1) + 1").unwrap(),
            bin(BinaryOp::Add, call("helper", vec![int(1)]), int(1))
        );
    }

    #[test]
    fn redirect_inside_call_argument_parses() {
        // A redirect can appear as a call argument (each arg is parse_expr(0)),
        // while the call's own commas still separate arguments.
        assert_eq!(
            parse_str("cond(enemy, life, 1, 0)").unwrap(),
            call(
                "cond",
                vec![
                    redirected(Redirect::Enemy, ident("life")),
                    int(1),
                    int(0),
                ],
            )
        );
    }

    #[test]
    fn malformed_redirect_missing_expr_is_recoverable_error() {
        // `enemy,` — comma with no following sub-expression.
        let err = parse_str("enemy,").unwrap_err();
        assert!(matches!(err, ParseError::MalformedRedirect { .. }), "{err:?}");
    }

    #[test]
    fn malformed_redirect_bad_id_for_keyword_is_error() {
        // `parent(1), x` — parent takes no id; `playerid, x` — playerid needs one.
        assert!(matches!(
            parse_str("parent(1), x").unwrap_err(),
            ParseError::MalformedRedirect { .. }
        ));
        assert!(matches!(
            parse_str("playerid, x").unwrap_err(),
            ParseError::MalformedRedirect { .. }
        ));
    }

    #[test]
    fn unknown_keyword_before_comma_is_not_a_redirect() {
        // A non-keyword ident before a top-level comma is NOT a redirect: it is
        // the real two-parameter trigger form (`AnimElem = 3, -1`), which the
        // expression grammar leaves as a trailing-token error for the CNS layer.
        let err = parse_str("foo, bar").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn does_not_panic_on_fuzzy_garbage() {
        // A spread of malformed inputs must all return Err, never panic.
        for src in [
            // Redirection garbage variants must not panic either.
            "enemy,", "helper(,", "playerid,", "parent(1),", "enemy(),",
            "root,,x", "helper(1)(2),x", ",enemy", "enemy(", "enemynear(-",
            "", "(", ")", "[", "]", ",", "+", "**", "1 +", "a b c", "((1)",
            "[1,2", "cond(", "= = =", "1 ** ** 2", "var(", "@#$", "1,2,3",
        ] {
            let _ = parse_str(src); // Result; panicking here fails the test.
        }
    }

    // =====================================================================
    // Proctor: additional coverage — edge cases, error paths, MUGEN semantics.
    // These tests do not modify the parser; they exercise the acceptance
    // criteria more thoroughly and pin down behaviors the evaluator relies on.
    // =====================================================================

    /// Shorthand for a range literal expression.
    fn range(lb: Bound, lo: Expr, hi: Expr, ub: Bound) -> Expr {
        Expr::Range {
            lower_bound: lb,
            lower: Box::new(lo),
            upper: Box::new(hi),
            upper_bound: ub,
        }
    }
    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            name: name.into(),
            args,
        }
    }

    // ---- AC1: parse(tokens) entry point, not just parse_str ----

    #[test]
    fn parse_consumes_token_slice_directly() {
        // The `parse(&[Token])` path is the primary API for the CNS parser, which
        // already holds tokens. Exercise it directly rather than only via
        // parse_str.
        let toks = tokenize("a + b * c");
        assert_eq!(
            parse(&toks).unwrap(),
            bin(
                BinaryOp::Add,
                ident("a"),
                bin(BinaryOp::Mul, ident("b"), ident("c")),
            )
        );
    }

    #[test]
    fn parse_empty_token_slice_is_empty_error() {
        // An empty token slice (e.g. a blank/comment-only RHS) is the `Empty`
        // error, distinct from a malformed expression.
        let toks: Vec<Token> = Vec::new();
        assert_eq!(parse(&toks).unwrap_err(), ParseError::Empty);
    }

    #[test]
    fn negative_int_literal_via_neg() {
        // The lexer never folds the sign into the literal; `-5` is unary Neg over
        // Int(5). Confirm and contrast with a positive literal.
        assert_eq!(parse_str("-5").unwrap(), un(UnaryOp::Neg, int(5)));
        assert_eq!(parse_str("5").unwrap(), int(5));
    }

    #[test]
    fn float_literal_preserved() {
        // Float atoms must survive parsing untouched (the evaluator distinguishes
        // int vs float MUGEN semantics).
        assert_eq!(parse_str("0.5").unwrap(), Expr::Float(0.5));
        assert_eq!(
            parse_str("0.5 + 1").unwrap(),
            bin(BinaryOp::Add, Expr::Float(0.5), int(1))
        );
    }

    // ---- AC2: precedence & associativity — the full ladder ----

    #[test]
    fn full_precedence_ladder_in_one_expr() {
        // a || b && c | d = e + f * g ** h
        // Expected nesting (low->high binding):
        //   a || (b && ((c | (d = (e + (f * (g ** h)))))))
        let expected = bin(
            BinaryOp::Or,
            ident("a"),
            bin(
                BinaryOp::And,
                ident("b"),
                bin(
                    BinaryOp::BitOr,
                    ident("c"),
                    bin(
                        BinaryOp::Eq,
                        ident("d"),
                        bin(
                            BinaryOp::Add,
                            ident("e"),
                            bin(
                                BinaryOp::Mul,
                                ident("f"),
                                bin(BinaryOp::Pow, ident("g"), ident("h")),
                            ),
                        ),
                    ),
                ),
            ),
        );
        assert_eq!(parse_str("a || b && c | d = e + f * g ** h").unwrap(), expected);
    }

    #[test]
    fn or_is_left_associative() {
        // a || b || c == (a || b) || c
        assert_eq!(
            parse_str("a || b || c").unwrap(),
            bin(
                BinaryOp::Or,
                bin(BinaryOp::Or, ident("a"), ident("b")),
                ident("c"),
            )
        );
    }

    #[test]
    fn and_is_left_associative() {
        // a && b && c == (a && b) && c
        assert_eq!(
            parse_str("a && b && c").unwrap(),
            bin(
                BinaryOp::And,
                bin(BinaryOp::And, ident("a"), ident("b")),
                ident("c"),
            )
        );
    }

    #[test]
    fn multiplicative_is_left_associative() {
        // 8 / 4 / 2 == (8 / 4) / 2, not 8 / (4 / 2). Associativity matters for
        // non-commutative ops like / and %.
        assert_eq!(
            parse_str("8 / 4 / 2").unwrap(),
            bin(BinaryOp::Div, bin(BinaryOp::Div, int(8), int(4)), int(2))
        );
        assert_eq!(
            parse_str("10 % 4 % 3").unwrap(),
            bin(BinaryOp::Mod, bin(BinaryOp::Mod, int(10), int(4)), int(3))
        );
    }

    #[test]
    fn bitwise_operators_share_one_level_left_assoc() {
        // `| ^ &` are all level 3, left-to-right: a | b ^ c & d == ((a | b) ^ c) & d
        assert_eq!(
            parse_str("a | b ^ c & d").unwrap(),
            bin(
                BinaryOp::BitAnd,
                bin(
                    BinaryOp::BitXor,
                    bin(BinaryOp::BitOr, ident("a"), ident("b")),
                    ident("c"),
                ),
                ident("d"),
            )
        );
    }

    #[test]
    fn all_relational_ops_same_level_left_assoc() {
        // Relational ops share level 4 and fold left: a < b > c == (a < b) > c.
        assert_eq!(
            parse_str("a < b > c").unwrap(),
            bin(BinaryOp::Gt, bin(BinaryOp::Lt, ident("a"), ident("b")), ident("c"))
        );
        // Each relational token maps to its own BinaryOp.
        assert_eq!(
            parse_str("a <= b").unwrap(),
            bin(BinaryOp::Le, ident("a"), ident("b"))
        );
        assert_eq!(
            parse_str("a >= b").unwrap(),
            bin(BinaryOp::Ge, ident("a"), ident("b"))
        );
        assert_eq!(
            parse_str("a != b").unwrap(),
            bin(BinaryOp::Ne, ident("a"), ident("b"))
        );
    }

    #[test]
    fn additive_binds_tighter_than_relational() {
        // a + b = c + d  ==  (a + b) = (c + d)
        assert_eq!(
            parse_str("a + b = c + d").unwrap(),
            bin(
                BinaryOp::Eq,
                bin(BinaryOp::Add, ident("a"), ident("b")),
                bin(BinaryOp::Add, ident("c"), ident("d")),
            )
        );
    }

    #[test]
    fn pow_binds_tighter_than_unary_on_the_left() {
        // -x ** 2 parses as -(x ** 2): unary Neg sits BELOW `**`, so the power
        // grabs the base before the negation applies. This is the documented
        // (and MUGEN-matching) behavior and a classic precedence gotcha.
        assert_eq!(
            parse_str("-x ** 2").unwrap(),
            un(UnaryOp::Neg, bin(BinaryOp::Pow, ident("x"), int(2)))
        );
    }

    #[test]
    fn pow_with_unary_exponent_is_right_recursive() {
        // 2 ** -3 : the exponent may itself carry a prefix.
        assert_eq!(
            parse_str("2 ** -3").unwrap(),
            bin(BinaryOp::Pow, int(2), un(UnaryOp::Neg, int(3)))
        );
        // 2 ** -3 ** 2 : right-assoc through the prefix == 2 ** (-(3 ** 2)).
        assert_eq!(
            parse_str("2 ** -3 ** 2").unwrap(),
            bin(
                BinaryOp::Pow,
                int(2),
                un(UnaryOp::Neg, bin(BinaryOp::Pow, int(3), int(2))),
            )
        );
    }

    #[test]
    fn deeply_chained_unary_prefixes() {
        // ~-!a : three distinct prefixes nest right-to-left.
        assert_eq!(
            parse_str("~-!a").unwrap(),
            un(UnaryOp::BitNot, un(UnaryOp::Neg, un(UnaryOp::Not, ident("a"))))
        );
    }

    #[test]
    fn unary_not_binds_looser_than_pow_but_applies_to_call() {
        // !cond(...) — already covered for gethitvar; here ensure `!` over a power
        // expression groups as !(2 ** 3) since `**` binds tighter than unary.
        assert_eq!(
            parse_str("!2 ** 3").unwrap(),
            un(UnaryOp::Not, bin(BinaryOp::Pow, int(2), int(3)))
        );
    }

    // ---- AC3: function calls & parameterized triggers ----

    #[test]
    fn all_named_functions_from_spec_parse_as_calls() {
        // Every function the task calls out must parse as Expr::Call with the
        // right arity. Names are case-preserved.
        assert_eq!(
            parse_str("ifelse(a, b, c)").unwrap(),
            call("ifelse", vec![ident("a"), ident("b"), ident("c")])
        );
        for f in ["abs", "floor", "ceil", "sin", "cos", "atan", "exp", "ln"] {
            let src = format!("{f}(x)");
            assert_eq!(
                parse_str(&src).unwrap(),
                call(f, vec![ident("x")]),
                "function {f} should parse as a 1-arg call"
            );
        }
        assert_eq!(
            parse_str("min(a, b)").unwrap(),
            call("min", vec![ident("a"), ident("b")])
        );
        assert_eq!(
            parse_str("max(a, b)").unwrap(),
            call("max", vec![ident("a"), ident("b")])
        );
    }

    #[test]
    fn random_bare_ident_vs_random_call() {
        // `random` with no parens is a bare trigger identifier; `random()` is a
        // zero-arg call. The parser must distinguish them.
        assert_eq!(parse_str("random").unwrap(), ident("random"));
        assert_eq!(parse_str("random()").unwrap(), call("random", vec![]));
    }

    #[test]
    fn parameterized_triggers_fvar_and_sysvar() {
        // fvar(1), sysvar(0) — float/system variable accessors parse as calls.
        assert_eq!(parse_str("fvar(1)").unwrap(), call("fvar", vec![int(1)]));
        assert_eq!(parse_str("sysvar(0)").unwrap(), call("sysvar", vec![int(0)]));
    }

    #[test]
    fn call_with_expression_args() {
        // min(life + 1, 100) — argument expressions are full expressions, parsed
        // with precedence (each arg is its own parse_expr(0)).
        assert_eq!(
            parse_str("min(life + 1, 100)").unwrap(),
            call("min", vec![bin(BinaryOp::Add, ident("life"), int(1)), int(100)])
        );
    }

    #[test]
    fn call_arg_may_contain_top_level_logical_op() {
        // A comma separates args, but commas only at the TOP level of the arg
        // list — a `||` inside an arg stays part of that single argument.
        assert_eq!(
            parse_str("cond(a || b, t, f)").unwrap(),
            call(
                "cond",
                vec![
                    bin(BinaryOp::Or, ident("a"), ident("b")),
                    ident("t"),
                    ident("f"),
                ],
            )
        );
    }

    #[test]
    fn doubly_nested_calls() {
        // max(min(a, b), c) — nesting through the atom path.
        assert_eq!(
            parse_str("max(min(a, b), c)").unwrap(),
            call(
                "max",
                vec![call("min", vec![ident("a"), ident("b")]), ident("c")],
            )
        );
    }

    #[test]
    fn call_result_participates_in_arithmetic() {
        // abs(x) + 1 — a call is an atom that can be an operand.
        assert_eq!(
            parse_str("abs(x) + 1").unwrap(),
            bin(BinaryOp::Add, call("abs", vec![ident("x")]), int(1))
        );
    }

    #[test]
    fn whitespace_between_ident_and_paren_still_call() {
        // Real KFM content writes `AnimElemTime (2)` with a space (kfm.cns line
        // 333). The lexer drops whitespace, so the parser sees Ident then LParen
        // and MUST treat it as a call — not a bare ident followed by a group.
        assert_eq!(
            parse_str("AnimElemTime (2)").unwrap(),
            call("AnimElemTime", vec![int(2)])
        );
        assert_eq!(
            parse_str("(AnimElemTime (2) >= 0)").unwrap(),
            bin(BinaryOp::Ge, call("AnimElemTime", vec![int(2)]), int(0))
        );
    }

    // ---- AC4: range literals, all bound combos, as RHS of = / != ----

    #[test]
    fn range_with_float_and_negative_bounds() {
        // Ranges can hold arbitrary expressions, including negatives and floats.
        assert_eq!(
            parse_str("x = [-1, 2.5]").unwrap(),
            bin(
                BinaryOp::Eq,
                ident("x"),
                range(
                    Bound::Inclusive,
                    un(UnaryOp::Neg, int(1)),
                    Expr::Float(2.5),
                    Bound::Inclusive,
                ),
            )
        );
    }

    #[test]
    fn range_bound_expressions_are_full_expressions() {
        // GetHitVar(animtype) = [3, 5] mirrors common1.cns; bounds can also be
        // arithmetic: [a+1, b*2].
        assert_eq!(
            parse_str("GetHitVar(animtype) != [3,5]").unwrap(),
            bin(
                BinaryOp::Ne,
                call("GetHitVar", vec![ident("animtype")]),
                range(Bound::Inclusive, int(3), int(5), Bound::Inclusive),
            )
        );
        assert_eq!(
            parse_str("v = [a + 1, b * 2]").unwrap(),
            bin(
                BinaryOp::Eq,
                ident("v"),
                range(
                    Bound::Inclusive,
                    bin(BinaryOp::Add, ident("a"), int(1)),
                    bin(BinaryOp::Mul, ident("b"), int(2)),
                    Bound::Inclusive,
                ),
            )
        );
    }

    #[test]
    fn exclusive_lower_range_disambiguated_from_grouping() {
        // `(0, 10]` is a range (comma after first sub-expr); `(0)` is grouping.
        assert_eq!(
            parse_str("x = (0, 10]").unwrap(),
            bin(
                BinaryOp::Eq,
                ident("x"),
                range(Bound::Exclusive, int(0), int(10), Bound::Inclusive),
            )
        );
        // Plain grouping yields the inner expression unchanged.
        assert_eq!(parse_str("(0)").unwrap(), int(0));
    }

    #[test]
    fn range_as_rhs_of_eqeq_alias() {
        // The `==` alias must also accept a range RHS, same as `=`.
        assert_eq!(
            parse_str("x == [1,3]").unwrap(),
            bin(
                BinaryOp::Eq,
                ident("x"),
                range(Bound::Inclusive, int(1), int(3), Bound::Inclusive),
            )
        );
    }

    // ---- AC2/AC3: == / = alias equivalence across contexts ----

    #[test]
    fn eq_alias_equivalent_inside_larger_expr() {
        // The alias normalizes to BinaryOp::Eq everywhere, not just at top level.
        let single = parse_str("Time = 0 && Life = 100").unwrap();
        let double = parse_str("Time == 0 && Life == 100").unwrap();
        assert_eq!(single, double);
        assert_eq!(
            single,
            bin(
                BinaryOp::And,
                bin(BinaryOp::Eq, ident("Time"), int(0)),
                bin(BinaryOp::Eq, ident("Life"), int(100)),
            )
        );
    }

    // ---- AC5: error paths — recoverable, carry column info, never panic ----

    #[test]
    fn unknown_token_reports_correct_column() {
        // The column on UnknownToken must point at the offending char so the CNS
        // parser can produce a useful warn!.
        let err = parse_str("a @ b").unwrap_err();
        match err {
            // `a` parses; `@` is then either flagged as a trailing unexpected
            // token or, depending on lookahead, an unknown token. Either way the
            // column is the `@` at index 2.
            ParseError::UnexpectedToken { column, .. }
            | ParseError::UnknownToken { column, .. } => {
                assert_eq!(column, 2, "column should point at `@`");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn leading_binary_operator_is_error() {
        // `* 3` — a binary operator with no left operand. parse_atom sees `*`,
        // which is not an atom, and reports UnexpectedToken (not a panic).
        let err = parse_str("* 3").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn double_binary_operator_is_error() {
        // `1 + * 2` — after `+`, the parser needs an operand but finds `*`.
        let err = parse_str("1 + * 2").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn empty_parens_as_grouping_is_error() {
        // `()` is neither a valid group nor a call here (no callee). parse_atom
        // recurses into parse_expr which immediately hits `)`.
        let err = parse_str("()").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn trailing_comma_in_call_is_error() {
        // `cond(a, b, )` — a dangling comma leaves an empty final argument.
        let err = parse_str("cond(a, b, )").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn range_missing_upper_bound_is_error() {
        // `[1,]` — comma present but no upper bound expression.
        let err = parse_str("x = [1,]").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn unclosed_range_bracket_is_error() {
        // `[1,2` — runs out of input before the closing delimiter.
        let err = parse_str("x = [1,2").unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::ExpectedDelimiter { .. } | ParseError::UnexpectedEof { .. }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn assign_operator_is_not_an_expression_operator() {
        // `:=` is the redirection/assignment op; it is NOT a binary expression
        // operator here. `var(0) := 5` parses the call, then `:=` is a leftover
        // token the top-level reports as unexpected. It must not panic.
        let err = parse_str("var(0) := 5").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn top_level_comma_on_non_animelem_leaves_trailing_tokens() {
        // A top-level comma that is NOT the `AnimElem = N, op M` tail is still a
        // trailing-token error: `Time = 3, -1` is not a two-parameter trigger, so
        // the parser consumes `Time = 3` and reports the `,` as unexpected.
        let err = parse_str("Time = 3, -1").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
        // Likewise a bare value with a trailing comma (`5, 6`) is an error.
        let err = parse_str("5, 6").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn animelem_two_parameter_tail_parses() {
        // Task 4.10 gap 2: `AnimElem = 3, -1` (real kfm.cns line 1703) is MUGEN's
        // two-parameter AnimElem syntax, folded into an AnimElemTail node.
        assert_eq!(
            parse_str("AnimElem = 3, -1").unwrap(),
            Expr::AnimElemTail {
                name: "AnimElem".into(),
                element: Box::new(int(3)),
                op: BinaryOp::Eq, // omitted operator defaults to `=`
                operand: Box::new(un(UnaryOp::Neg, int(1))),
            }
        );
        // With an explicit operator (kfm.cns line 3200): `AnimElem = 2, >= 0`.
        assert_eq!(
            parse_str("AnimElem = 2, >= 0").unwrap(),
            Expr::AnimElemTail {
                name: "AnimElem".into(),
                element: Box::new(int(2)),
                op: BinaryOp::Ge,
                operand: Box::new(int(0)),
            }
        );
        // The whole family + case-insensitive name matching.
        assert_eq!(
            parse_str("animelemtime = 5, <= 3").unwrap(),
            Expr::AnimElemTail {
                name: "animelemtime".into(),
                element: Box::new(int(5)),
                op: BinaryOp::Le,
                operand: Box::new(int(3)),
            }
        );
        // A malformed tail (nothing after the comma) is a recoverable error.
        assert!(matches!(
            parse_str("AnimElem = 2,").unwrap_err(),
            ParseError::UnexpectedEof { .. }
        ));
    }

    #[test]
    fn lone_comparison_operator_rhs_is_error() {
        // `, >= 0` style fragments (kfm.cns line 3200 second parameter) begin
        // with an operator and cannot parse as an expression; must be Err.
        let err = parse_str(">= 0").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn axis_suffixed_component_triggers_parse_as_calls() {
        // Task 4.10 gap 1: `Vel Y`, `Pos X`, … fold to a one-arg call with the
        // axis as an upper-cased string literal argument.
        assert_eq!(parse_str("Vel Y").unwrap(), call("Vel", vec![Expr::Str("Y".into())]));
        assert_eq!(parse_str("Pos X").unwrap(), call("Pos", vec![Expr::Str("X".into())]));
        // Case-insensitive axis word, normalized to upper-case.
        assert_eq!(parse_str("Pos y").unwrap(), call("Pos", vec![Expr::Str("Y".into())]));
        assert_eq!(parse_str("Vel x").unwrap(), call("Vel", vec![Expr::Str("X".into())]));
        // Multi-word trigger names (P2Dist, P2BodyDist, ScreenPos).
        assert_eq!(
            parse_str("P2BodyDist X").unwrap(),
            call("P2BodyDist", vec![Expr::Str("X".into())])
        );
        assert_eq!(
            parse_str("ScreenPos Y").unwrap(),
            call("ScreenPos", vec![Expr::Str("Y".into())])
        );
        // The Z axis is accepted too.
        assert_eq!(parse_str("Pos Z").unwrap(), call("Pos", vec![Expr::Str("Z".into())]));
    }

    #[test]
    fn axis_suffix_participates_in_comparisons_and_redirects() {
        // The folded call is an ordinary atom, so it composes with operators...
        assert_eq!(
            parse_str("Vel Y > 0").unwrap(),
            bin(BinaryOp::Gt, call("Vel", vec![Expr::Str("Y".into())]), int(0))
        );
        assert_eq!(
            parse_str("(Vel y > 0) && (Pos y >= 0)").unwrap(),
            bin(
                BinaryOp::And,
                bin(BinaryOp::Gt, call("Vel", vec![Expr::Str("Y".into())]), int(0)),
                bin(BinaryOp::Ge, call("Pos", vec![Expr::Str("Y".into())]), int(0)),
            )
        );
        // ...and through a redirect: `enemy, P2BodyDist X` (a real KFM shape).
        assert_eq!(
            parse_str("enemy, P2BodyDist X").unwrap(),
            redirected(Redirect::Enemy, call("P2BodyDist", vec![Expr::Str("X".into())]))
        );
    }

    #[test]
    fn non_axis_trailing_ident_is_not_folded() {
        // A non-axis word after an ident is NOT an axis suffix; two adjacent
        // idents stay a (recoverable) trailing-token error.
        let err = parse_str("Vel W").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
        // A bare trigger that merely starts with an axis letter is unaffected: a
        // standalone `Y` is just an ident; `yaccel` is an ident.
        assert_eq!(parse_str("yaccel").unwrap(), ident("yaccel"));
        // `Vel` with no following axis word stays a bare ident.
        assert_eq!(parse_str("Vel").unwrap(), ident("Vel"));
    }

    #[test]
    fn dotted_member_in_call_arg_parses() {
        // Task 4.10 gap 3: `GetHitVar(fall.yvel)` — the dotted member name is one
        // ident token, passed through as the single call argument.
        assert_eq!(
            parse_str("GetHitVar(fall.yvel)").unwrap(),
            call("GetHitVar", vec![ident("fall.yvel")])
        );
        assert_eq!(
            parse_str("GetHitVar(xveladd)").unwrap(),
            call("GetHitVar", vec![ident("xveladd")])
        );
        // It composes in a comparison: `GetHitVar(fall.yvel) = 0` (common1.cns).
        assert_eq!(
            parse_str("GetHitVar(fall.yvel) = 0").unwrap(),
            bin(BinaryOp::Eq, call("GetHitVar", vec![ident("fall.yvel")]), int(0))
        );
    }

    #[test]
    fn error_display_is_non_empty_and_mentions_column() {
        // ParseError implements Display (thiserror); the CNS parser logs it. Spot
        // check the messages are populated and panic-free to format.
        let e = ParseError::UnknownToken { ch: '@', column: 4 };
        let s = e.to_string();
        assert!(s.contains('@') && s.contains('4'), "{s}");
        assert_eq!(ParseError::Empty.to_string(), "empty expression");
        let e2 = ParseError::UnexpectedEof {
            expected: "an expression".into(),
        };
        assert!(e2.to_string().contains("an expression"));
    }

    #[test]
    fn error_is_clone_and_partial_eq() {
        // The CNS layer may compare/store errors; confirm the derives hold.
        let a = ParseError::UnknownToken { ch: '$', column: 0 };
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(a, ParseError::Empty);
    }

    // ---- Non-panic robustness: large/adversarial inputs ----

    #[test]
    fn deeply_nested_input_does_not_overflow_for_reasonable_depth() {
        // A reasonably deep nest of parens/calls must parse without panicking.
        // (Keep depth modest to avoid stress on debug-build stack.)
        let depth = 64;
        let src = format!("{}1{}", "(".repeat(depth), ")".repeat(depth));
        let parsed = parse_str(&src);
        assert!(parsed.is_ok(), "deep parens should parse: {parsed:?}");
        assert_eq!(parsed.unwrap(), int(1));
    }

    #[test]
    fn long_left_associative_chain_does_not_panic() {
        // A long `+` chain exercises the iterative left-fold loop.
        let n = 200;
        let src = (0..n).map(|_| "1").collect::<Vec<_>>().join(" + ");
        let parsed = parse_str(&src).expect("long chain parses");
        // Leftmost-deepest: count that the top node is an Add.
        assert!(matches!(parsed, Expr::Binary { op: BinaryOp::Add, .. }));
    }

    #[test]
    fn extended_fuzzy_garbage_never_panics() {
        // Broader adversarial set covering ranges, assign, mixed delimiters, and
        // operator soup. None may panic; results are ignored.
        for src in [
            "[", "]", "[]", "[,]", "[1,]", "[,2]", "(,)", "(1,)", "(,2)",
            "1 := 2", ":=", "var() ()", "cond(,)", "a,,b", "= [1,2]",
            "[1,2] = x", "((((", "))))", "** **", "1 2 3 4", "!", "~", "- -",
            "()", "f(", "f(,", "[1,2)(3,4]", "&|^~", "1...2", "abs()",
        ] {
            let _ = parse_str(src);
        }
    }

    // ---- AC: real-fixture test (gated on test-assets/ presence) ----

    /// Parses every `trigger... = <rhs>` expression in the real KFM character
    /// files and asserts the parser **never panics** on production content, and
    /// that the *simple single-expression* subset yields an `Ok(Expr)`.
    ///
    /// Two real-MUGEN shapes are NOT single expressions for this grammar and are
    /// resolved by the CNS layer *before* the expression parser runs, so they are
    /// excluded from the must-parse subset (but still covered by the no-panic
    /// guarantee):
    ///   1. **Two-parameter triggers** like `AnimElem = N, op M` — a top-level
    ///      comma separates the value from a comparison operator/operand.
    ///   2. **Multi-word triggers** like `Vel Y`, `Pos X`, `P2BodyDist X` — two
    ///      space-separated identifiers with no operator between them; the second
    ///      word is an axis/sub-selector the CNS layer folds into the trigger
    ///      name. The bare expression grammar (correctly) rejects adjacent atoms.
    ///
    /// Gated on the fixtures' presence so the default `cargo test` still passes
    /// when `test-assets/` is absent.
    #[test]
    fn real_kfm_cns_triggers_parse_without_panic() {
        use std::path::Path;

        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = [
            manifest.join("../../test-assets/kfm/kfm.cns"),
            manifest.join("../../test-assets/kfm/common1.cns"),
        ];

        let mut total = 0usize;
        let mut ok_count = 0usize;
        let mut any_file = false;

        for path in &files {
            if !path.exists() {
                eprintln!("skipping (absent): {path:?}");
                continue;
            }
            any_file = true;
            let text = std::fs::read_to_string(path).expect("read cns fixture");
            for line in text.lines() {
                let trimmed = line.trim_start();
                let lower = trimmed.to_ascii_lowercase();
                // Match trigger assignment lines: `trigger1 = ...`, `triggerall = ...`.
                if !(lower.starts_with("trigger") && line.contains('=')) {
                    continue;
                }
                // Strip any trailing `;` comment, then take the RHS after `=`.
                let no_comment = line.split(';').next().unwrap_or(line);
                let rhs = match no_comment.split_once('=') {
                    Some((_, r)) => r.trim(),
                    None => continue,
                };
                if rhs.is_empty() {
                    continue;
                }
                total += 1;

                // Hard requirement: parsing real content must never panic.
                let result = parse_str(rhs);

                // The "simple single expression" subset must parse OK. We exclude
                // the two CNS-layer shapes documented above.
                let toks = tokenize(rhs);
                let is_simple_single_expr = !rhs_has_top_level_comma(rhs)
                    && !has_adjacent_idents(&toks)
                    && !toks.iter().any(|t| matches!(t.kind, TokenKind::Unknown(_)));
                if is_simple_single_expr {
                    assert!(
                        result.is_ok(),
                        "expected simple real trigger to parse: {rhs:?} -> {result:?}"
                    );
                    ok_count += 1;
                }
            }
        }

        if !any_file {
            eprintln!("skipping real_kfm_cns_triggers_parse_without_panic: no fixtures present");
            return;
        }

        assert!(total > 0, "fixtures present but no trigger lines were found");
        assert!(
            ok_count > 0,
            "expected at least one simple real trigger to parse cleanly"
        );
        eprintln!("real-fixture: {ok_count}/{total} trigger RHS parsed (simple single-expr subset)");
    }

    /// Helper for the fixture test: true if two `Ident` tokens are adjacent with
    /// no operator between them — the signature of a multi-word MUGEN trigger
    /// such as `Vel Y` or `P2BodyDist X`, which the bare expression grammar
    /// rejects (the CNS layer resolves these before parsing).
    fn has_adjacent_idents(toks: &[Token]) -> bool {
        toks.windows(2).any(|w| {
            matches!(w[0].kind, TokenKind::Ident(_)) && matches!(w[1].kind, TokenKind::Ident(_))
        })
    }

    /// Helper for the fixture test: detects a comma that sits at paren/bracket
    /// depth 0 (i.e. a real MUGEN second-parameter separator) versus one nested
    /// inside a call's argument list or a range literal. Quotes are skipped so a
    /// comma inside a string literal does not count.
    fn rhs_has_top_level_comma(rhs: &str) -> bool {
        let mut depth: i32 = 0;
        let mut in_string = false;
        let mut prev_backslash = false;
        for c in rhs.chars() {
            if in_string {
                if prev_backslash {
                    prev_backslash = false;
                } else if c == '\\' {
                    prev_backslash = true;
                } else if c == '"' {
                    in_string = false;
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                ',' if depth <= 0 => return true,
                _ => {}
            }
        }
        false
    }

    // =====================================================================
    // Proctor (task 4.8): redirection-focused gaps — bare-id lowering, the
    // negative `enemy(n)` form, whitespace tolerance, the `parse(&[Token])`
    // entry point, redirects inside groupings, non-int `(id)` fall-through,
    // and the `Expr::Redirected` derive surface. No impl code is modified.
    // =====================================================================

    // ---- AC1/AC2: bare (id-less) keyword lowering for the optional-id forms ----

    #[test]
    fn bare_helper_lowers_to_id_zero() {
        // `helper, x` with no `(id)` selects the engine's "any" helper, modeled as
        // Helper(0) — the id is never dropped, just defaulted (parser.rs
        // `into_redirect`: `Helper(id.unwrap_or(0))`).
        assert_eq!(
            parse_str("helper, stateno").unwrap(),
            redirected(Redirect::Helper(0), ident("stateno"))
        );
    }

    #[test]
    fn bare_enemynear_lowers_to_nearest_zero() {
        // `enemynear, x` (no id) is the nearest enemy → EnemyNear(0).
        assert_eq!(
            parse_str("enemynear, life").unwrap(),
            redirected(Redirect::EnemyNear(0), ident("life"))
        );
    }

    #[test]
    fn bare_target_lowers_to_none() {
        // `target, x` (no id) selects the "any/most-recent" target → Target(None),
        // distinct from `target(0)` → Target(Some(0)).
        assert_eq!(
            parse_str("target, gethitvar(yvel)").unwrap(),
            redirected(Redirect::Target(None), call("gethitvar", vec![ident("yvel")]))
        );
        assert_eq!(
            parse_str("target(0), life").unwrap(),
            redirected(Redirect::Target(Some(0)), ident("life"))
        );
    }

    // ---- AC2/CB8: the negative `enemy(n)` / `enemynear(n)` forms ----

    #[test]
    fn negative_enemy_index_lowers_to_enemynear_negative() {
        // `scan_redirect_id` accepts a leading `-`, and CB8 lowering maps a
        // nonzero `enemy(n)` to EnemyNear(n) — including a negative n. The index
        // is carried verbatim (never silently dropped or clamped to 0).
        assert_eq!(
            parse_str("enemy(-3), life").unwrap(),
            redirected(Redirect::EnemyNear(-3), ident("life"))
        );
        assert_eq!(
            parse_str("enemynear(-1), life").unwrap(),
            redirected(Redirect::EnemyNear(-1), ident("life"))
        );
    }

    #[test]
    fn negative_id_for_target_helper_playerid_parses() {
        // The negative-id path is shared by every id-taking keyword.
        assert_eq!(
            parse_str("helper(-1), x").unwrap(),
            redirected(Redirect::Helper(-1), ident("x"))
        );
        assert_eq!(
            parse_str("target(-2), x").unwrap(),
            redirected(Redirect::Target(Some(-2)), ident("x"))
        );
        assert_eq!(
            parse_str("playerid(-7), x").unwrap(),
            redirected(Redirect::PlayerId(-7), ident("x"))
        );
    }

    // ---- AC1: redirection via the parse(&[Token]) entry point ----

    #[test]
    fn redirect_parses_through_token_slice_entry_point() {
        // The CNS layer holds tokens and calls `parse(&[Token])` directly; a
        // redirect must be recognized on that path too, not only via parse_str.
        let toks = tokenize("root, var(0)");
        assert_eq!(
            parse(&toks).unwrap(),
            redirected(Redirect::Root, call("var", vec![int(0)]))
        );
    }

    // ---- AC2: whitespace tolerance around keyword / (id) / comma ----

    #[test]
    fn redirect_tolerates_whitespace_around_id_and_comma() {
        // The lexer drops whitespace, so spacing around the keyword, the `(id)`,
        // and the comma must not change the parse.
        assert_eq!(
            parse_str("enemy , life").unwrap(),
            redirected(Redirect::Enemy, ident("life"))
        );
        assert_eq!(
            parse_str("helper ( 1 ) , stateno").unwrap(),
            redirected(Redirect::Helper(1), ident("stateno"))
        );
        assert_eq!(
            parse_str("enemy ( 2 ) , life").unwrap(),
            redirected(Redirect::EnemyNear(2), ident("life"))
        );
    }

    // ---- AC1/AC2: a non-int `(id)` is NOT a redirect id → fall through ----

    #[test]
    fn keyword_with_non_int_paren_is_not_a_redirect() {
        // `helper(x), y` — the `(x)` is not an integer literal, so
        // `scan_redirect_id` declines and the keyword is parsed as an ordinary
        // call. With a trailing `, y` the top-level comma is then a stray token,
        // so the whole thing is a (recoverable) trailing-token error — NOT a
        // malformed-redirect, and never a panic.
        let err = parse_str("helper(x), y").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
        // Used as a plain value (no comma) it parses as a normal call.
        assert_eq!(
            parse_str("helper(x)").unwrap(),
            call("helper", vec![ident("x")])
        );
        // An empty `()` after a keyword is likewise not a redirect id.
        assert_eq!(parse_str("helper()").unwrap(), call("helper", vec![]));
    }

    #[test]
    fn keyword_with_expression_id_is_not_a_redirect() {
        // `helper(1+1), x` — the `(id)` must be a bare int literal; an arithmetic
        // expression inside the parens declines the redirect scan (scan_redirect_id
        // requires `(int)` / `(-int)` exactly), so this is a plain call followed by
        // a stray comma → recoverable error.
        let err = parse_str("helper(1 + 1), x").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    // ---- AC1: a redirect nested inside a parenthesized group ----

    #[test]
    fn redirect_inside_paren_group_parses() {
        // A paren group recurses through parse_expr(0), so a redirect is allowed
        // inside it: `(enemy, life) + 1` redirects only `life`, then adds 1 in the
        // OUTER (self) context.
        assert_eq!(
            parse_str("(enemy, life) + 1").unwrap(),
            bin(
                BinaryOp::Add,
                redirected(Redirect::Enemy, ident("life")),
                int(1),
            )
        );
        // And a bare parenthesized redirect is just the redirect itself.
        assert_eq!(
            parse_str("(root, stateno)").unwrap(),
            redirected(Redirect::Root, ident("stateno"))
        );
    }

    #[test]
    fn redirect_as_range_bound_parses() {
        // Range bounds are parse_expr(0), so a redirect can appear as a bound. This
        // is exotic but must parse (and not be mistaken for the range's comma).
        assert_eq!(
            parse_str("x = [enemy, life, 100]").unwrap(),
            bin(
                BinaryOp::Eq,
                ident("x"),
                range(
                    Bound::Inclusive,
                    redirected(Redirect::Enemy, ident("life")),
                    int(100),
                    Bound::Inclusive,
                ),
            )
        );
    }

    // ---- AC2: deeper redirect nesting (three hops) ----

    #[test]
    fn triple_nested_redirect_chain_parses() {
        // `root, parent, enemy, life` nests right-associatively, three redirects
        // deep, with the trailing trigger at the bottom.
        assert_eq!(
            parse_str("root, parent, enemy, life").unwrap(),
            redirected(
                Redirect::Root,
                redirected(
                    Redirect::Parent,
                    redirected(Redirect::Enemy, ident("life")),
                ),
            )
        );
    }

    #[test]
    fn redirect_keyword_as_trailing_trigger_name_stays_ident() {
        // When a redirect keyword name appears as the *trailing* sub-expression
        // (no comma after it), it is an ordinary ident, not a second redirect:
        // `parent, root` → Redirected(Parent, Ident("root")).
        assert_eq!(
            parse_str("parent, root").unwrap(),
            redirected(Redirect::Parent, ident("root"))
        );
    }

    // ---- AC4: malformed redirects — more shapes, all recoverable ----

    #[test]
    fn malformed_redirect_unclosed_id_then_comma_is_recoverable() {
        // `helper(1, x` — the `(` opens but never closes before the comma, so the
        // id scan declines; `helper` then becomes a call whose arg list is
        // unterminated → a recoverable delimiter/EOF error, never a panic.
        let err = parse_str("helper(1, x").unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::ExpectedDelimiter { .. } | ParseError::UnexpectedEof { .. }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn malformed_redirect_double_comma_is_recoverable() {
        // `enemy,,x` — the redirect commits on the first comma, then the sub-expr
        // parse immediately hits a second comma (not an atom) → recoverable error.
        let err = parse_str("enemy,,x").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn malformed_redirect_carries_keyword_column() {
        // The MalformedRedirect error column points at the offending keyword, for
        // the CNS layer's diagnostic warn!. `   playerid, x` puts the keyword at
        // column 3.
        match parse_str("   playerid, x").unwrap_err() {
            ParseError::MalformedRedirect { column, reason } => {
                assert_eq!(column, 3, "column should point at the keyword");
                assert!(!reason.is_empty(), "reason should be populated");
            }
            other => panic!("expected MalformedRedirect, got {other:?}"),
        }
    }

    #[test]
    fn malformed_redirect_error_display_is_formattable() {
        // The MalformedRedirect Display (thiserror) must render without panic and
        // mention the column + reason (the CNS layer logs it).
        let e = ParseError::MalformedRedirect {
            reason: "redirection `,` has no following expression".into(),
            column: 5,
        };
        let s = e.to_string();
        assert!(s.contains('5') && s.contains("redirection"), "{s}");
    }

    // ---- AC1: Expr::Redirected derive surface (Debug / Clone / PartialEq) ----

    #[test]
    fn redirected_expr_derives_clone_eq_debug() {
        let a = redirected(Redirect::Helper(1), ident("x"));
        // Clone + PartialEq.
        assert_eq!(a.clone(), a);
        // A different target is unequal.
        assert_ne!(a, redirected(Redirect::Helper(2), ident("x")));
        // A different sub-expr is unequal.
        assert_ne!(a, redirected(Redirect::Helper(1), ident("y")));
        // Debug is non-empty and names the variant.
        let dbg = format!("{a:?}");
        assert!(dbg.contains("Redirected"), "{dbg}");
    }

    // =====================================================================
    // Proctor (task 4.10) — additional parser edge cases & error paths for
    // the four real-content gaps. These complement Forge's happy-path parse
    // tests with the boundary, negative, and error-recovery cases.
    // =====================================================================

    fn str_lit(s: &str) -> Expr {
        Expr::Str(s.into())
    }

    // ---- Gap 1: axis suffix — boundaries & non-folding cases ----

    #[test]
    fn axis_suffix_does_not_fold_after_a_call() {
        // The axis fold only applies to a bare `Ident` followed by an axis word.
        // A *call* `var(0)` followed by `Y` is two atoms, not an axis form — it
        // must stay a (recoverable) trailing-token error, never silently fold.
        let err = parse_str("var(0) Y").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn axis_suffix_does_not_consume_a_following_paren_call() {
        // `Vel Y(0)` — `Vel` folds with axis `Y`, but the trailing `(0)` is then a
        // leftover token (the folded Call is complete). Recoverable error, not a
        // panic, and not a silent two-arg call.
        let err = parse_str("Vel Y(0)").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn axis_word_alone_is_a_bare_ident_not_an_axis() {
        // A standalone axis letter is just a trigger/ident; folding requires a
        // *preceding* trigger name.
        assert_eq!(parse_str("X").unwrap(), ident("X"));
        assert_eq!(parse_str("Y").unwrap(), ident("Y"));
        assert_eq!(parse_str("Z").unwrap(), ident("Z"));
    }

    #[test]
    fn axis_suffix_only_consumes_one_axis_word() {
        // `Pos X Y` — `Pos` folds with the first axis word `X`; the second `Y` is a
        // leftover trailing token (recoverable error). Confirms the fold is
        // single-shot, not greedy.
        let err = parse_str("Pos X Y").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn axis_suffix_lowercase_trigger_name_still_folds() {
        // Real common1.cns writes the trigger name in lower case too (`vel x`).
        // Case is preserved on the NAME (matching is the evaluator's job) but the
        // axis is normalized to upper-case.
        assert_eq!(parse_str("vel x").unwrap(), call("vel", vec![str_lit("X")]));
        assert_eq!(parse_str("pos y").unwrap(), call("pos", vec![str_lit("Y")]));
    }

    #[test]
    fn axis_suffix_composes_with_arithmetic_and_unary() {
        // The folded call is an ordinary atom: it participates in arithmetic and
        // under a unary prefix exactly like any other primary.
        assert_eq!(
            parse_str("Pos Y + 10").unwrap(),
            bin(BinaryOp::Add, call("Pos", vec![str_lit("Y")]), int(10))
        );
        assert_eq!(
            parse_str("-Vel Y").unwrap(),
            Expr::Unary {
                op: UnaryOp::Neg,
                operand: Box::new(call("Vel", vec![str_lit("Y")])),
            }
        );
    }

    // ---- Gap 2: AnimElem comma tail — error paths & family coverage ----

    #[test]
    fn animelem_tail_missing_operand_after_comma_is_eof_error() {
        // `AnimElem = 2,` — the comma matched the family shape and was consumed,
        // but nothing follows. This must be a recoverable EOF error, not a panic.
        let err = parse_str("AnimElem = 2,").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedEof { .. }), "{err:?}");
    }

    #[test]
    fn animelem_tail_double_comma_is_recoverable_error() {
        // `AnimElem = 2,,` — comma consumed, then a stray comma where an operand
        // is required. Recoverable token error, never a panic.
        let err = parse_str("AnimElem = 2,,").unwrap_err();
        assert!(
            matches!(
                err,
                ParseError::UnexpectedToken { .. } | ParseError::UnexpectedEof { .. }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn non_family_trigger_with_comma_tail_does_not_fold() {
        // The comma tail only folds for the AnimElem family. `Time = 2, >= 0` is
        // NOT a family trigger, so the comma is left unconsumed and surfaces as a
        // trailing-token error — it must NOT be silently swallowed into an
        // AnimElemTail.
        let err = parse_str("Time = 2, >= 0").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn animelem_tail_requires_equality_head_not_other_comparison() {
        // The fold only fires when the head is an *equality* (`= N`). A `>=` head
        // is a different comparison; the comma after it is a stray trailing token,
        // not a tail. `AnimElem >= 2, 0` must error, not fold.
        let err = parse_str("AnimElem >= 2, 0").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedToken { .. }), "{err:?}");
    }

    #[test]
    fn animelem_tail_all_family_members_parse() {
        // Every family member named in `is_animelem_family` accepts the tail.
        for name in ["AnimElem", "AnimElemTime", "AnimElemNo", "TimeMod"] {
            let src = format!("{name} = 2, >= 0");
            let ast = parse_str(&src).unwrap_or_else(|e| panic!("{src:?}: {e}"));
            match ast {
                Expr::AnimElemTail {
                    name: n, op, ..
                } => {
                    assert!(n.eq_ignore_ascii_case(name), "name preserved: {n}");
                    assert_eq!(op, BinaryOp::Ge);
                }
                other => panic!("{src:?} should fold to AnimElemTail, got {other:?}"),
            }
        }
    }

    #[test]
    fn animelem_tail_omitted_operator_defaults_to_eq() {
        // `AnimElem = 2, 1` — no operator in the tail means `=`.
        match parse_str("AnimElem = 2, 1").unwrap() {
            Expr::AnimElemTail { op, element, operand, .. } => {
                assert_eq!(op, BinaryOp::Eq);
                assert_eq!(*element, int(2));
                assert_eq!(*operand, int(1));
            }
            other => panic!("expected AnimElemTail, got {other:?}"),
        }
    }

    #[test]
    fn animelem_tail_real_kfm_negative_operand() {
        // Verbatim kfm.cns line 1703: `AnimElem = 3, -1` — operand is a negated
        // literal; the omitted op defaults to `=`.
        match parse_str("AnimElem = 3, -1").unwrap() {
            Expr::AnimElemTail { op, element, operand, .. } => {
                assert_eq!(op, BinaryOp::Eq);
                assert_eq!(*element, int(3));
                assert_eq!(
                    *operand,
                    Expr::Unary { op: UnaryOp::Neg, operand: Box::new(int(1)) }
                );
            }
            other => panic!("expected AnimElemTail, got {other:?}"),
        }
    }

    #[test]
    fn animelem_tail_fold_is_top_level_only_documented_limitation() {
        // KNOWN LIMITATION (reported to Forge): the `= N, op M` fold is attempted
        // ONLY at the very top of the parse, when the comma is the immediate next
        // token after the full expression AND that full expression is exactly a
        // `<family> = N` equality. Consequences, all pinned here so a future change
        // is a conscious one:
        //
        //  (a) A standalone line folds correctly (the real-content shape — all four
        //      kfm.cns occurrences are standalone `trigger1 = AnimElem = N, ...`).
        assert!(matches!(
            parse_str("AnimElem = 2, >= 0"),
            Ok(Expr::AnimElemTail { .. })
        ));
        //  (b) A *parenthesized* tail does NOT fold: inside `(...)` the comma is
        //      read as a range separator and the `op` after it is a stray token.
        //      Recoverable error, never a panic.
        assert!(matches!(
            parse_str("(AnimElem = 2, >= 0)"),
            Err(ParseError::UnexpectedToken { .. })
        ));
        //  (c) When the family equality is buried inside a larger boolean on its
        //      LEFT, the comma is stranded and surfaces as a trailing-token error.
        assert!(matches!(
            parse_str("Time > 0 && AnimElem = 2, >= 0"),
            Err(ParseError::UnexpectedToken { .. })
        ));
        //  (d) When it is on the LEFT of the line, the tail operand greedily
        //      absorbs the trailing boolean (`>= 0 && Time > 0` becomes the
        //      operand). It still parses to an AnimElemTail (no panic), but the
        //      operand is the whole `0 && ...` — a mis-binding to be aware of.
        match parse_str("AnimElem = 2, >= 0 && Time > 0").unwrap() {
            Expr::AnimElemTail { operand, .. } => {
                assert!(
                    matches!(*operand, Expr::Binary { op: BinaryOp::And, .. }),
                    "operand greedily absorbs the && tail: {operand:?}"
                );
            }
            other => panic!("expected AnimElemTail, got {other:?}"),
        }
    }

    // ---- Gap 3: dotted member args — multi-dot, composition ----

    #[test]
    fn dotted_multi_segment_member_arg_parses() {
        // `GetHitVar(fall.envshake.time)` — a three-segment dotted member is one
        // ident argument.
        assert_eq!(
            parse_str("GetHitVar(fall.envshake.time)").unwrap(),
            call("GetHitVar", vec![ident("fall.envshake.time")])
        );
    }

    #[test]
    fn dotted_member_arg_through_redirect_parses() {
        // `enemy, GetHitVar(fall.yvel)` — dotted arg survives inside a redirect.
        assert_eq!(
            parse_str("enemy, GetHitVar(fall.yvel)").unwrap(),
            redirected(Redirect::Enemy, call("GetHitVar", vec![ident("fall.yvel")]))
        );
    }

    #[test]
    fn dotted_member_arg_with_arithmetic_parses() {
        // `GetHitVar(fall.yvel) + 1` — the dotted-arg call composes in arithmetic.
        assert_eq!(
            parse_str("GetHitVar(fall.yvel) + 1").unwrap(),
            bin(BinaryOp::Add, call("GetHitVar", vec![ident("fall.yvel")]), int(1))
        );
    }

    // ---- Gap 4: command = "string" — parse shape & chained forms ----

    #[test]
    fn command_string_equality_parses_in_both_orders() {
        // The parser is order-agnostic; the evaluator recognizes either operand
        // order. Confirm both parse to the obvious Binary shape.
        assert_eq!(
            parse_str("command = \"x\"").unwrap(),
            bin(BinaryOp::Eq, ident("command"), str_lit("x"))
        );
        assert_eq!(
            parse_str("\"x\" = command").unwrap(),
            bin(BinaryOp::Eq, str_lit("x"), ident("command"))
        );
        assert_eq!(
            parse_str("command != \"x\"").unwrap(),
            bin(BinaryOp::Ne, ident("command"), str_lit("x"))
        );
    }

    #[test]
    fn command_string_or_chain_parses() {
        // Real kfm.cns shape: `Command = "a" || Command = "b"`.
        assert_eq!(
            parse_str("Command = \"a\" || Command = \"b\"").unwrap(),
            bin(
                BinaryOp::Or,
                bin(BinaryOp::Eq, ident("Command"), str_lit("a")),
                bin(BinaryOp::Eq, ident("Command"), str_lit("b")),
            )
        );
    }

    #[test]
    fn chained_var_eq_command_eq_string_is_left_associative() {
        // Real kfm.cns shape `var(2) = command = "holdfwd"` (a VarSet value, not a
        // trigger condition). `=` is left-associative, so this parses as
        // `(var(2) = command) = "holdfwd"` — the OUTER lhs is a Binary, NOT a bare
        // `command` ident, which is why the evaluator does NOT route it through the
        // command seam (documented in the evaluator tests). Pin the parse shape.
        assert_eq!(
            parse_str("var(2) = command = \"holdfwd\"").unwrap(),
            bin(
                BinaryOp::Eq,
                bin(BinaryOp::Eq, call("var", vec![int(2)]), ident("command")),
                str_lit("holdfwd"),
            )
        );
    }

    #[test]
    fn empty_string_literal_compare_parses() {
        // A degenerate but well-formed `command = ""` must parse (the lexer yields
        // an empty Str), never panic.
        assert_eq!(
            parse_str("command = \"\"").unwrap(),
            bin(BinaryOp::Eq, ident("command"), str_lit(""))
        );
    }
}
