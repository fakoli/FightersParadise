//! Conformance test for the shipped, ORIGINAL HUD bitmap font asset (FL2b).
//!
//! `assets/data/font.fnt` is a clean-room MUGEN FNT v1 font, authored from
//! scratch for this engine (MIT — no Elecbyte/MUGEN content; the only ASCII run
//! in the file is the required `ElecbyteFnt` format magic plus the font's own
//! original `[Def]`/`[Map]` config). Like the Training Dummy character and the
//! fightfx effects, it **ships in the repository**, so this test is NOT
//! asset-gated — it must load and parse on every machine and on CI.
//!
//! If you regenerate the HUD font, this is the test that proves the bytes still
//! parse, every HUD glyph is mapped, and the clean-room contract holds.

use std::path::{Path, PathBuf};

use fp_formats::fnt::FntFont;

/// Resolves a path inside the workspace `assets/data/` directory.
///
/// Integration tests run with the *crate* directory as the manifest root
/// (`crates/fp-formats`), so go up two levels to the workspace root.
fn font_asset(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/data")
        .join(rel)
}

/// The exact characters the HUD needs (the strings `KO`, `ROUND`, `WINS`,
/// `DRAW`, `P1`, `P2`, plus all digits for the timer/round number).
const REQUIRED_CHARS: &[char] = &[
    ' ', ':', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F', 'G',
    'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];

#[test]
fn hud_font_file_exists_in_repo() {
    let p = font_asset("font.fnt");
    assert!(
        p.exists(),
        "shipped HUD font missing: {} — it must be committed",
        p.display()
    );
}

#[test]
fn hud_font_loads_and_parses_as_v1() {
    let font = FntFont::load(&font_asset("font.fnt"))
        .unwrap_or_else(|e| panic!("shipped font.fnt failed to load: {e}"));

    // A real glyph strip with a non-zero size.
    assert!(font.image_width > 0, "font strip must have width");
    assert!(font.image_height > 0, "font strip must have height");
    assert_eq!(
        font.pixels.len(),
        font.image_width as usize * font.image_height as usize,
        "decoded strip pixel count must match its declared dimensions"
    );
    // A 1024-byte RGBA palette with an opaque ink color at index 1.
    assert_eq!(font.palette.len(), 1024, "palette is 256 RGBA entries");
    assert_eq!(font.palette[3], 0, "palette index 0 must be transparent");
    assert_eq!(font.palette[7], 255, "palette index 1 (ink) must be opaque");
}

#[test]
fn hud_font_maps_every_required_glyph() {
    let font = FntFont::load(&font_asset("font.fnt"))
        .unwrap_or_else(|e| panic!("shipped font.fnt failed to load: {e}"));

    for &c in REQUIRED_CHARS {
        let g = font
            .glyph(c)
            .unwrap_or_else(|| panic!("HUD font missing glyph for {c:?}"));
        assert!(g.width > 0, "glyph {c:?} must have a non-zero width");
        // The glyph column must lie inside the strip.
        assert!(
            (g.x as usize + g.width as usize) <= font.image_width as usize,
            "glyph {c:?} column [{}..{}] runs past the {}px strip",
            g.x,
            g.x + g.width,
            font.image_width
        );
    }
}

#[test]
fn hud_font_has_visible_ink() {
    // At least some glyph pixels must be ink (index 1) — a font of all-transparent
    // glyphs would render nothing.
    let font = FntFont::load(&font_asset("font.fnt"))
        .unwrap_or_else(|e| panic!("shipped font.fnt failed to load: {e}"));
    let ink = font.pixels.iter().filter(|&&p| p == 1).count();
    assert!(ink > 0, "HUD font strip must contain visible ink pixels");
}

#[test]
fn hud_font_is_clean_room_only_magic_and_config_ascii() {
    // Clean-room guard: the ONLY name-like ASCII run permitted is the required
    // `ElecbyteFnt` format magic. The remaining ASCII runs are the font's own
    // original `[Def]`/`[Map]` INI config (standard FNT v1 keywords + numeric
    // glyph-map entries), NOT any copyrighted string (author names, tool banners,
    // Elecbyte data). We assert the magic is the first run and that every other
    // run is drawn only from the allowed config vocabulary.
    let bytes = std::fs::read(font_asset("font.fnt")).expect("read font.fnt");
    let mut runs: Vec<String> = Vec::new();
    let mut cur = String::new();
    for &b in &bytes {
        if b.is_ascii_graphic() || b == b' ' {
            cur.push(b as char);
        } else if cur.len() >= 4 {
            runs.push(std::mem::take(&mut cur));
        } else {
            cur.clear();
        }
    }
    if cur.len() >= 4 {
        runs.push(cur);
    }

    assert!(
        !runs.is_empty(),
        "font.fnt must contain the ElecbyteFnt magic"
    );
    assert_eq!(
        runs[0], "ElecbyteFnt",
        "the first ASCII run must be the required FNT magic, found {:?}",
        runs[0]
    );

    // Every other run must be an `[...]` section header, a `Key = value` config
    // line built from the allowed keywords, or a numeric `[Map]` entry
    // (`code x width`). No free-form copyrighted text may appear.
    const ALLOWED_KEYS: &[&str] = &["type", "def", "spacing", "colors", "size", "offset"];
    for run in &runs[1..] {
        let r = run.trim();
        if r.starts_with('[') && r.ends_with(']') {
            continue; // section header, e.g. [Def] / [Map]
        }
        if let Some((key, _value)) = r.split_once('=') {
            assert!(
                ALLOWED_KEYS.contains(&key.trim().to_ascii_lowercase().as_str()),
                "unexpected config key in font.fnt: {run:?}"
            );
            continue;
        }
        // Otherwise it must be a numeric [Map] entry: all whitespace-separated
        // tokens parse as integers (the char code + x + width).
        let all_numeric = r.split_whitespace().all(|tok| tok.parse::<i64>().is_ok());
        assert!(
            all_numeric,
            "unexpected non-numeric ASCII run in font.fnt (possible leaked text): {run:?}"
        );
    }
}
