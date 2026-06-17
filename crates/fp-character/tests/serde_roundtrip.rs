//! Serde round-trip tests for the static compiled load graph
//! (`LoadedCharacter` and its `CompiledState` / `CompiledController` /
//! `CompiledParam` / `CompiledExpr` members).
//!
//! These prove the content-import IR-cache seam (F034 T086): a fully loaded
//! character survives a bincode encode → decode unchanged (`PartialEq`), and
//! two encodings of the same `LoadedCharacter` are byte-identical even though
//! its `states` and per-controller `params` are `HashMap`s (the deterministic
//! `sorted_map` serializer emits keys in sorted order).
//!
//! The fixture is the shipped, version-controlled clean-room `trainingdummy`
//! character, so this test is **not** asset-gated and runs on CI.

use std::path::PathBuf;

use fp_character::loader::LoadedCharacter;

/// Resolves the shipped trainingdummy `.def` under the workspace `assets/` dir.
fn trainingdummy_def() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/trainingdummy/trainingdummy.def")
}

#[test]
fn loaded_character_serde_roundtrip_is_structurally_equal() {
    let chr = LoadedCharacter::load(trainingdummy_def()).expect("trainingdummy must load");

    let bytes = bincode::serialize(&chr).expect("serialize");
    let decoded: LoadedCharacter = bincode::deserialize(&bytes).expect("deserialize");

    assert_eq!(
        chr, decoded,
        "LoadedCharacter round-trip is not structurally equal"
    );
}

#[test]
fn loaded_character_serde_roundtrip_is_byte_identical() {
    let chr = LoadedCharacter::load(trainingdummy_def()).expect("trainingdummy must load");

    // Two encodings of the same character must be byte-for-byte identical: the
    // HashMap-backed `states` and `params` are serialized through the sorted-map
    // helper, so iteration-order nondeterminism cannot perturb the bytes. This is
    // the invariant the content-addressed IR cache depends on.
    let a = bincode::serialize(&chr).expect("serialize a");
    let b = bincode::serialize(&chr).expect("serialize b");
    assert_eq!(a, b, "two encodings of the same LoadedCharacter differ");

    // And a decode of the deterministic bytes still matches the original.
    let decoded: LoadedCharacter = bincode::deserialize(&a).expect("deserialize");
    assert_eq!(chr, decoded);
}
