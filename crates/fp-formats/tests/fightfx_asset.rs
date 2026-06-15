//! Conformance test for the shipped, ORIGINAL common-effects (`fightfx`) asset.
//!
//! `assets/data/fightfx.sff` + `assets/data/fightfx.air` are clean-room, authored
//! from scratch for this engine (MIT — no Elecbyte/MUGEN content; the only ASCII
//! string in the `.sff` is the required `ElecbyteSpr\0` format magic). Like the
//! Training Dummy character, they **ship in the repository**, so this test is NOT
//! asset-gated — it must load and decode on every machine and on CI.
//!
//! If you regenerate the fightfx asset, this is the test that proves the bytes
//! still parse and every spark sprite decodes.

use std::path::{Path, PathBuf};

use fp_formats::air::AirFile;
use fp_formats::sff::{SffFile, SffVersion};

/// Resolves a path inside the workspace `assets/data/` directory.
///
/// Integration tests run with the *crate* directory as the manifest root
/// (`crates/fp-formats`), so go up two levels to the workspace root.
fn fx_asset(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/data")
        .join(rel)
}

/// The common spark action / sprite-group indices the asset must ship (the
/// standard set Kung Fu Man uses, plus a dedicated guard spark at 120).
const EXPECTED_GROUPS: &[u16] = &[0, 1, 2, 3, 40, 120];

#[test]
fn fightfx_asset_files_exist_in_repo() {
    // Shippable content: both files must be present (no skip).
    for rel in ["fightfx.sff", "fightfx.air"] {
        let p = fx_asset(rel);
        assert!(
            p.exists(),
            "shipped common-fx asset missing: {} — it must be committed",
            p.display()
        );
    }
}

#[test]
fn fightfx_sff_loads_as_v2_and_decodes_every_sprite() {
    let sff = SffFile::load(&fx_asset("fightfx.sff"))
        .unwrap_or_else(|e| panic!("fightfx.sff failed to load: {e}"));

    assert_eq!(sff.version, SffVersion::V2, "fightfx ships as SFF v2");
    assert!(!sff.sprites.is_empty(), "fightfx must ship spark sprites");
    assert!(!sff.palettes.is_empty(), "fightfx must ship a palette");

    // Every sprite must decode to RGBA without error (proves the synthesized
    // RLE8 blobs + palette round-trip through the real parser).
    for (i, s) in sff.sprites.iter().enumerate() {
        assert!(
            s.width > 0 && s.height > 0,
            "sprite {i} has zero dimensions"
        );
        let rgba = sff
            .decode_sprite_rgba(i)
            .unwrap_or_else(|e| panic!("fightfx sprite {i} failed to decode: {e}"));
        assert_eq!(
            rgba.len(),
            s.width as usize * s.height as usize * 4,
            "sprite {i} decoded to the wrong pixel count"
        );
        // A spark must have at least one non-transparent pixel (some glow).
        assert!(
            rgba.chunks_exact(4).any(|px| px[3] != 0),
            "sprite {i} decoded fully transparent — not a visible spark"
        );
    }
}

#[test]
fn fightfx_sff_has_every_expected_spark_group() {
    let sff = SffFile::load(&fx_asset("fightfx.sff"))
        .unwrap_or_else(|e| panic!("fightfx.sff failed to load: {e}"));
    for &g in EXPECTED_GROUPS {
        assert!(
            sff.sprite(g, 0).is_some(),
            "fightfx.sff missing spark sprite group {g}, image 0"
        );
    }
}

#[test]
fn fightfx_air_has_every_expected_spark_action() {
    let air = AirFile::load(&fx_asset("fightfx.air"))
        .unwrap_or_else(|e| panic!("fightfx.air failed to load: {e}"));
    for &g in EXPECTED_GROUPS {
        let action = air
            .action(i32::from(g))
            .unwrap_or_else(|| panic!("fightfx.air missing spark action {g}"));
        assert!(
            !action.frames.is_empty(),
            "fightfx spark action {g} has no frames"
        );
        // Sparks are one-shot: the action must reference its own sprite group.
        assert!(
            action.frames.iter().all(|f| f.sprite.group() == g),
            "fightfx action {g} references a foreign sprite group"
        );
    }
}

#[test]
fn fightfx_sff_is_clean_room_only_magic_ascii() {
    // The ONLY long ASCII run permitted in the synthesized SFF is the required
    // `ElecbyteSpr` format magic. This guards the clean-room contract: no stray
    // copyrighted strings (author names, tool banners, Elecbyte data) leaked in.
    let bytes = std::fs::read(fx_asset("fightfx.sff")).expect("read fightfx.sff");
    let mut runs: Vec<String> = Vec::new();
    let mut cur = String::new();
    for &b in &bytes {
        if b.is_ascii_graphic() || b == b' ' {
            cur.push(b as char);
        } else {
            if cur.len() >= 4 {
                runs.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if cur.len() >= 4 {
        runs.push(cur);
    }
    assert_eq!(
        runs,
        vec!["ElecbyteSpr".to_string()],
        "fightfx.sff must contain only the ElecbyteSpr magic as ASCII, found: {runs:?}"
    );
}
