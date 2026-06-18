//! Serde round-trip tests for the compiled expression AST (`Expr`).
//!
//! These prove the content-import IR-cache seam (F034 T086): the parsed
//! expression tree — the leaf of the static load graph — survives a bincode
//! encode → decode unchanged (`PartialEq`), and two encodings of the same value
//! are byte-identical so the content-addressed cache key is stable.

use fp_vm::parse_str;

/// A spread of expression shapes exercising every `Expr` variant reachable from
/// real CNS: literals, idents, unary/binary ops, calls, ranges, redirects, the
/// `AnimElem`/`TimeMod`/`HitDefAttr`/`Proj` comma-tail forms, and `:=` assign.
fn sample_sources() -> Vec<&'static str> {
    vec![
        "42",
        "3.14",
        "\"hello\"",
        "Time",
        "-x",
        "!flag",
        "a + b * 2",
        "Time >= 30 && stateno = 200",
        "cond(var(0), 1, 0)",
        "Vel Y",
        "P2BodyDist X",
        "var(5) := 8000",
        "enemy, life + 1",
        "root, var(0)",
        "AnimElem = 2, >= 0",
        "TimeMod = 20, 19",
        "HitDefAttr = S, NA, SA, HA",
        "ProjContact2000 = 1, < 20",
        "[6,9]",
    ]
}

#[test]
fn expr_serde_roundtrip_is_structurally_equal() {
    for src in sample_sources() {
        let expr = parse_str(src).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
        let bytes = bincode::serialize(&expr).expect("serialize");
        let decoded: fp_vm::Expr = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(expr, decoded, "round-trip mismatch for {src:?}");
    }
}

#[test]
fn expr_serde_roundtrip_is_byte_identical() {
    for src in sample_sources() {
        let expr = parse_str(src).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
        let a = bincode::serialize(&expr).expect("serialize a");
        let b = bincode::serialize(&expr).expect("serialize b");
        assert_eq!(a, b, "two encodings of {src:?} differ");
    }
}
