//! # fp-vm
//!
//! Bytecode compiler and stack-based virtual machine for evaluating MUGEN
//! trigger expressions. Expressions in CNS files are compiled at load time
//! into compact bytecode and executed at runtime via a stack-based interpreter.
//!
//! ## Pipeline
//!
//! The expression engine is built in stages (see the execution plan, Phase 4):
//!
//! 1. **Lexer** ([`lexer`]) — turns expression source text into a stream of
//!    [`Token`]s. Implemented.
//! 2. **Parser** ([`parser`]) — builds an [`Expr`] AST with full operator
//!    precedence. Implemented.
//! 3. **Evaluation model** ([`mod@eval`]) — the runtime [`Value`] type,
//!    redirection targets, and the [`EvalContext`] trait the tree-walk evaluator
//!    queries. Implemented (4.3).
//! 4. **Evaluator** ([`evaluator`]) — walks the [`Expr`] AST against an
//!    [`EvalContext`], producing a [`Value`] with faithful MUGEN numeric
//!    semantics. Implemented (4.4).
//!
//! The lexer is deliberately tolerant: malformed input never panics, in keeping
//! with the engine-wide "never crash on bad content" rule. The evaluator upholds
//! the same contract: every error path (divide-by-zero, unknown trigger, …)
//! resolves to the safe default `Value::Int(0)`.

#![warn(missing_docs)]

pub mod eval;
pub mod evaluator;
pub mod lexer;
pub mod parser;

pub use eval::{EvalContext, Redirect, Value};
pub use evaluator::{eval, Rng};
pub use lexer::{tokenize, Token, TokenKind};
pub use parser::{parse, parse_str, BinaryOp, Bound, Expr, ParseError, UnaryOp};
