//! # Expression lexer
//!
//! Tokenizes MUGEN trigger expressions into a flat [`Vec`] of [`Token`]s for the
//! parser (task 4.2) to consume. The lexer covers the full
//! MUGEN expression grammar: integer and floating-point literals, identifiers,
//! the relational / logical / arithmetic / bitwise operators, range delimiters,
//! commas, the redirection/assignment operator `:=`, double-quoted strings, and
//! `;`-introduced line comments.
//!
//! ## Tolerance
//!
//! Following the engine-wide "never crash on bad content" rule (see `CLAUDE.md`),
//! the lexer never panics. An unrecognized character is logged at debug level and
//! emitted as a [`TokenKind::Unknown`] token so the parser can decide how to
//! recover, rather than aborting the whole tokenization. (Unknown characters log
//! at `debug!`, not `warn!`, to avoid log floods when scanning the thousands of
//! malformed community triggers the engine must tolerate.)
//!
//! ## Grammar notes specific to MUGEN
//!
//! - Comments start with `;` and run to the end of the line (CNS/INI style), not
//!   `//` or `#`.
//! - `**` is the exponent operator and must be matched before `*`.
//! - `:=` is the variable-assignment operator used in expressions such as
//!   `var(0) := 5`.
//! - Both `=` and `==` are accepted for equality; MUGEN historically uses a
//!   single `=` inside triggers.
//! - Numbers may be written with a leading decimal point (e.g. `.5`).
//!
//! # Example
//!
//! ```
//! use fp_vm::lexer::{tokenize, TokenKind};
//!
//! let tokens = tokenize("AnimElem = 2 && Time >= 30");
//! assert_eq!(tokens.first().map(|t| &t.kind), Some(&TokenKind::Ident("AnimElem".into())));
//! ```

use std::fmt;

/// The category and payload of a single lexical token.
///
/// Each variant corresponds to one terminal in the MUGEN expression grammar.
/// Multi-character operators (`**`, `<=`, `:=`, `&&`, …) are single variants so
/// the parser never has to re-combine characters.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// An integer literal, e.g. `42`. Stored as `i64` to avoid overflow during
    /// lexing; the evaluator narrows to MUGEN's 32-bit semantics later.
    Int(i64),
    /// A floating-point literal, e.g. `3.14` or `.5`.
    Float(f64),
    /// A double-quoted string literal with surrounding quotes removed, e.g. the
    /// `"x"` in `Command = "x"`.
    Str(String),
    /// An identifier: a trigger name, function name, or keyword such as
    /// `AnimElem`, `cond`, or `var`. Case is preserved as written.
    Ident(String),

    // --- Relational operators ---
    /// `=` (equality; MUGEN's primary equality operator).
    Eq,
    /// `==` (equality; accepted as an alias for `=`).
    EqEq,
    /// `!=` (inequality).
    NotEq,
    /// `<` (less than).
    Lt,
    /// `<=` (less than or equal).
    Le,
    /// `>` (greater than).
    Gt,
    /// `>=` (greater than or equal).
    Ge,

    // --- Logical operators ---
    /// `&&` (logical AND).
    AndAnd,
    /// `||` (logical OR).
    OrOr,
    /// `!` (logical NOT).
    Not,

    // --- Bitwise operators ---
    /// `&` (bitwise AND).
    Amp,
    /// `|` (bitwise OR).
    Pipe,
    /// `^` (bitwise XOR).
    Caret,
    /// `~` (bitwise NOT).
    Tilde,

    // --- Arithmetic operators ---
    /// `+` (addition / unary plus).
    Plus,
    /// `-` (subtraction / unary minus).
    Minus,
    /// `*` (multiplication).
    Star,
    /// `**` (exponentiation).
    StarStar,
    /// `/` (division).
    Slash,
    /// `%` (modulo).
    Percent,

    // --- Assignment / redirection ---
    /// `:=` (variable assignment, e.g. `var(0) := 1`).
    Assign,

    // --- Delimiters ---
    /// `(` — also an exclusive range bound.
    LParen,
    /// `)` — also an exclusive range bound.
    RParen,
    /// `[` — also an inclusive range bound.
    LBracket,
    /// `]` — also an inclusive range bound.
    RBracket,
    /// `,` (argument / range separator).
    Comma,

    /// A character the lexer did not recognize. The payload is the offending
    /// character so the parser can report it. Never produced for valid MUGEN
    /// expressions; exists purely so the lexer can stay panic-free.
    Unknown(char),
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Int(n) => write!(f, "{n}"),
            TokenKind::Float(n) => write!(f, "{n}"),
            TokenKind::Str(s) => write!(f, "\"{s}\""),
            TokenKind::Ident(s) => write!(f, "{s}"),
            TokenKind::Eq => write!(f, "="),
            TokenKind::EqEq => write!(f, "=="),
            TokenKind::NotEq => write!(f, "!="),
            TokenKind::Lt => write!(f, "<"),
            TokenKind::Le => write!(f, "<="),
            TokenKind::Gt => write!(f, ">"),
            TokenKind::Ge => write!(f, ">="),
            TokenKind::AndAnd => write!(f, "&&"),
            TokenKind::OrOr => write!(f, "||"),
            TokenKind::Not => write!(f, "!"),
            TokenKind::Amp => write!(f, "&"),
            TokenKind::Pipe => write!(f, "|"),
            TokenKind::Caret => write!(f, "^"),
            TokenKind::Tilde => write!(f, "~"),
            TokenKind::Plus => write!(f, "+"),
            TokenKind::Minus => write!(f, "-"),
            TokenKind::Star => write!(f, "*"),
            TokenKind::StarStar => write!(f, "**"),
            TokenKind::Slash => write!(f, "/"),
            TokenKind::Percent => write!(f, "%"),
            TokenKind::Assign => write!(f, ":="),
            TokenKind::LParen => write!(f, "("),
            TokenKind::RParen => write!(f, ")"),
            TokenKind::LBracket => write!(f, "["),
            TokenKind::RBracket => write!(f, "]"),
            TokenKind::Comma => write!(f, ","),
            TokenKind::Unknown(c) => write!(f, "{c}"),
        }
    }
}

/// A token paired with its source location.
///
/// `column` is a 0-based index into the **`char`** sequence of the input (not a
/// byte offset), counted from the start of the expression. It is intended for
/// human-readable diagnostics, not for slicing the source string.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// What kind of token this is, plus any literal payload.
    pub kind: TokenKind,
    /// 0-based character offset of the token's first character.
    pub column: usize,
}

impl Token {
    /// Creates a token at the given character column.
    pub fn new(kind: TokenKind, column: usize) -> Self {
        Self { kind, column }
    }
}

/// Tokenizes a MUGEN expression into a vector of [`Token`]s.
///
/// This function is **infallible and panic-free**: malformed input never causes
/// it to fail. Unrecognized characters become [`TokenKind::Unknown`] tokens
/// (and emit a [`tracing::warn!`]); an unterminated string literal is closed at
/// end-of-input with a warning. The returned vector does not include comments or
/// whitespace and does not carry an explicit end-of-input marker — an empty
/// input (or input that is only whitespace/comments) yields an empty vector.
///
/// # Example
///
/// ```
/// use fp_vm::lexer::{tokenize, TokenKind};
///
/// let tokens = tokenize("1 + 2 ; trailing comment");
/// let kinds: Vec<_> = tokens.into_iter().map(|t| t.kind).collect();
/// assert_eq!(kinds, vec![TokenKind::Int(1), TokenKind::Plus, TokenKind::Int(2)]);
/// ```
pub fn tokenize(input: &str) -> Vec<Token> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        // Whitespace: skip.
        if c.is_whitespace() {
            i += 1;
            continue;
        }

        // Comment: `;` runs to end of line.
        if c == ';' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        let start = i;

        // String literal.
        if c == '"' {
            let (kind, next) = lex_string(&chars, i);
            tokens.push(Token::new(kind, start));
            i = next;
            continue;
        }

        // Number: a digit, or a `.` immediately followed by a digit (e.g. `.5`).
        if c.is_ascii_digit() || (c == '.' && peek_is_digit(&chars, i + 1)) {
            let (kind, next) = lex_number(&chars, i);
            tokens.push(Token::new(kind, start));
            i = next;
            continue;
        }

        // Identifier / keyword: starts with a letter or underscore.
        if c.is_alphabetic() || c == '_' {
            let (kind, next) = lex_ident(&chars, i);
            tokens.push(Token::new(kind, start));
            i = next;
            continue;
        }

        // Operators and delimiters. Two-character operators are matched first.
        if let Some((kind, consumed)) = lex_operator(&chars, i) {
            tokens.push(Token::new(kind, start));
            i += consumed;
            continue;
        }

        // Anything else is unrecognized; stay tolerant. Logged at debug (not warn)
        // to avoid flooding logs when scanning large amounts of malformed content.
        tracing::debug!(column = i, character = %c, "lexer: skipping unknown character");
        tokens.push(Token::new(TokenKind::Unknown(c), start));
        i += 1;
    }

    tokens
}

/// Returns whether the character at `idx` exists and is an ASCII digit.
fn peek_is_digit(chars: &[char], idx: usize) -> bool {
    chars.get(idx).is_some_and(|c| c.is_ascii_digit())
}

/// Lexes a double-quoted string starting at `start` (which must point at `"`).
///
/// Returns the [`TokenKind::Str`] (without surrounding quotes) and the index just
/// past the closing quote. An unterminated string is closed at end-of-input and
/// a warning is logged. A backslash escapes the next character (so `\"` is a
/// literal quote inside the string).
fn lex_string(chars: &[char], start: usize) -> (TokenKind, usize) {
    let mut i = start + 1; // skip opening quote
    let mut value = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            // Escape: take the next character literally.
            value.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '"' {
            return (TokenKind::Str(value), i + 1);
        }
        value.push(c);
        i += 1;
    }
    tracing::warn!(column = start, "lexer: unterminated string literal");
    (TokenKind::Str(value), i)
}

/// Lexes a numeric literal starting at `start`.
///
/// Produces a [`TokenKind::Float`] if the literal contains a decimal point or an
/// exponent (`e`/`E`), otherwise a [`TokenKind::Int`]. If the digit sequence
/// cannot be parsed (e.g. integer overflow), the lexer falls back to a value of
/// `0` and logs a warning rather than failing. This `0` fallback is a lexer-level
/// safety net only; note it can flip the meaning of a comparison against an
/// out-of-range literal (e.g. `Life < 99999999999`), so whether to saturate
/// instead is tracked as backlog item CB4, alongside the evaluator's 32-bit
/// narrowing (task 4.4).
fn lex_number(chars: &[char], start: usize) -> (TokenKind, usize) {
    let mut i = start;
    let mut is_float = false;

    // Integer part.
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }

    // Fractional part: a single `.` followed by digits.
    if i < chars.len() && chars[i] == '.' && peek_is_digit(chars, i + 1) {
        is_float = true;
        i += 1; // consume '.'
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
    } else if i < chars.len() && chars[i] == '.' {
        // A trailing dot with no following digit (e.g. `5.`): treat as float.
        is_float = true;
        i += 1;
    }

    // Exponent part: e[+/-]digits.
    if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
        let mut j = i + 1;
        if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
            j += 1;
        }
        if j < chars.len() && chars[j].is_ascii_digit() {
            is_float = true;
            i = j;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
        }
    }

    let text: String = chars[start..i].iter().collect();
    if is_float {
        match text.parse::<f64>() {
            Ok(n) => (TokenKind::Float(n), i),
            Err(_) => {
                tracing::warn!(column = start, text = %text, "lexer: invalid float literal, using 0.0");
                (TokenKind::Float(0.0), i)
            }
        }
    } else {
        match text.parse::<i64>() {
            Ok(n) => (TokenKind::Int(n), i),
            Err(_) => {
                tracing::warn!(column = start, text = %text, "lexer: invalid integer literal, using 0");
                (TokenKind::Int(0), i)
            }
        }
    }
}

/// Lexes an identifier or keyword starting at `start`.
///
/// Identifiers begin with a letter or `_` and continue with letters, digits, or
/// `_`. The original casing is preserved; case-insensitive matching (MUGEN
/// trigger names are case-insensitive) is left to the parser/evaluator.
///
/// ## Dotted member names (task 4.10, gap 3)
///
/// MUGEN's `GetHitVar` accessor uses *dotted* member keys for some of its hit
/// variables — `GetHitVar(fall.yvel)`, `GetHitVar(fall.envshake.time)`. To let
/// these flow through as a single argument key, a `.` that immediately follows
/// an identifier **and** is immediately followed by another identifier character
/// (letter or `_`) is folded into the same `Ident` token: `fall.yvel` lexes as
/// one `Ident("fall.yvel")`, not `Ident("fall")` `.` `Ident("yvel")`. The dot is
/// only consumed when it is sandwiched between identifier characters, so a lone
/// trailing dot, or a dot before a digit, is left alone — a numeric literal such
/// as `5.x` is still lexed by [`lex_number`] (the leading digit routes there
/// first) and a bare `.` remains [`TokenKind::Unknown`].
fn lex_ident(chars: &[char], start: usize) -> (TokenKind, usize) {
    let mut i = start;
    loop {
        // Run of normal identifier characters.
        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        // A dotted member continues the identifier only when the dot is
        // immediately followed by another identifier-start character. This is
        // what makes `fall.yvel` one token while leaving `fall.` (trailing dot)
        // and `fall.5` (dot-then-digit) split for the parser to handle.
        if i < chars.len() && chars[i] == '.' {
            if let Some(next) = chars.get(i + 1) {
                if next.is_alphabetic() || *next == '_' {
                    i += 1; // consume the '.'
                    continue;
                }
            }
        }
        break;
    }
    let text: String = chars[start..i].iter().collect();
    (TokenKind::Ident(text), i)
}

/// Lexes an operator or delimiter starting at `start`.
///
/// Returns the token and the number of characters consumed (1 or 2), or `None`
/// if the character at `start` does not begin a known operator. Two-character
/// operators (`**`, `<=`, `>=`, `!=`, `==`, `&&`, `||`, `:=`) are matched before
/// their single-character prefixes.
fn lex_operator(chars: &[char], start: usize) -> Option<(TokenKind, usize)> {
    let c = chars[start];
    let next = chars.get(start + 1).copied();

    let two = |kind| Some((kind, 2));
    let one = |kind| Some((kind, 1));

    match c {
        '=' => match next {
            Some('=') => two(TokenKind::EqEq),
            _ => one(TokenKind::Eq),
        },
        '!' => match next {
            Some('=') => two(TokenKind::NotEq),
            _ => one(TokenKind::Not),
        },
        '<' => match next {
            Some('=') => two(TokenKind::Le),
            _ => one(TokenKind::Lt),
        },
        '>' => match next {
            Some('=') => two(TokenKind::Ge),
            _ => one(TokenKind::Gt),
        },
        '&' => match next {
            Some('&') => two(TokenKind::AndAnd),
            _ => one(TokenKind::Amp),
        },
        '|' => match next {
            Some('|') => two(TokenKind::OrOr),
            _ => one(TokenKind::Pipe),
        },
        '*' => match next {
            Some('*') => two(TokenKind::StarStar),
            _ => one(TokenKind::Star),
        },
        ':' => match next {
            Some('=') => two(TokenKind::Assign),
            // A lone ':' is not part of the grammar; let the caller report it.
            _ => None,
        },
        '^' => one(TokenKind::Caret),
        '~' => one(TokenKind::Tilde),
        '+' => one(TokenKind::Plus),
        '-' => one(TokenKind::Minus),
        '/' => one(TokenKind::Slash),
        '%' => one(TokenKind::Percent),
        '(' => one(TokenKind::LParen),
        ')' => one(TokenKind::RParen),
        '[' => one(TokenKind::LBracket),
        ']' => one(TokenKind::RBracket),
        ',' => one(TokenKind::Comma),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: tokenize and return just the kinds.
    fn kinds(input: &str) -> Vec<TokenKind> {
        tokenize(input).into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn empty_and_whitespace_only() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   \t\n  ").is_empty());
    }

    #[test]
    // `3.14` is a deliberate decimal literal under test, not a use of PI.
    #[allow(clippy::approx_constant)]
    fn integer_and_float_literals() {
        assert_eq!(kinds("42"), vec![TokenKind::Int(42)]);
        assert_eq!(kinds("3.14"), vec![TokenKind::Float(3.14)]);
        assert_eq!(kinds(".5"), vec![TokenKind::Float(0.5)]);
        assert_eq!(kinds("1e3"), vec![TokenKind::Float(1000.0)]);
        assert_eq!(kinds("2.5e-1"), vec![TokenKind::Float(0.25)]);
    }

    #[test]
    fn leading_dot_is_float_not_member_access() {
        // `.5` is a float; a bare `.` not followed by a digit is unknown.
        assert_eq!(kinds(".5"), vec![TokenKind::Float(0.5)]);
        assert_eq!(kinds("."), vec![TokenKind::Unknown('.')]);
    }

    #[test]
    fn identifiers_preserve_case() {
        assert_eq!(kinds("AnimElem"), vec![TokenKind::Ident("AnimElem".into())]);
        assert_eq!(kinds("var"), vec![TokenKind::Ident("var".into())]);
        assert_eq!(kinds("_x1"), vec![TokenKind::Ident("_x1".into())]);
    }

    #[test]
    fn all_relational_operators() {
        assert_eq!(
            kinds("= == != < <= > >="),
            vec![
                TokenKind::Eq,
                TokenKind::EqEq,
                TokenKind::NotEq,
                TokenKind::Lt,
                TokenKind::Le,
                TokenKind::Gt,
                TokenKind::Ge,
            ]
        );
    }

    #[test]
    fn logical_and_bitwise_operators() {
        assert_eq!(
            kinds("&& || ! & | ^ ~"),
            vec![
                TokenKind::AndAnd,
                TokenKind::OrOr,
                TokenKind::Not,
                TokenKind::Amp,
                TokenKind::Pipe,
                TokenKind::Caret,
                TokenKind::Tilde,
            ]
        );
    }

    #[test]
    fn arithmetic_operators_and_exponent() {
        assert_eq!(
            kinds("+ - * / % **"),
            vec![
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Percent,
                TokenKind::StarStar,
            ]
        );
    }

    #[test]
    fn double_star_beats_single_star() {
        // `2**3` must lex as Int Exp Int, not Int Mul Mul Int.
        assert_eq!(
            kinds("2**3"),
            vec![TokenKind::Int(2), TokenKind::StarStar, TokenKind::Int(3)]
        );
        assert_eq!(
            kinds("2*3"),
            vec![TokenKind::Int(2), TokenKind::Star, TokenKind::Int(3)]
        );
    }

    #[test]
    fn assignment_operator() {
        assert_eq!(
            kinds("var(0):=5"),
            vec![
                TokenKind::Ident("var".into()),
                TokenKind::LParen,
                TokenKind::Int(0),
                TokenKind::RParen,
                TokenKind::Assign,
                TokenKind::Int(5),
            ]
        );
    }

    #[test]
    fn range_delimiters_and_commas() {
        assert_eq!(
            kinds("[1,2] (3,4)"),
            vec![
                TokenKind::LBracket,
                TokenKind::Int(1),
                TokenKind::Comma,
                TokenKind::Int(2),
                TokenKind::RBracket,
                TokenKind::LParen,
                TokenKind::Int(3),
                TokenKind::Comma,
                TokenKind::Int(4),
                TokenKind::RParen,
            ]
        );
    }

    #[test]
    fn string_literal() {
        assert_eq!(
            kinds(r#"Command = "QCF_x""#),
            vec![
                TokenKind::Ident("Command".into()),
                TokenKind::Eq,
                TokenKind::Str("QCF_x".into()),
            ]
        );
    }

    #[test]
    fn string_with_escape_and_unterminated() {
        assert_eq!(kinds(r#""a\"b""#), vec![TokenKind::Str("a\"b".into())]);
        // Unterminated string is recovered, not a panic.
        assert_eq!(kinds("\"abc"), vec![TokenKind::Str("abc".into())]);
    }

    #[test]
    fn semicolon_comment_is_ignored() {
        assert_eq!(
            kinds("1 + 2 ; this is a comment"),
            vec![TokenKind::Int(1), TokenKind::Plus, TokenKind::Int(2)]
        );
        // Comment runs only to end of line.
        assert_eq!(
            kinds("1 ; c\n+ 2"),
            vec![TokenKind::Int(1), TokenKind::Plus, TokenKind::Int(2)]
        );
    }

    #[test]
    fn unknown_character_is_tolerated() {
        // `@` and a lone `:` are not in the grammar; emit Unknown, do not panic.
        let ks = kinds("1 @ 2");
        assert_eq!(
            ks,
            vec![
                TokenKind::Int(1),
                TokenKind::Unknown('@'),
                TokenKind::Int(2)
            ]
        );
        assert_eq!(kinds(":"), vec![TokenKind::Unknown(':')]);
    }

    #[test]
    fn realistic_trigger_expression() {
        // A representative trigger seen in CNS files.
        let ks = kinds("AnimElem = 2 && Time >= 30 && Pos Y < 0");
        assert_eq!(
            ks,
            vec![
                TokenKind::Ident("AnimElem".into()),
                TokenKind::Eq,
                TokenKind::Int(2),
                TokenKind::AndAnd,
                TokenKind::Ident("Time".into()),
                TokenKind::Ge,
                TokenKind::Int(30),
                TokenKind::AndAnd,
                TokenKind::Ident("Pos".into()),
                TokenKind::Ident("Y".into()),
                TokenKind::Lt,
                TokenKind::Int(0),
            ]
        );
    }

    #[test]
    fn columns_are_tracked() {
        let toks = tokenize("ab + 1");
        assert_eq!(toks[0].column, 0); // "ab"
        assert_eq!(toks[1].column, 3); // "+"
        assert_eq!(toks[2].column, 5); // "1"
    }

    #[test]
    fn token_kind_display_roundtrip() {
        assert_eq!(TokenKind::StarStar.to_string(), "**");
        assert_eq!(TokenKind::Assign.to_string(), ":=");
        assert_eq!(TokenKind::Int(7).to_string(), "7");
        assert_eq!(TokenKind::Ident("foo".into()).to_string(), "foo");
    }

    #[test]
    fn negative_looking_input_lexes_as_minus_then_int() {
        // The lexer does not fold signs into literals; that is the parser's job.
        assert_eq!(kinds("-5"), vec![TokenKind::Minus, TokenKind::Int(5)]);
    }

    // ---------------------------------------------------------------------
    // Additional coverage: edge cases, error paths, MUGEN semantics.
    // ---------------------------------------------------------------------

    #[test]
    fn token_new_constructor_sets_fields() {
        let t = Token::new(TokenKind::Plus, 7);
        assert_eq!(t.kind, TokenKind::Plus);
        assert_eq!(t.column, 7);
    }

    #[test]
    fn exponent_without_digits_falls_back_to_ident() {
        // `1e` is NOT a valid float: the `e` is not consumed into the number,
        // so it lexes as Int(1) followed by the identifier `e`. This matters
        // because triggers like `e` could (in principle) be names; the lexer
        // must not greedily swallow a dangling exponent marker.
        assert_eq!(
            kinds("1e"),
            vec![TokenKind::Int(1), TokenKind::Ident("e".into())]
        );
        // `1e+` — exponent marker with a sign but no digits: the `e` becomes an
        // identifier and the `+` becomes a Plus operator.
        assert_eq!(
            kinds("1e+"),
            vec![
                TokenKind::Int(1),
                TokenKind::Ident("e".into()),
                TokenKind::Plus
            ]
        );
    }

    #[test]
    fn exponent_variants_are_floats() {
        assert_eq!(kinds("1e3"), vec![TokenKind::Float(1000.0)]);
        assert_eq!(kinds("1E3"), vec![TokenKind::Float(1000.0)]);
        assert_eq!(kinds("1e+5"), vec![TokenKind::Float(100_000.0)]);
        assert_eq!(kinds("2e-2"), vec![TokenKind::Float(0.02)]);
        // A float with both fraction and exponent.
        assert_eq!(kinds("1.5e2"), vec![TokenKind::Float(150.0)]);
    }

    #[test]
    fn trailing_dot_is_float() {
        // `5.` (trailing dot, no following digit) is treated as a float per the
        // lexer's documented rule, and the dot is consumed.
        assert_eq!(kinds("5."), vec![TokenKind::Float(5.0)]);
        // `5.x` — float `5.` then identifier `x`; the dot does not start a new
        // token.
        assert_eq!(
            kinds("5.x"),
            vec![TokenKind::Float(5.0), TokenKind::Ident("x".into())]
        );
    }

    #[test]
    fn number_immediately_followed_by_ident() {
        // `5x` is two tokens: the number does not absorb the trailing letter.
        assert_eq!(
            kinds("5x"),
            vec![TokenKind::Int(5), TokenKind::Ident("x".into())]
        );
    }

    #[test]
    fn double_dot_number_does_not_panic() {
        // `1.2.3` is malformed for the parser, but the lexer must stay
        // panic-free: it produces Float(1.2) then re-enters number lexing at the
        // second dot, yielding Float(0.3). The contract is "never crash", and the
        // parser rejects the adjacency later.
        assert_eq!(
            kinds("1.2.3"),
            vec![TokenKind::Float(1.2), TokenKind::Float(0.3)]
        );
    }

    #[test]
    fn integer_overflow_falls_back_to_zero() {
        // A literal too large for i64 must not panic; the lexer substitutes 0
        // (the engine-wide "bad expression -> 0" safe default).
        assert_eq!(kinds("99999999999999999999999999"), vec![TokenKind::Int(0)]);
    }

    #[test]
    fn empty_string_literal() {
        assert_eq!(kinds(r#""""#), vec![TokenKind::Str(String::new())]);
    }

    #[test]
    fn backslash_at_end_of_input_is_recovered() {
        // A trailing backslash inside an (unterminated) string has no character
        // to escape; the lexer keeps it literally and closes the string at EOI
        // rather than panicking or looping.
        assert_eq!(kinds("\"\\"), vec![TokenKind::Str("\\".into())]);
    }

    #[test]
    fn escaped_backslash_in_string() {
        // `"a\\b"` -> the escape consumes the second backslash, yielding `a\b`.
        assert_eq!(kinds(r#""a\\b""#), vec![TokenKind::Str("a\\b".into())]);
    }

    #[test]
    fn string_with_special_chars_inside() {
        // Operators, semicolons, and whitespace inside quotes are literal text,
        // not tokens or comments.
        assert_eq!(
            kinds(r#""a + b ; not a comment""#),
            vec![TokenKind::Str("a + b ; not a comment".into())]
        );
    }

    #[test]
    fn assign_then_eq_adjacent() {
        // `:==` is `:=` followed by `=`, not `:` then `==`.
        assert_eq!(kinds(":=="), vec![TokenKind::Assign, TokenKind::Eq]);
    }

    #[test]
    fn lone_colon_is_unknown() {
        // A bare `:` is not in the grammar (only `:=` is); it must surface as
        // Unknown so the parser can report it, without panicking.
        assert_eq!(kinds(":"), vec![TokenKind::Unknown(':')]);
        // `:x` — Unknown colon then identifier.
        assert_eq!(
            kinds(":x"),
            vec![TokenKind::Unknown(':'), TokenKind::Ident("x".into())]
        );
    }

    #[test]
    fn triple_operator_chars_split_greedily_two_then_one() {
        // `&&&` -> AndAnd, Amp;  `|||` -> OrOr, Pipe;  `2***3` -> Int ** * Int.
        assert_eq!(kinds("&&&"), vec![TokenKind::AndAnd, TokenKind::Amp]);
        assert_eq!(kinds("|||"), vec![TokenKind::OrOr, TokenKind::Pipe]);
        assert_eq!(
            kinds("2***3"),
            vec![
                TokenKind::Int(2),
                TokenKind::StarStar,
                TokenKind::Star,
                TokenKind::Int(3),
            ]
        );
    }

    #[test]
    fn bang_eq_versus_bang() {
        // `!=` is one token; `!!` is two Not tokens (logical double-negation).
        assert_eq!(kinds("!="), vec![TokenKind::NotEq]);
        assert_eq!(kinds("!!"), vec![TokenKind::Not, TokenKind::Not]);
    }

    #[test]
    fn unicode_identifier_and_char_column_tracking() {
        // `is_alphabetic()` accepts non-ASCII letters, so accented/Greek names
        // lex as identifiers. Crucially, `column` is a CHAR offset, not a byte
        // offset: "café" is 4 chars (5 bytes), so `=` lands at column 5, not 6.
        let toks = tokenize("café = 1");
        assert_eq!(toks[0].kind, TokenKind::Ident("café".into()));
        assert_eq!(toks[0].column, 0);
        assert_eq!(toks[1].kind, TokenKind::Eq);
        assert_eq!(toks[1].column, 5); // char offset, not byte offset (would be 6)
        assert_eq!(toks[2].kind, TokenKind::Int(1));
        assert_eq!(toks[2].column, 7);
    }

    #[test]
    fn columns_track_after_string_and_comment() {
        // Column accounting must remain correct across a string literal...
        let toks = tokenize(r#""hi" + 9"#);
        assert_eq!(toks[0].column, 0); // "hi"
        assert_eq!(toks[1].column, 5); // +
        assert_eq!(toks[2].column, 7); // 9
                                       // ...and after a comment is skipped on the same logical input across a
                                       // newline.
        let toks = tokenize("; lead comment\nfoo");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, TokenKind::Ident("foo".into()));
        assert_eq!(toks[0].column, 15); // 14 comment chars + newline
    }

    #[test]
    fn columns_track_multichar_operators() {
        // A two-char operator occupies one column (its start), and the next
        // token's column reflects the two consumed characters.
        let toks = tokenize("a>=b");
        assert_eq!(toks[0].column, 0); // a
        assert_eq!(toks[1].kind, TokenKind::Ge);
        assert_eq!(toks[1].column, 1); // >=
        assert_eq!(toks[2].column, 3); // b (after the two-char >=)
    }

    #[test]
    fn comment_only_input_is_empty() {
        assert!(tokenize("; just a comment with no code").is_empty());
        assert!(tokenize(";").is_empty());
    }

    #[test]
    fn display_covers_every_token_kind() {
        // Round-trip the textual form of each operator/delimiter so the Display
        // impl can't silently regress (it is used in parser diagnostics).
        let cases = [
            (TokenKind::Eq, "="),
            (TokenKind::EqEq, "=="),
            (TokenKind::NotEq, "!="),
            (TokenKind::Lt, "<"),
            (TokenKind::Le, "<="),
            (TokenKind::Gt, ">"),
            (TokenKind::Ge, ">="),
            (TokenKind::AndAnd, "&&"),
            (TokenKind::OrOr, "||"),
            (TokenKind::Not, "!"),
            (TokenKind::Amp, "&"),
            (TokenKind::Pipe, "|"),
            (TokenKind::Caret, "^"),
            (TokenKind::Tilde, "~"),
            (TokenKind::Plus, "+"),
            (TokenKind::Minus, "-"),
            (TokenKind::Star, "*"),
            (TokenKind::StarStar, "**"),
            (TokenKind::Slash, "/"),
            (TokenKind::Percent, "%"),
            (TokenKind::Assign, ":="),
            (TokenKind::LParen, "("),
            (TokenKind::RParen, ")"),
            (TokenKind::LBracket, "["),
            (TokenKind::RBracket, "]"),
            (TokenKind::Comma, ","),
            (TokenKind::Unknown('@'), "@"),
        ];
        for (kind, text) in cases {
            assert_eq!(kind.to_string(), text, "Display mismatch for {kind:?}");
        }
        // Literal payloads.
        assert_eq!(TokenKind::Str("hi".into()).to_string(), "\"hi\"");
        assert_eq!(TokenKind::Float(2.5).to_string(), "2.5");
    }

    // ---- MUGEN-semantics: realistic trigger expressions ----

    #[test]
    fn range_trigger_with_function_call() {
        // `AnimElem = [6,9]` — equality against an inclusive range, a common
        // MUGEN idiom for "during animation elements 6 through 9".
        assert_eq!(
            kinds("AnimElem = [6,9]"),
            vec![
                TokenKind::Ident("AnimElem".into()),
                TokenKind::Eq,
                TokenKind::LBracket,
                TokenKind::Int(6),
                TokenKind::Comma,
                TokenKind::Int(9),
                TokenKind::RBracket,
            ]
        );
    }

    #[test]
    fn exclusive_range_with_floats() {
        // `(0.0,1.0)` — exclusive range using float bounds.
        assert_eq!(
            kinds("Pos Y = (0.0,1.0)"),
            vec![
                TokenKind::Ident("Pos".into()),
                TokenKind::Ident("Y".into()),
                TokenKind::Eq,
                TokenKind::LParen,
                TokenKind::Float(0.0),
                TokenKind::Comma,
                TokenKind::Float(1.0),
                TokenKind::RParen,
            ]
        );
    }

    #[test]
    fn redirection_expression_with_comma() {
        // `enemy, P2BodyDist X` — redirection: identifier, comma, then the
        // redirected trigger. The lexer just splits the pieces.
        assert_eq!(
            kinds("enemy, P2BodyDist X"),
            vec![
                TokenKind::Ident("enemy".into()),
                TokenKind::Comma,
                TokenKind::Ident("P2BodyDist".into()),
                TokenKind::Ident("X".into()),
            ]
        );
    }

    #[test]
    fn variable_assignment_redirection() {
        // `root, var(0) := 1` — assignment through a redirection, exercising
        // Ident, Comma, function-call parens, Assign, and Int together.
        assert_eq!(
            kinds("root, var(0) := 1"),
            vec![
                TokenKind::Ident("root".into()),
                TokenKind::Comma,
                TokenKind::Ident("var".into()),
                TokenKind::LParen,
                TokenKind::Int(0),
                TokenKind::RParen,
                TokenKind::Assign,
                TokenKind::Int(1),
            ]
        );
    }

    #[test]
    fn not_gethitvar_call() {
        // `!gethitvar(isbound)` — a real KFM trigger (kfm.cns). Not, ident,
        // parens, ident.
        assert_eq!(
            kinds("!gethitvar(isbound)"),
            vec![
                TokenKind::Not,
                TokenKind::Ident("gethitvar".into()),
                TokenKind::LParen,
                TokenKind::Ident("isbound".into()),
                TokenKind::RParen,
            ]
        );
    }

    #[test]
    fn dotted_member_name_is_one_ident() {
        // Task 4.10 gap 3: `gethitvar(fall.yvel)` — the dotted member name is a
        // single Ident so it can be passed through as one argument key.
        assert_eq!(
            kinds("gethitvar(fall.yvel)"),
            vec![
                TokenKind::Ident("gethitvar".into()),
                TokenKind::LParen,
                TokenKind::Ident("fall.yvel".into()),
                TokenKind::RParen,
            ]
        );
        // A multi-dot member (`fall.envshake.time`) folds entirely too.
        assert_eq!(
            kinds("fall.envshake.time"),
            vec![TokenKind::Ident("fall.envshake.time".into())]
        );
    }

    #[test]
    fn dot_not_between_idents_is_not_folded() {
        // A trailing dot after an identifier is NOT consumed into it (no
        // following identifier char): `fall.` is `Ident("fall")` then Unknown('.').
        assert_eq!(
            kinds("fall."),
            vec![TokenKind::Ident("fall".into()), TokenKind::Unknown('.')]
        );
        // A dot before a digit is not folded: `x.5` is Ident, then Float(0.5)
        // (the lexer routes `.5` to the number path).
        assert_eq!(
            kinds("x.5"),
            vec![TokenKind::Ident("x".into()), TokenKind::Float(0.5)]
        );
        // A numeric literal with a fractional part is unaffected (digit-led →
        // lex_number, never lex_ident): `5.x` stays Float then Ident.
        assert_eq!(
            kinds("5.x"),
            vec![TokenKind::Float(5.0), TokenKind::Ident("x".into())]
        );
    }

    #[test]
    fn cond_function_with_nested_arithmetic() {
        // `cond(var(0) > 0, life - 10, life)` — nested calls + arithmetic.
        let ks = kinds("cond(var(0) > 0, life - 10, life)");
        assert_eq!(
            ks,
            vec![
                TokenKind::Ident("cond".into()),
                TokenKind::LParen,
                TokenKind::Ident("var".into()),
                TokenKind::LParen,
                TokenKind::Int(0),
                TokenKind::RParen,
                TokenKind::Gt,
                TokenKind::Int(0),
                TokenKind::Comma,
                TokenKind::Ident("life".into()),
                TokenKind::Minus,
                TokenKind::Int(10),
                TokenKind::Comma,
                TokenKind::Ident("life".into()),
                TokenKind::RParen,
            ]
        );
    }

    #[test]
    fn trailing_comment_after_real_trigger() {
        // CNS files frequently put `;` comments after a trigger value.
        assert_eq!(
            kinds("AnimTime = 0 ; end of anim"),
            vec![
                TokenKind::Ident("AnimTime".into()),
                TokenKind::Eq,
                TokenKind::Int(0),
            ]
        );
    }

    /// Tokenizes every trigger right-hand side in the real KFM character file,
    /// asserting the lexer never panics and never emits an `Unknown` token for
    /// production MUGEN content. Gated on the fixture's presence so the default
    /// `cargo test` still passes when `test-assets/` is absent.
    #[test]
    fn real_kfm_cns_triggers_lex_cleanly() {
        use std::path::Path;

        // crates/fp-vm/src/lexer.rs -> repo root is three levels up.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/kfm/kfm.cns");
        if !path.exists() {
            eprintln!("skipping real_kfm_cns_triggers_lex_cleanly: {path:?} absent");
            return;
        }

        let text = std::fs::read_to_string(&path).expect("read kfm.cns");
        let mut checked = 0usize;
        for line in text.lines() {
            let trimmed = line.trim_start();
            let lower = trimmed.to_ascii_lowercase();
            // Match `trigger1 = ...`, `triggerall = ...`, etc.
            if !(lower.starts_with("trigger") && line.contains('=')) {
                continue;
            }
            // Take the right-hand side after the first `=`.
            let rhs = line.split_once('=').unwrap().1;
            let toks = tokenize(rhs);
            // Production content must not produce Unknown tokens.
            for t in &toks {
                assert!(
                    !matches!(t.kind, TokenKind::Unknown(_)),
                    "unexpected Unknown token in real trigger {rhs:?}: {:?}",
                    t.kind
                );
            }
            checked += 1;
        }
        assert!(
            checked > 0,
            "fixture present but no trigger lines were parsed"
        );
    }

    // =====================================================================
    // Proctor (task 4.10) — additional dotted-identifier lexer edge cases.
    // Gap 3 folds `ident(.ident)*` into one Ident token; these pin the
    // boundary conditions the existing tests do not, so a future lexer change
    // that over- or under-folds is caught.
    // =====================================================================

    #[test]
    fn dotted_ident_underscore_and_digit_members_fold() {
        // A member name may contain digits and underscores after its first
        // letter — `fall.envshake.time` already covered; here a mixed member.
        assert_eq!(
            kinds("get.var_2.x"),
            vec![TokenKind::Ident("get.var_2.x".into())]
        );
        // An underscore-led member segment is an identifier-start char, so it
        // folds: `a._b` is one ident.
        assert_eq!(kinds("a._b"), vec![TokenKind::Ident("a._b".into())]);
    }

    #[test]
    fn dotted_ident_does_not_swallow_following_operator_or_paren() {
        // The fold stops at the first non-identifier char, so an operator or
        // delimiter right after a dotted member stays its own token.
        assert_eq!(
            kinds("fall.yvel = 0"),
            vec![
                TokenKind::Ident("fall.yvel".into()),
                TokenKind::Eq,
                TokenKind::Int(0),
            ]
        );
        assert_eq!(
            kinds("gethitvar(fall.yvel)+1"),
            vec![
                TokenKind::Ident("gethitvar".into()),
                TokenKind::LParen,
                TokenKind::Ident("fall.yvel".into()),
                TokenKind::RParen,
                TokenKind::Plus,
                TokenKind::Int(1),
            ]
        );
    }

    #[test]
    fn double_dot_in_ident_is_not_folded_across_empty_segment() {
        // `a..b` — the first dot is followed by another dot (not an
        // identifier-start char), so it is NOT folded into `a`. The lexer
        // emits `Ident("a")` then handles the dots without panicking. The exact
        // remainder is not load-bearing; the contract is "never crash" and that
        // the leading ident does not absorb a `..`.
        let ks = kinds("a..b");
        assert_eq!(ks.first(), Some(&TokenKind::Ident("a".into())));
        // ...and the trailing `b` survives as its own ident somewhere after.
        assert!(
            ks.iter()
                .any(|k| matches!(k, TokenKind::Ident(s) if s == "b")),
            "trailing member should remain a separate ident: {ks:?}"
        );
    }

    #[test]
    fn dotted_member_after_digit_in_name_still_folds() {
        // The member-continuation rule only requires the char after the dot to
        // be a letter/underscore; the char before may be a digit (idents may
        // contain digits): `p2.x` folds (`2` precedes the dot, `x` follows).
        assert_eq!(kinds("p2.x"), vec![TokenKind::Ident("p2.x".into())]);
    }
}
