//! End-to-end integration tests against the real Kung Fu Man (KFM) fixture.
//!
//! These tests load the genuine MUGEN content shipped under the workspace's
//! `test-assets/kfm/` directory and assert that every parser in `fp-formats`
//! handles it end-to-end. Real content exercises quirks the synthetic unit
//! tests cannot: UTF-8 BOMs, CRLF line endings, SFF v1 *and* v2 containers, and
//! the sheer scale/variety of a shipping character.
//!
//! ## Skip-if-missing
//!
//! `test-assets/` is local-only and may be absent (e.g. on CI). Every test
//! resolves the fixture directory relative to `CARGO_MANIFEST_DIR` and **early
//! returns cleanly** when the asset is missing, so `cargo test -p fp-formats`
//! stays green whether or not the assets are present.
//!
//! ## RLE5: synthesized, never skipped
//!
//! The shipping `kfm.sff` contains only RLE8 + LZ5 sprites — zero RLE5 — so it
//! gives the RLE5 decoder no real-content coverage. Rather than skip RLE5
//! entirely, [`synthetic_rle5_sff_decodes_end_to_end`] *builds* a genuine SFF v2
//! container with one RLE5 sprite at runtime and decodes it through the on-disk
//! `SffFile::load` path. Because the fixture is generated, that test always runs
//! — including on CI where `test-assets/` is absent.

use std::path::{Path, PathBuf};

use fp_formats::air::AirFile;
use fp_formats::cmd::CmdFile;
use fp_formats::cns::CnsFile;
use fp_formats::def::DefFile;
use fp_formats::sff::{SffFile, SffVersion, SpriteFormat};
use fp_formats::snd::SndFile;

/// Resolves a path inside the workspace `test-assets/kfm/` directory.
///
/// Integration tests run with the *crate* directory as the manifest root, so
/// `CARGO_MANIFEST_DIR` points at `crates/fp-formats`; we go up two levels to
/// reach the workspace root before descending into `test-assets/kfm`.
fn kfm_asset(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-assets/kfm")
        .join(rel)
}

/// Returns `Some(path)` if the asset exists, or `None` (with a notice) if not.
///
/// Centralizes the skip-if-missing behavior so every test reads the same.
fn require(rel: &str) -> Option<PathBuf> {
    let path = kfm_asset(rel);
    if path.exists() {
        Some(path)
    } else {
        eprintln!(
            "skipping real-content check: {} not present (test-assets/ is local-only)",
            path.display()
        );
        None
    }
}

#[test]
fn kfm_sff_v2_loads_and_decodes_a_sprite() {
    let Some(path) = require("kfm.sff") else {
        return;
    };

    let sff = SffFile::load(&path).expect("kfm.sff (SFF v2) should load");
    assert_eq!(sff.version, SffVersion::V2, "kfm.sff is an SFF v2 file");
    assert!(
        !sff.sprites.is_empty(),
        "kfm.sff should contain at least one sprite"
    );

    // Decode at least one real sprite end-to-end. Some entries can be zero-size
    // links or use formats we route through specific decoders; scan until one
    // decodes to non-empty pixel data so the assertion is meaningful.
    let decoded = (0..sff.sprites.len())
        .filter_map(|i| sff.decode_sprite(i).ok())
        .find(|pixels| !pixels.is_empty());
    assert!(
        decoded.is_some(),
        "at least one kfm.sff sprite should decode to non-empty pixels"
    );
}

/// Stronger check: every kfm.sff sprite (overwhelmingly LZ5-compressed) must
/// decode without error to exactly `width * height` palette indices, and the
/// decoded pixels must not be entirely zeroes (which would betray a decoder that
/// silently zero-pads on failure). This is the regression guard for the LZ5
/// codec — a generic-LZ77 implementation fails the bulk of these sprites.
#[test]
fn kfm_sff_v2_decodes_every_sprite_to_exact_size() {
    let Some(path) = require("kfm.sff") else {
        return;
    };

    let sff = SffFile::load(&path).expect("kfm.sff (SFF v2) should load");

    let mut checked = 0usize;
    let mut nonzero_sprites = 0usize;
    for (i, sprite) in sff.sprites.iter().enumerate() {
        let expected = sprite.width as usize * sprite.height as usize;
        if expected == 0 {
            continue; // zero-area entries carry no pixels to validate
        }
        let pixels = sff
            .decode_sprite(i)
            .unwrap_or_else(|e| panic!("kfm.sff sprite {i} should decode, got error: {e}"));
        assert_eq!(
            pixels.len(),
            expected,
            "kfm.sff sprite {i} ({}x{}) decoded to {} pixels, expected {expected}",
            sprite.width,
            sprite.height,
            pixels.len()
        );
        if pixels.iter().any(|&p| p != 0) {
            nonzero_sprites += 1;
        }
        checked += 1;
    }

    assert!(checked > 0, "kfm.sff should have decodable sprites");
    // Real artwork is not all-transparent; the vast majority must carry pixels.
    assert!(
        nonzero_sprites * 2 > checked,
        "most kfm.sff sprites should decode to real (non-zero) pixel data \
         ({nonzero_sprites}/{checked} non-zero)"
    );
}

#[test]
fn kfm_sff_v1_loads() {
    // intro.sff is the SFF v1 fixture; fall back to ending.sff if absent.
    let path = require("intro.sff").or_else(|| require("ending.sff"));
    let Some(path) = path else {
        return;
    };

    let sff = SffFile::load(&path).expect("an SFF v1 file should load");
    assert_eq!(
        sff.version,
        SffVersion::V1,
        "intro.sff/ending.sff are SFF v1 files"
    );
    assert!(
        !sff.sprites.is_empty(),
        "the SFF v1 file should contain at least one sprite"
    );

    // Decoding must never panic, even on real PCX data. Beyond no-panic, at least
    // one inline PCX sprite must decode to genuine (non-empty) pixel data,
    // exercising the PCX RLE path against real WinMUGEN-era content.
    let decoded_any = (0..sff.sprites.len())
        .filter_map(|i| sff.decode_sprite(i).ok())
        .any(|pixels| !pixels.is_empty());
    assert!(
        decoded_any,
        "at least one SFF v1 sprite should decode to non-empty PCX pixels"
    );
}

#[test]
fn kfm_air_loads_with_actions() {
    let Some(path) = require("kfm.air") else {
        return;
    };

    let air = AirFile::load(&path).expect("kfm.air should load");
    assert!(
        !air.actions.is_empty(),
        "kfm.air should contain at least one action"
    );
    // KFM's idle stance is action 0 — a good real-content sanity check.
    let stance = air.action(0).expect("kfm.air should define action 0");
    assert!(
        !stance.frames.is_empty(),
        "action 0 should have at least one frame"
    );
}

#[test]
fn kfm_cmd_loads_with_commands() {
    let Some(path) = require("kfm.cmd") else {
        return;
    };

    let cmd = CmdFile::load(&path).expect("kfm.cmd should load");
    assert!(
        !cmd.commands.is_empty(),
        "kfm.cmd should contain at least one command"
    );
    // Every parsed command must carry a non-empty name.
    assert!(
        cmd.commands.iter().all(|c| !c.name.is_empty()),
        "all kfm.cmd commands should have a name"
    );
}

#[test]
fn kfm_def_loads_with_info_name() {
    let Some(path) = require("kfm.def") else {
        return;
    };

    let def = DefFile::load(&path).expect("kfm.def should load");
    let name = def
        .get("Info", "name")
        .expect("kfm.def [Info] section should have a name");
    assert!(!name.is_empty(), "kfm.def [Info] name should be non-empty");
    // The KFM fixture's display name is the canonical "Kung Fu Man".
    assert!(
        name.to_ascii_lowercase().contains("kung fu man"),
        "unexpected kfm.def name: {name:?}"
    );

    // The trailing inline `;Name ...` comment on the real name line (after the
    // quoted value) must not leak into the parsed value.
    assert!(
        !name.contains(';'),
        "inline comment leaked into parsed name: {name:?}"
    );

    // The [Files] section must reference the sibling fixture files, and each
    // referenced file must resolve to a path that actually exists on disk —
    // a real end-to-end check that DEF path resolution works.
    let sprite_ref = def
        .get("Files", "sprite")
        .expect("kfm.def [Files] should reference a sprite file");
    assert!(
        sprite_ref.eq_ignore_ascii_case("kfm.sff"),
        "unexpected sprite reference: {sprite_ref:?}"
    );
    let resolved = DefFile::resolve_path(&path, sprite_ref);
    assert!(
        resolved.exists(),
        "resolved sprite path should exist: {}",
        resolved.display()
    );
}

#[test]
fn kfm_cns_loads_with_statedefs() {
    let Some(path) = require("kfm.cns") else {
        return;
    };

    let cns = CnsFile::load(&path).expect("kfm.cns should load");
    assert!(
        !cns.statedefs.is_empty(),
        "kfm.cns should contain at least one statedef"
    );
    // Real characters define negative system statedefs (e.g. -1/-2/-3).
    assert!(
        cns.statedefs.iter().any(|s| s.number < 0),
        "kfm.cns should contain a negative (system) statedef"
    );
}

#[test]
fn common1_cns_loads_with_statedefs() {
    let Some(path) = require("common1.cns") else {
        return;
    };

    let cns = CnsFile::load(&path).expect("common1.cns should load");
    assert!(
        !cns.statedefs.is_empty(),
        "common1.cns should contain at least one statedef"
    );
}

#[test]
fn kfm_snd_loads_and_exposes_wav_payloads() {
    let Some(path) = require("kfm.snd") else {
        return;
    };

    let snd = SndFile::load(&path).expect("kfm.snd should load");

    // KFM ships an ElecbyteSnd v4.0.0.0 container.
    assert_eq!(snd.version, 4, "kfm.snd is an ElecbyteSnd v4.0.0.0 file");

    // The container should yield a non-zero number of sounds.
    assert!(!snd.is_empty(), "kfm.snd should contain at least one sound");

    // Every payload should be non-empty and begin with a RIFF/WAVE header — the
    // observed payload format in KFM is standard RIFF-wrapped WAVE/PCM.
    for entry in &snd.sounds {
        assert!(
            !entry.data.is_empty(),
            "sound ({}, {}) should have a non-empty payload",
            entry.group,
            entry.sample
        );
        assert!(
            entry.data.starts_with(b"RIFF"),
            "sound ({}, {}) should begin with a RIFF header (got {:?})",
            entry.group,
            entry.sample,
            &entry.data[..entry.data.len().min(4)]
        );
    }

    // KFM defines sound (0, 0); a direct lookup must resolve to its bytes.
    let s0 = snd.sound(0, 0).expect("kfm.snd should define sound (0, 0)");
    assert!(s0.starts_with(b"RIFF"), "sound (0, 0) should be a RIFF blob");

    // Documented observed format: the payload is a complete RIFF/WAVE container
    // (the "WAVE" form-type and a "fmt " chunk both appear), confirming we
    // capture the whole blob, not a truncated prefix. fp-audio decodes it later.
    assert!(
        s0.len() > 12 && &s0[8..12] == b"WAVE",
        "sound (0, 0) should be a RIFF/WAVE container, got form {:?}",
        &s0[8..s0.len().min(12)]
    );
    assert!(
        s0.windows(4).any(|w| w == b"fmt "),
        "sound (0, 0) WAVE payload should contain a 'fmt ' chunk"
    );

    // The KFM container holds exactly 12 sounds laid out as a linked list that
    // begins after a ~488-byte zero gap (first_offset = 512). Recovering all 12
    // proves the directory walk follows `next` across the gap correctly.
    assert_eq!(
        snd.len(),
        12,
        "kfm.snd is known to declare 12 sounds; recovered {}",
        snd.len()
    );

    // The observed (group, sample) keys form contiguous samples in group 0
    // (0,0)..=(0,N): KFM's voice/SFX bank. Spot-check that (0, 1) also resolves
    // to a RIFF blob, exercising traversal past the first entry.
    let s1 = snd.sound(0, 1).expect("kfm.snd should define sound (0, 1)");
    assert!(s1.starts_with(b"RIFF"), "sound (0, 1) should be a RIFF blob");

    // No sound should carry a zero-byte payload in the real fixture, and every
    // declared length must round-trip into the captured data slice.
    let total: usize = snd.sounds.iter().map(|e| e.data.len()).sum();
    assert!(total > 0, "kfm.snd payloads should total non-zero bytes");

    // A clearly absent key returns None.
    assert!(snd.sound(9999, 9999).is_none());
}

/// Builds a complete, valid SFF v2 file holding a single RLE5-compressed sprite.
///
/// The layout mirrors the real MUGEN 1.0 SFF v2 format (see `sff::header`): a
/// 512-byte header whose directory fields at offsets 36/40/44/48/52/56/60/64
/// store *counts and offsets* (block lengths are derived from the counts), one
/// 28-byte sprite sub-header, one 16-byte palette sub-header, a 768-byte LData
/// palette block, and a TData block carrying the RLE5 codec stream.
///
/// The codec stream `[0x00, 0x82, 0x05, 0x23, 0x47]` (after its 4-byte LE
/// decompressed-size prefix `6,0,0,0`) decodes to the 6 palette indices
/// `[5, 3, 3, 7, 7, 7]`:
///   - header: `rl = 0` (emit the colour once), data byte `0x82` -> `dl = 2`
///     (two further segments) with the high bit set, so an explicit colour byte
///     `0x05` follows -> emit `[5]`
///   - segment `0x23`: colour `0x23 & 0x1f = 3`, run `(0x23 >> 5) + 1 = 2` -> `[3, 3]`
///   - segment `0x47`: colour `0x47 & 0x1f = 7`, run `(0x47 >> 5) + 1 = 3` -> `[7, 7, 7]`
fn synthesize_rle5_sff() -> Vec<u8> {
    // RLE5 codec stream: 4-byte LE decompressed size (6) followed by the packet.
    let rle5: [u8; 9] = [6, 0, 0, 0, 0x00, 0x82, 0x05, 0x23, 0x47];

    let sprite_offset: u32 = 512;
    let palette_offset: u32 = 540;
    let ldata_offset: u32 = 556;
    let ldata_length: u32 = 768; // 256 RGB triples
    let tdata_offset: u32 = ldata_offset + ldata_length; // 1324
    let tdata_length: u32 = rle5.len() as u32;

    let total = tdata_offset as usize + tdata_length as usize;
    let mut buf = vec![0u8; total];

    // --- Header (MUGEN 1.0 SFF v2 layout) ---
    buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
    buf[12] = 0; // version minor3
    buf[13] = 0; // version minor2
    buf[14] = 1; // version minor1
    buf[15] = 2; // version major -> v2 container
    buf[36..40].copy_from_slice(&sprite_offset.to_le_bytes());
    buf[40..44].copy_from_slice(&1u32.to_le_bytes()); // num_sprites
    buf[44..48].copy_from_slice(&palette_offset.to_le_bytes());
    buf[48..52].copy_from_slice(&1u32.to_le_bytes()); // num_palettes
    buf[52..56].copy_from_slice(&ldata_offset.to_le_bytes());
    buf[56..60].copy_from_slice(&ldata_length.to_le_bytes());
    buf[60..64].copy_from_slice(&tdata_offset.to_le_bytes());
    buf[64..68].copy_from_slice(&tdata_length.to_le_bytes());

    // --- Sprite sub-header (28 bytes) at 512: a 3x2 RLE5 sprite living in TData ---
    let s = sprite_offset as usize;
    buf[s..s + 2].copy_from_slice(&0u16.to_le_bytes()); // group
    buf[s + 2..s + 4].copy_from_slice(&0u16.to_le_bytes()); // image
    buf[s + 4..s + 6].copy_from_slice(&3u16.to_le_bytes()); // width = 3
    buf[s + 6..s + 8].copy_from_slice(&2u16.to_le_bytes()); // height = 2 (3*2 = 6 px)
    buf[s + 8..s + 10].copy_from_slice(&0i16.to_le_bytes()); // axis_x
    buf[s + 10..s + 12].copy_from_slice(&0i16.to_le_bytes()); // axis_y
    buf[s + 12..s + 14].copy_from_slice(&0u16.to_le_bytes()); // linked_index = self
    buf[s + 14] = 3; // format = RLE5
    buf[s + 15] = 8; // color_depth
    buf[s + 16..s + 20].copy_from_slice(&0u32.to_le_bytes()); // data_offset within TData
    buf[s + 20..s + 24].copy_from_slice(&tdata_length.to_le_bytes()); // data_length
    buf[s + 24..s + 26].copy_from_slice(&0u16.to_le_bytes()); // palette_index
    buf[s + 26..s + 28].copy_from_slice(&1u16.to_le_bytes()); // flags: bit0 = use TData

    // --- Palette sub-header (16 bytes) at 540 ---
    let p = palette_offset as usize;
    buf[p..p + 2].copy_from_slice(&1u16.to_le_bytes()); // group
    buf[p + 2..p + 4].copy_from_slice(&1u16.to_le_bytes()); // item
    buf[p + 4..p + 6].copy_from_slice(&256u16.to_le_bytes()); // num_colors
    buf[p + 6..p + 8].copy_from_slice(&0u16.to_le_bytes()); // linked_index = self
    buf[p + 8..p + 12].copy_from_slice(&0u32.to_le_bytes()); // data_offset in LData
    buf[p + 12..p + 16].copy_from_slice(&ldata_length.to_le_bytes()); // data_length

    // --- LData palette (RGB triples) at 556: give colours 3/5/7 distinct reds ---
    let l = ldata_offset as usize;
    buf[l + 3 * 3] = 0x30; // colour index 3, R channel
    buf[l + 5 * 3] = 0x50; // colour index 5, R channel
    buf[l + 7 * 3] = 0x70; // colour index 7, R channel

    // --- TData: RLE5 codec stream at 1324 ---
    let t = tdata_offset as usize;
    buf[t..t + rle5.len()].copy_from_slice(&rle5);

    buf
}

/// Real-content RLE5 coverage. Because `kfm.sff` ships zero RLE5 sprites, this
/// synthesizes a genuine SFF v2 file containing one and decodes it end-to-end
/// through the public on-disk API (`read` -> detect -> parse -> RLE5 decode),
/// matching the genuine-asset tests above. It never skips: the fixture is
/// generated at runtime, so the RLE5 codec always has a full-pipeline guard.
#[test]
fn synthetic_rle5_sff_decodes_end_to_end() {
    let bytes = synthesize_rle5_sff();

    // Exercise the real file-loading path, not just in-memory parsing. A unique
    // per-process name avoids collisions when tests run concurrently.
    let path = std::env::temp_dir().join(format!("fp_formats_rle5_{}.sff", std::process::id()));
    std::fs::write(&path, &bytes).expect("write synthetic RLE5 SFF fixture");

    let loaded = SffFile::load(&path);
    // Remove the temp file before asserting so a failure never leaks it.
    let _ = std::fs::remove_file(&path);
    let sff = loaded.expect("synthetic RLE5 SFF should load");

    assert_eq!(sff.version, SffVersion::V2, "fixture is an SFF v2 container");
    assert_eq!(sff.sprites.len(), 1, "fixture declares exactly one sprite");

    let sprite = &sff.sprites[0];
    assert_eq!(
        sprite.format,
        SpriteFormat::Rle5,
        "the fixture's sprite must be RLE5-encoded"
    );
    let expected_px = sprite.width as usize * sprite.height as usize;
    assert_eq!(expected_px, 6, "fixture sprite is 3x2");

    let pixels = sff.decode_sprite(0).expect("RLE5 sprite should decode");
    assert_eq!(
        pixels.len(),
        expected_px,
        "decoded pixel count must equal width*height"
    );
    assert_eq!(
        pixels,
        vec![5, 3, 3, 7, 7, 7],
        "RLE5 stream must decode to the hand-traced palette indices"
    );
}
