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
fn kfm_sff_v1_loads_with_palettes() {
    // intro.sff / ending.sff are SFF v1: each inline PCX image carries a trailing
    // 256-colour VGA palette. The v1 loader must extract those into `palettes`
    // (gap #25) so sprites render with real colours instead of invisibly.
    let path = require("intro.sff").or_else(|| require("ending.sff"));
    let Some(path) = path else {
        return;
    };

    let sff = SffFile::load(&path).expect("an SFF v1 file should load");
    assert_eq!(sff.version, SffVersion::V1, "fixture is an SFF v1 file");

    // The trailing-PCX-palette extraction must populate the palette table.
    assert!(
        !sff.palettes.is_empty(),
        "SFF v1 loader should extract per-sprite trailing PCX palettes"
    );

    // Every sprite's palette_index must resolve to a real palette, and the
    // resolved RGBA must contain at least one non-transparent, non-black colour —
    // i.e. genuine VGA palette data, not an all-zero default.
    let mut saw_real_colour = false;
    for sprite in &sff.sprites {
        let rgba = sff
            .palette(sprite.palette_index as usize)
            .expect("each v1 sprite's palette_index must resolve");
        // Look past index 0 (always transparent) for any opaque coloured entry.
        if rgba
            .chunks_exact(4)
            .skip(1)
            .any(|px| px[3] != 0 && (px[0] != 0 || px[1] != 0 || px[2] != 0))
        {
            saw_real_colour = true;
        }
    }
    assert!(
        saw_real_colour,
        "at least one extracted SFF v1 palette should carry real (non-black) colours"
    );

    // Regression guard for the inverted byte-18 shared-palette semantic
    // (gap #25): SFF v1 sprites each carry their OWN distinct trailing VGA
    // palette, even though real WinMUGEN content sets the byte-18 "shared" flag on
    // nearly every sprite. The loader must therefore extract one DISTINCT palette
    // per data-owning sprite — not collapse them onto a handful. (The previous,
    // inverted code kept only the ~1-2 byte-18==0 sprites' palettes and forced all
    // the rest onto a single wrong palette.)
    //
    // Count sprites that own pixel data and whose `palette_index` resolves to a
    // *real* (non-empty) palette entry, then count how many DISTINCT resolved
    // palettes those produce. The two counts must match: every such sprite has its
    // own palette.
    let mut data_owning_with_real_palette = 0usize;
    let mut distinct_palette_colours: std::collections::HashSet<[u8; 1024]> =
        std::collections::HashSet::new();
    for sprite in &sff.sprites {
        if sprite.data_length == 0 {
            continue; // linked/data-less sprite: legitimately reuses a palette
        }
        let entry = &sff.palettes[sprite.palette_index as usize];
        if entry.data_length == 0 {
            continue; // resolved to the safe zeroed default, not a real palette
        }
        data_owning_with_real_palette += 1;
        let rgba = sff
            .palette(sprite.palette_index as usize)
            .expect("data-owning v1 sprite's palette must resolve");
        distinct_palette_colours.insert(rgba);
    }

    assert!(
        data_owning_with_real_palette >= 2,
        "the v1 fixture should have several data-owning sprites with real palettes \
         (got {data_owning_with_real_palette})"
    );
    // The headline assertion: distinct palettes == data-owning sprites with real
    // palettes. If the byte-18 semantic were inverted, this would be far smaller
    // (a couple) than the sprite count.
    assert_eq!(
        distinct_palette_colours.len(),
        data_owning_with_real_palette,
        "each data-owning SFF v1 sprite must resolve to its OWN distinct trailing \
         palette ({} distinct palettes for {data_owning_with_real_palette} sprites)",
        distinct_palette_colours.len()
    );

    // Belt-and-braces: two specific sprites that each own pixel+palette data must
    // resolve to DIFFERENT palette colours (they would be identical under the
    // inverted semantic that forced both onto sprite 0's palette).
    let data_owning_indices: Vec<usize> = (0..sff.sprites.len())
        .filter(|&i| {
            sff.sprites[i].data_length > 0
                && sff.palettes[sff.sprites[i].palette_index as usize].data_length > 0
        })
        .collect();
    assert!(
        data_owning_indices.len() >= 2,
        "need at least two data-owning sprites to compare palettes"
    );
    let pal_a = sff
        .palette(sff.sprites[data_owning_indices[0]].palette_index as usize)
        .expect("palette A resolves");
    let pal_b = sff
        .palette(sff.sprites[data_owning_indices[1]].palette_index as usize)
        .expect("palette B resolves");
    assert_ne!(
        pal_a, pal_b,
        "two distinct data-owning v1 sprites must resolve to different palettes \
         (they were wrongly identical under the inverted byte-18 semantic)"
    );
    // Their palette_index values must also differ (each owns its own entry).
    assert_ne!(
        sff.sprites[data_owning_indices[0]].palette_index,
        sff.sprites[data_owning_indices[1]].palette_index,
        "two data-owning v1 sprites must point at distinct palette entries"
    );

    // End to end: at least one sprite must decode to RGBA pixels that are not
    // entirely transparent, proving indices + palette combine into a visible
    // sprite (the gap-#25 regression: previously every v1 sprite was invisible).
    let any_visible = (0..sff.sprites.len())
        .filter_map(|i| sff.decode_sprite_rgba(i).ok())
        .any(|rgba| rgba.chunks_exact(4).any(|px| px[3] != 0));
    assert!(
        any_visible,
        "at least one SFF v1 sprite should decode to visible (opaque) RGBA pixels"
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
    assert!(
        s0.starts_with(b"RIFF"),
        "sound (0, 0) should be a RIFF blob"
    );

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
    assert!(
        s1.starts_with(b"RIFF"),
        "sound (0, 1) should be a RIFF blob"
    );

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

    assert_eq!(
        sff.version,
        SffVersion::V2,
        "fixture is an SFF v2 container"
    );
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

/// Builds a complete SFF v2 file holding a single PNG8 (indexed) sprite.
///
/// The TData block carries a real PNG datastream (encoded with the `png` crate):
/// a 2x2 indexed image whose palette is `[black, red, green, blue]` and whose
/// pixels are indices `[1, 2, 3, 1]`. The SFF palette table also carries a
/// 768-byte LData palette, but PNG8 decoding uses the PNG's *embedded* `PLTE`.
fn synthesize_png8_sff() -> Vec<u8> {
    // Encode the indexed PNG payload.
    let plte: [u8; 12] = [
        0, 0, 0, // 0 transparent slot
        255, 0, 0, // 1 red
        0, 255, 0, // 2 green
        0, 0, 255, // 3 blue
    ];
    let indices: [u8; 4] = [1, 2, 3, 1];
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, 2, 2);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(plte.to_vec());
        let mut writer = encoder.write_header().expect("write PNG header");
        writer
            .write_image_data(&indices)
            .expect("write PNG indices");
    }

    let sprite_offset: u32 = 512;
    let palette_offset: u32 = 540;
    let ldata_offset: u32 = 556;
    let ldata_length: u32 = 768;
    let tdata_offset: u32 = ldata_offset + ldata_length;
    let tdata_length: u32 = png_bytes.len() as u32;

    let total = tdata_offset as usize + tdata_length as usize;
    let mut buf = vec![0u8; total];

    buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
    buf[14] = 1;
    buf[15] = 2; // v2 container
    buf[36..40].copy_from_slice(&sprite_offset.to_le_bytes());
    buf[40..44].copy_from_slice(&1u32.to_le_bytes());
    buf[44..48].copy_from_slice(&palette_offset.to_le_bytes());
    buf[48..52].copy_from_slice(&1u32.to_le_bytes());
    buf[52..56].copy_from_slice(&ldata_offset.to_le_bytes());
    buf[56..60].copy_from_slice(&ldata_length.to_le_bytes());
    buf[60..64].copy_from_slice(&tdata_offset.to_le_bytes());
    buf[64..68].copy_from_slice(&tdata_length.to_le_bytes());

    // Sprite sub-header: 2x2 PNG8 living in TData.
    let s = sprite_offset as usize;
    buf[s + 4..s + 6].copy_from_slice(&2u16.to_le_bytes()); // width
    buf[s + 6..s + 8].copy_from_slice(&2u16.to_le_bytes()); // height
    buf[s + 14] = 10; // format = PNG8
    buf[s + 15] = 8; // color_depth
    buf[s + 16..s + 20].copy_from_slice(&0u32.to_le_bytes()); // data_offset in TData
    buf[s + 20..s + 24].copy_from_slice(&tdata_length.to_le_bytes());
    buf[s + 24..s + 26].copy_from_slice(&0u16.to_le_bytes()); // palette_index
    buf[s + 26..s + 28].copy_from_slice(&1u16.to_le_bytes()); // flags: bit0 -> TData

    // Palette sub-header (a valid SFF palette so palette(0) resolves too).
    let p = palette_offset as usize;
    buf[p + 4..p + 6].copy_from_slice(&256u16.to_le_bytes());
    buf[p + 8..p + 12].copy_from_slice(&0u32.to_le_bytes());
    buf[p + 12..p + 16].copy_from_slice(&ldata_length.to_le_bytes());

    // TData: the PNG datastream.
    let t = tdata_offset as usize;
    buf[t..t + png_bytes.len()].copy_from_slice(&png_bytes);

    buf
}

/// End-to-end PNG8 coverage (gap #35). Synthesizes an SFF v2 file containing one
/// indexed PNG sprite and decodes it through the public on-disk API: indices via
/// `decode_sprite`, and full RGBA (using the PNG's embedded PLTE) via
/// `decode_sprite_rgba`. Always runs — the fixture is generated at runtime.
#[test]
fn synthetic_png8_sff_decodes_end_to_end() {
    let bytes = synthesize_png8_sff();

    let path = std::env::temp_dir().join(format!("fp_formats_png8_{}.sff", std::process::id()));
    std::fs::write(&path, &bytes).expect("write synthetic PNG8 SFF fixture");
    let loaded = SffFile::load(&path);
    let _ = std::fs::remove_file(&path);
    let sff = loaded.expect("synthetic PNG8 SFF should load");

    assert_eq!(sff.version, SffVersion::V2);
    assert_eq!(sff.sprites.len(), 1);
    assert_eq!(sff.sprites[0].format, SpriteFormat::Png8);

    // Indexed pixels flow through the standard index path.
    let indices = sff
        .decode_sprite(0)
        .expect("PNG8 sprite should decode to indices");
    assert_eq!(indices, vec![1, 2, 3, 1]);

    // RGBA uses the PNG's embedded palette: red, green, blue, red.
    let rgba = sff
        .decode_sprite_rgba(0)
        .expect("PNG8 sprite should decode to RGBA");
    assert_eq!(rgba.len(), 4 * 4);
    assert_eq!(&rgba[0..4], &[255, 0, 0, 255], "pixel 0 -> red");
    assert_eq!(&rgba[4..8], &[0, 255, 0, 255], "pixel 1 -> green");
    assert_eq!(&rgba[8..12], &[0, 0, 255, 255], "pixel 2 -> blue");
    assert_eq!(&rgba[12..16], &[255, 0, 0, 255], "pixel 3 -> red");
}

/// Builds a complete SFF v2 file holding a single truecolor PNG sprite.
///
/// `format_byte` selects PNG24 (11, RGB source) or PNG32 (12, RGBA source). The
/// TData block carries a real PNG datastream encoded with the `png` crate. This
/// drives the `format byte 11/12 -> DecodedPng::TrueColor -> decode_sprite_rgba`
/// wiring in `sff::mod` end to end (not just at the `decode_png` unit level).
fn synthesize_truecolor_sff(format_byte: u8, rgba_in: bool) -> Vec<u8> {
    // Encode a 1x2 truecolor PNG: a red pixel over a semi-transparent blue pixel.
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, 1, 2);
        if rgba_in {
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("write PNG header");
            writer
                .write_image_data(&[255, 0, 0, 255, 0, 0, 255, 128])
                .expect("write RGBA pixels");
        } else {
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("write PNG header");
            writer
                .write_image_data(&[255, 0, 0, 0, 0, 255])
                .expect("write RGB pixels");
        }
    }

    let sprite_offset: u32 = 512;
    let palette_offset: u32 = 540;
    let ldata_offset: u32 = 556;
    let ldata_length: u32 = 768;
    let tdata_offset: u32 = ldata_offset + ldata_length;
    let tdata_length: u32 = png_bytes.len() as u32;

    let total = tdata_offset as usize + tdata_length as usize;
    let mut buf = vec![0u8; total];

    buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
    buf[14] = 1;
    buf[15] = 2; // v2 container
    buf[36..40].copy_from_slice(&sprite_offset.to_le_bytes());
    buf[40..44].copy_from_slice(&1u32.to_le_bytes());
    buf[44..48].copy_from_slice(&palette_offset.to_le_bytes());
    buf[48..52].copy_from_slice(&1u32.to_le_bytes());
    buf[52..56].copy_from_slice(&ldata_offset.to_le_bytes());
    buf[56..60].copy_from_slice(&ldata_length.to_le_bytes());
    buf[60..64].copy_from_slice(&tdata_offset.to_le_bytes());
    buf[64..68].copy_from_slice(&tdata_length.to_le_bytes());

    // Sprite sub-header: 1x2 truecolor PNG living in TData.
    let s = sprite_offset as usize;
    buf[s + 4..s + 6].copy_from_slice(&1u16.to_le_bytes()); // width
    buf[s + 6..s + 8].copy_from_slice(&2u16.to_le_bytes()); // height
    buf[s + 14] = format_byte; // 11 = PNG24, 12 = PNG32
    buf[s + 15] = if rgba_in { 32 } else { 24 }; // color_depth (advisory)
    buf[s + 16..s + 20].copy_from_slice(&0u32.to_le_bytes()); // data_offset in TData
    buf[s + 20..s + 24].copy_from_slice(&tdata_length.to_le_bytes());
    buf[s + 24..s + 26].copy_from_slice(&0u16.to_le_bytes()); // palette_index
    buf[s + 26..s + 28].copy_from_slice(&1u16.to_le_bytes()); // flags: bit0 -> TData

    // Palette sub-header (present but unused by truecolor sprites).
    let p = palette_offset as usize;
    buf[p + 4..p + 6].copy_from_slice(&256u16.to_le_bytes());
    buf[p + 8..p + 12].copy_from_slice(&0u32.to_le_bytes());
    buf[p + 12..p + 16].copy_from_slice(&ldata_length.to_le_bytes());

    // TData: the PNG datastream.
    let t = tdata_offset as usize;
    buf[t..t + png_bytes.len()].copy_from_slice(&png_bytes);

    buf
}

/// End-to-end PNG24 coverage (gap #35, truecolor). Synthesizes an SFF v2 file
/// with one PNG24 (RGB) sprite and decodes it through the public on-disk
/// `decode_sprite_rgba` path, asserting the `format byte 11 -> TrueColor` wiring
/// in `sff::mod`. PNG24 has no alpha, so every pixel must come back fully opaque.
#[test]
fn synthetic_png24_sff_decodes_to_rgba_end_to_end() {
    let bytes = synthesize_truecolor_sff(11, false);

    let path = std::env::temp_dir().join(format!("fp_formats_png24_{}.sff", std::process::id()));
    std::fs::write(&path, &bytes).expect("write synthetic PNG24 SFF fixture");
    let loaded = SffFile::load(&path);
    let _ = std::fs::remove_file(&path);
    let sff = loaded.expect("synthetic PNG24 SFF should load");

    assert_eq!(sff.version, SffVersion::V2);
    assert_eq!(sff.sprites.len(), 1);
    assert_eq!(sff.sprites[0].format, SpriteFormat::Png24);

    // Truecolor sprites carry no palette indices: the index path must refuse them
    // with a recoverable error (never silently mis-handle them).
    assert!(
        sff.decode_sprite(0).is_err(),
        "PNG24 truecolor sprite has no indices for the index path"
    );

    // The RGBA path surfaces the truecolor pixels directly, with opaque alpha.
    let rgba = sff
        .decode_sprite_rgba(0)
        .expect("PNG24 sprite should decode to RGBA");
    // 1x2 sprite at 4 bytes/pixel (RGBA) = 8 bytes.
    assert_eq!(rgba.len(), 8);
    assert_eq!(&rgba[0..4], &[255, 0, 0, 255], "pixel 0 -> opaque red");
    assert_eq!(&rgba[4..8], &[0, 0, 255, 255], "pixel 1 -> opaque blue");
}

/// End-to-end PNG32 coverage (gap #35, truecolor + alpha). Mirrors the PNG24 test
/// for the `format byte 12 -> TrueColor` path, preserving the source alpha.
#[test]
fn synthetic_png32_sff_decodes_to_rgba_end_to_end() {
    let bytes = synthesize_truecolor_sff(12, true);

    let path = std::env::temp_dir().join(format!("fp_formats_png32_{}.sff", std::process::id()));
    std::fs::write(&path, &bytes).expect("write synthetic PNG32 SFF fixture");
    let loaded = SffFile::load(&path);
    let _ = std::fs::remove_file(&path);
    let sff = loaded.expect("synthetic PNG32 SFF should load");

    assert_eq!(sff.version, SffVersion::V2);
    assert_eq!(sff.sprites.len(), 1);
    assert_eq!(sff.sprites[0].format, SpriteFormat::Png32);

    assert!(
        sff.decode_sprite(0).is_err(),
        "PNG32 truecolor sprite has no indices for the index path"
    );

    // The RGBA path preserves the source alpha (the blue pixel is semi-transparent).
    let rgba = sff
        .decode_sprite_rgba(0)
        .expect("PNG32 sprite should decode to RGBA");
    // 1x2 sprite at 4 bytes/pixel (RGBA) = 8 bytes.
    assert_eq!(rgba.len(), 8);
    assert_eq!(&rgba[0..4], &[255, 0, 0, 255], "pixel 0 -> opaque red");
    assert_eq!(
        &rgba[4..8],
        &[0, 0, 255, 128],
        "pixel 1 -> semi-transparent blue"
    );
}
