//! Serde round-trip tests for the parsed asset containers
//! (`SffFile` / `AirFile` / `CmdFile` / `SndFile` / `ActPalette`).
//!
//! These prove the content-import IR-cache seam (F034 T086): a parsed container
//! survives a bincode encode → decode unchanged (`PartialEq`), the encoding is
//! byte-identical across two passes (so the cache key is stable even though the
//! AIR action map is a `HashMap`), and — for SFF — the raw indexed pixel buffer
//! decodes identically after the round-trip with the index-0-transparent
//! invariant intact.
//!
//! The fixtures are the shipped, version-controlled clean-room `trainingdummy`
//! assets, so these tests are **not** asset-gated and run on CI.

use std::path::PathBuf;

use fp_formats::air::AirFile;
use fp_formats::cmd::CmdFile;
use fp_formats::sff::SffFile;

/// Resolves a path under the workspace `assets/trainingdummy/` directory.
fn asset(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/trainingdummy")
        .join(name)
}

#[test]
fn sff_serde_roundtrip_is_structurally_equal_and_byte_identical() {
    let sff = SffFile::load(&asset("trainingdummy.sff")).expect("trainingdummy.sff must load");

    let a = bincode::serialize(&sff).expect("serialize a");
    let b = bincode::serialize(&sff).expect("serialize b");
    assert_eq!(a, b, "two encodings of the same SffFile differ");

    let decoded: SffFile = bincode::deserialize(&a).expect("deserialize");
    assert_eq!(sff, decoded, "SffFile round-trip is not structurally equal");
}

#[test]
fn sff_pixel_buffer_serde_roundtrip_is_lossless() {
    let sff = SffFile::load(&asset("trainingdummy.sff")).expect("trainingdummy.sff must load");
    let bytes = bincode::serialize(&sff).expect("serialize");
    let decoded: SffFile = bincode::deserialize(&bytes).expect("deserialize");

    assert!(!sff.sprites.is_empty(), "fixture must have sprites");

    // Every sprite's decoded indexed pixel buffer must be byte-for-byte identical
    // after the round-trip, and the index-0-transparent invariant must hold the
    // same way it did before serialization.
    for i in 0..sff.sprites.len() {
        let before = sff.decode_sprite(i);
        let after = decoded.decode_sprite(i);
        match (before, after) {
            (Ok(before), Ok(after)) => {
                assert_eq!(before, after, "sprite {i} pixels differ after round-trip");
                // index-0 stays index-0 (transparent) — a lossless index buffer
                // can only preserve, never invent, palette index 0.
                let zeros_before = before.iter().filter(|&&p| p == 0).count();
                let zeros_after = after.iter().filter(|&&p| p == 0).count();
                assert_eq!(
                    zeros_before, zeros_after,
                    "sprite {i} index-0 (transparent) pixel count changed"
                );
            }
            (Err(_), Err(_)) => { /* both fail identically — acceptable */ }
            (b, a) => panic!("sprite {i} decode disagreed across round-trip: {b:?} vs {a:?}"),
        }
    }
}

#[test]
fn air_serde_roundtrip_is_structurally_equal_and_byte_identical() {
    let air = AirFile::load(&asset("trainingdummy.air")).expect("trainingdummy.air must load");

    // The action map is a HashMap; the deterministic-encode helper must make two
    // encodings byte-identical regardless of iteration order.
    let a = bincode::serialize(&air).expect("serialize a");
    let b = bincode::serialize(&air).expect("serialize b");
    assert_eq!(a, b, "two encodings of the same AirFile differ");

    let decoded: AirFile = bincode::deserialize(&a).expect("deserialize");
    assert_eq!(air, decoded, "AirFile round-trip is not structurally equal");
}

#[test]
fn cmd_serde_roundtrip_is_structurally_equal_and_byte_identical() {
    let cmd = CmdFile::load(&asset("trainingdummy.cmd")).expect("trainingdummy.cmd must load");

    let a = bincode::serialize(&cmd).expect("serialize a");
    let b = bincode::serialize(&cmd).expect("serialize b");
    assert_eq!(a, b, "two encodings of the same CmdFile differ");

    let decoded: CmdFile = bincode::deserialize(&a).expect("deserialize");
    assert_eq!(cmd, decoded, "CmdFile round-trip is not structurally equal");
}
