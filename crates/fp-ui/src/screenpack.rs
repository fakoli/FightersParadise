//! Typed `fight.def` screenpack model + parser.
//!
//! A MUGEN screenpack's `fight.def` (sometimes called the "motif" or "system"
//! fight file) is an INI-style file describing the in-fight HUD: which sprite
//! file (`fight.sff`) and fonts to load, and where the life bars, power bars,
//! round/time/combo counters, fighter names, and portraits sit on screen.
//!
//! This module parses that file — via the shared [`fp_formats::def::DefFile`]
//! INI reader, so it inherits BOM/CRLF tolerance, `;` comment stripping, and the
//! split-on-first-`=` rule — into a typed [`ScreenpackLayout`]. Parsing is pure
//! (no GPU, no I/O beyond the `DefFile` read) and **never panics**: an unknown
//! key is ignored, a malformed value falls back to a sensible default with a
//! `tracing::warn!`, and a missing section yields that section's defaults.
//!
//! The GPU-side renderer that consumes a [`ScreenpackLayout`] to issue draw
//! calls lives in [`crate::renderer`].
//!
//! # Sections modelled
//!
//! - `[Files]` — the `sff` sprite file and `font0..fontN` font files.
//! - `[Lifebar]` — per-player `p1`/`p2` background / middle / front life-bar
//!   sprites + positions, and the `range` (pixel span the full bar covers).
//! - `[Powerbar]` — per-player power-bar sprites + positions + level-up sounds.
//! - `[Round]` / `[Time]` / `[Combo]` — counter text/sprite positions + fonts.
//! - `[Name]` — per-player fighter-name text positions + fonts.
//! - `[Face]` — per-player portrait sprite + position.

use fp_formats::def::DefFile;

/// A 2D pixel position parsed from a `x, y` pair (MUGEN screen coordinates,
/// origin top-left).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos {
    /// X offset in screen pixels.
    pub x: i32,
    /// Y offset in screen pixels.
    pub y: i32,
}

impl Pos {
    /// A position at `(x, y)`.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A sprite reference: a `(group, image)` pair into the screenpack's
/// `fight.sff`, optionally placed at an offset.
///
/// MUGEN screenpack keys like `p1.bg0.spr = 0,0` and `p1.bg0.offset = 27,14`
/// pair a sprite id with a placement offset; this groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpriteRef {
    /// Sprite group number in `fight.sff`.
    pub group: u16,
    /// Image number within the group.
    pub image: u16,
    /// Pixel offset at which to draw the sprite.
    pub offset: Pos,
}

/// One player's life-bar layout: layered background / middle / front sprites,
/// the anchor position, and the pixel `range` the full (100%) bar spans.
///
/// MUGEN layers a life bar as one or more `bg0..bgN` (static backing) → `mid`
/// (a damage "ghost" that lags) → `front` (the live fill clipped to the current
/// life fraction). The background is a *stack*: MUGEN screenpacks routinely
/// author `bg0`, `bg1`, … and the engine paints them in slot order (bg0 first,
/// the highest `bgN` last), so [`bg_layers`](Self::bg_layers) holds all of them
/// rather than just `bg0`.
///
/// `range` is `(x0, x1)`: the front fill spans `[x0, x1]` at full life and
/// shrinks toward `x0` as life drops (toward `x1` for a right-anchored P2 bar —
/// the renderer decides anchoring from the player side).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LifebarSide {
    /// Static background sprites in `bg0..bgN` slot order, drawn first (bg0 at
    /// the back, the last entry just under `mid`). Empty when none are authored.
    pub bg_layers: Vec<SpriteRef>,
    /// Mid / "ghost" damage sprite (drawn over the background).
    pub mid: Option<SpriteRef>,
    /// Front live-fill sprite (clipped to the life fraction).
    pub front: Option<SpriteRef>,
    /// Anchor position of the whole bar group.
    pub pos: Pos,
    /// `(x0, x1)` pixel span of the front fill at full life.
    pub range: (i32, i32),
}

/// One player's power-bar layout (same layered structure as a life bar) plus the
/// per-level "meter filled" sounds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PowerbarSide {
    /// Static background sprites in `bg0..bgN` slot order (drawn back-to-front),
    /// mirroring [`LifebarSide::bg_layers`]. Empty when none are authored.
    pub bg_layers: Vec<SpriteRef>,
    /// Mid sprite.
    pub mid: Option<SpriteRef>,
    /// Front live-fill sprite (clipped to the power fraction).
    pub front: Option<SpriteRef>,
    /// Anchor position of the whole bar group.
    pub pos: Pos,
    /// `(x0, x1)` pixel span of the front fill at full power.
    pub range: (i32, i32),
    /// `(group, sample)` sound pairs played when each power level fills, indexed
    /// by level (`level1`, `level2`, …). Empty when none are authored.
    pub level_sounds: Vec<(i32, i32)>,
}

/// A text element placement: an anchor position and the font slot (`font0..`,
/// the index into `[Files] font0..fontN`) used to render it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TextElem {
    /// Anchor position of the text.
    pub pos: Pos,
    /// Index of the font (into [`ScreenpackLayout::fonts`]) to draw with.
    pub font: usize,
}

/// The round-counter / "Round 1" / "Fight" / "KO" announcer placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RoundLayout {
    /// Position of the round-number / announcement text.
    pub pos: Pos,
    /// Font slot used for the announcement.
    pub font: usize,
}

/// The fight timer (clock) placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeLayout {
    /// Position of the timer text.
    pub pos: Pos,
    /// Font slot used for the timer.
    pub font: usize,
}

/// The combo-counter placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ComboLayout {
    /// Position of the combo-count text (P1's side).
    pub pos: Pos,
    /// Font slot used for the combo count.
    pub font: usize,
}

/// One player's name-text placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NameSide {
    /// Position of the name text.
    pub pos: Pos,
    /// Font slot used for the name.
    pub font: usize,
}

/// One player's portrait ("face") placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FaceSide {
    /// The portrait sprite (group, image) + offset.
    pub spr: Option<SpriteRef>,
    /// Position of the portrait.
    pub pos: Pos,
}

/// A fully parsed `fight.def` screenpack layout.
///
/// Holds the file references ([`sff`](Self::sff), [`fonts`](Self::fonts)) and
/// every HUD element's typed placement. Build one with [`ScreenpackLayout::parse`]
/// from a [`DefFile`]. All fields carry [`Default`]s, so a partial or malformed
/// `fight.def` still yields a usable (if sparse) layout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScreenpackLayout {
    /// The `fight.sff` sprite file path, relative to the `fight.def` directory
    /// (from `[Files] sff`). Empty if unspecified.
    pub sff: String,
    /// Font file paths from `[Files] font0..fontN`, in slot order. `fonts[0]` is
    /// `font0`, etc. Gaps (a missing `fontN`) are filled with an empty string so
    /// slot indices stay stable.
    pub fonts: Vec<String>,
    /// P1's life-bar layout.
    pub p1_lifebar: LifebarSide,
    /// P2's life-bar layout.
    pub p2_lifebar: LifebarSide,
    /// P1's power-bar layout.
    pub p1_powerbar: PowerbarSide,
    /// P2's power-bar layout.
    pub p2_powerbar: PowerbarSide,
    /// Round announcer / counter placement.
    pub round: RoundLayout,
    /// Fight timer placement.
    pub time: TimeLayout,
    /// Combo counter placement.
    pub combo: ComboLayout,
    /// P1's name-text placement.
    pub p1_name: NameSide,
    /// P2's name-text placement.
    pub p2_name: NameSide,
    /// P1's portrait placement.
    pub p1_face: FaceSide,
    /// P2's portrait placement.
    pub p2_face: FaceSide,
}

impl ScreenpackLayout {
    /// Parses a [`DefFile`] (already read from a `fight.def`) into a typed layout.
    ///
    /// Tolerant by design: unknown sections/keys are ignored, malformed numeric
    /// values fall back to defaults (with a `tracing::warn!`), and absent
    /// sections yield that section's [`Default`]. Never panics.
    pub fn parse(def: &DefFile) -> Self {
        Self {
            sff: def.get("Files", "sff").unwrap_or("").to_string(),
            fonts: parse_fonts(def),
            p1_lifebar: parse_lifebar_side(def, "Lifebar", "p1"),
            p2_lifebar: parse_lifebar_side(def, "Lifebar", "p2"),
            p1_powerbar: parse_powerbar_side(def, "Powerbar", "p1"),
            p2_powerbar: parse_powerbar_side(def, "Powerbar", "p2"),
            round: RoundLayout {
                pos: parse_pos(def, "Round", "pos").unwrap_or_default(),
                font: parse_font_slot(def, "Round", "font"),
            },
            time: TimeLayout {
                pos: parse_pos(def, "Time", "pos").unwrap_or_default(),
                font: parse_font_slot(def, "Time", "font"),
            },
            combo: ComboLayout {
                pos: parse_pos(def, "Combo", "pos").unwrap_or_default(),
                font: parse_font_slot(def, "Combo", "font"),
            },
            p1_name: parse_name_side(def, "Name", "p1"),
            p2_name: parse_name_side(def, "Name", "p2"),
            p1_face: parse_face_side(def, "Face", "p1"),
            p2_face: parse_face_side(def, "Face", "p2"),
        }
    }
}

/// Collects `[Files] font0..fontN` into a dense, slot-indexed vector.
///
/// Stops at the first slot `n` with no `fontN` key (MUGEN font slots are
/// contiguous); a higher non-contiguous `fontN` is therefore ignored, which
/// matches how the renderer addresses fonts by contiguous slot index.
fn parse_fonts(def: &DefFile) -> Vec<String> {
    let mut fonts = Vec::new();
    let mut n = 0;
    loop {
        let key = format!("font{n}");
        match def.get("Files", &key) {
            Some(path) if !path.is_empty() => {
                fonts.push(path.to_string());
                n += 1;
            }
            _ => break,
        }
    }
    fonts
}

/// Parses a `[section] prefix.key = x, y` position pair into a [`Pos`].
///
/// Returns `None` if the key is absent. A present-but-malformed value warns and
/// also returns `None` (the caller substitutes a default).
fn parse_pos(def: &DefFile, section: &str, key: &str) -> Option<Pos> {
    let raw = def.get(section, key)?;
    match parse_int_pair(raw) {
        Some((x, y)) => Some(Pos::new(x, y)),
        None => {
            tracing::warn!(
                section,
                key,
                raw,
                "screenpack: malformed position; ignoring"
            );
            None
        }
    }
}

/// Parses a `[section] prefix.key = group, image` sprite reference (without an
/// offset — the offset is read separately and merged by the caller).
fn parse_sprite(def: &DefFile, section: &str, key: &str) -> Option<SpriteRef> {
    let raw = def.get(section, key)?;
    match parse_int_pair(raw) {
        Some((g, i)) => Some(SpriteRef {
            group: clamp_u16(g),
            image: clamp_u16(i),
            offset: Pos::default(),
        }),
        None => {
            tracing::warn!(
                section,
                key,
                raw,
                "screenpack: malformed sprite ref; ignoring"
            );
            None
        }
    }
}

/// Reads a `[section] key = N` font-slot index, defaulting to `0` when absent or
/// malformed. MUGEN font keys are commonly `font = N` or `N, bank, justify`; we
/// take the first integer as the slot.
fn parse_font_slot(def: &DefFile, section: &str, key: &str) -> usize {
    match def.get(section, key) {
        Some(raw) => first_int(raw).map(|n| n.max(0) as usize).unwrap_or(0),
        None => 0,
    }
}

/// Parses one side (`p1`/`p2`) of `[Lifebar]`.
fn parse_lifebar_side(def: &DefFile, section: &str, side: &str) -> LifebarSide {
    LifebarSide {
        bg_layers: parse_bg_layers(def, section, side),
        mid: parse_bar_layer(def, section, side, "mid"),
        front: parse_bar_layer(def, section, side, "front"),
        pos: parse_pos(def, section, &format!("{side}.pos")).unwrap_or_default(),
        range: parse_range(def, section, &format!("{side}.range.x")),
    }
}

/// Parses one side (`p1`/`p2`) of `[Powerbar]`.
fn parse_powerbar_side(def: &DefFile, section: &str, side: &str) -> PowerbarSide {
    PowerbarSide {
        bg_layers: parse_bg_layers(def, section, side),
        mid: parse_bar_layer(def, section, side, "mid"),
        front: parse_bar_layer(def, section, side, "front"),
        pos: parse_pos(def, section, &format!("{side}.pos")).unwrap_or_default(),
        range: parse_range(def, section, &format!("{side}.range.x")),
        level_sounds: parse_level_sounds(def, section, side),
    }
}

/// Collects a bar's contiguous `bg0..bgN` background layers (each with its own
/// `.spr` + optional `.offset`) into z-order — `bg0` first.
///
/// MUGEN background slots are contiguous, so this stops at the first missing
/// `bgN`: a non-contiguous higher slot is ignored (matching how `font0..fontN`
/// slots are collected). The returned vector is empty when no `bg0` is authored.
fn parse_bg_layers(def: &DefFile, section: &str, side: &str) -> Vec<SpriteRef> {
    let mut layers = Vec::new();
    let mut n = 0;
    loop {
        let layer = format!("bg{n}");
        match parse_bar_layer(def, section, side, &layer) {
            Some(spr) => {
                layers.push(spr);
                n += 1;
            }
            None => break,
        }
    }
    layers
}

/// Parses one bar layer's sprite + its `.offset`, merging the offset into the
/// returned [`SpriteRef`]. The sprite key is e.g. `p1.bg0.spr`, the offset key
/// `p1.bg0.offset`.
fn parse_bar_layer(def: &DefFile, section: &str, side: &str, layer: &str) -> Option<SpriteRef> {
    let spr_key = format!("{side}.{layer}.spr");
    let mut spr = parse_sprite(def, section, &spr_key)?;
    if let Some(off) = parse_pos(def, section, &format!("{side}.{layer}.offset")) {
        spr.offset = off;
    }
    Some(spr)
}

/// Parses a `[section] prefix.range.x = x0, x1` fill span, defaulting to `(0, 0)`
/// when absent or malformed.
fn parse_range(def: &DefFile, section: &str, key: &str) -> (i32, i32) {
    match def.get(section, key) {
        Some(raw) => parse_int_pair(raw).unwrap_or_else(|| {
            tracing::warn!(
                section,
                key,
                raw,
                "screenpack: malformed range; defaulting to (0,0)"
            );
            (0, 0)
        }),
        None => (0, 0),
    }
}

/// Collects `p1.levelN.snd = group, sample` power-level sounds in level order.
fn parse_level_sounds(def: &DefFile, section: &str, side: &str) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let mut level = 1;
    loop {
        let key = format!("{side}.level{level}.snd");
        match def.get(section, &key) {
            Some(raw) => {
                if let Some(pair) = parse_int_pair(raw) {
                    out.push(pair);
                } else {
                    tracing::warn!(
                        section,
                        key,
                        raw,
                        "screenpack: malformed level sound; skipping"
                    );
                }
                level += 1;
            }
            None => break,
        }
    }
    out
}

/// Parses one side (`p1`/`p2`) of `[Name]`.
fn parse_name_side(def: &DefFile, section: &str, side: &str) -> NameSide {
    NameSide {
        pos: parse_pos(def, section, &format!("{side}.pos")).unwrap_or_default(),
        font: parse_font_slot(def, section, &format!("{side}.font")),
    }
}

/// Parses one side (`p1`/`p2`) of `[Face]`.
fn parse_face_side(def: &DefFile, section: &str, side: &str) -> FaceSide {
    let mut spr = parse_sprite(def, section, &format!("{side}.spr"));
    if let (Some(s), Some(off)) = (
        spr.as_mut(),
        parse_pos(def, section, &format!("{side}.offset")),
    ) {
        s.offset = off;
    }
    FaceSide {
        spr,
        pos: parse_pos(def, section, &format!("{side}.pos")).unwrap_or_default(),
    }
}

/// Parses a two-integer pair from a comma/whitespace-separated value.
///
/// Returns `None` unless at least two integers are present. Extra tokens are
/// ignored, matching MUGEN's lenient value parsing.
fn parse_int_pair(s: &str) -> Option<(i32, i32)> {
    let mut it = int_tokens(s);
    let a = it.next()?;
    let b = it.next()?;
    Some((a, b))
}

/// The first integer in a comma/whitespace-separated value, or `None`.
fn first_int(s: &str) -> Option<i32> {
    int_tokens(s).next()
}

/// Tokenises `s` on commas/whitespace and parses each token as an `i32`,
/// skipping non-numeric tokens.
fn int_tokens(s: &str) -> impl Iterator<Item = i32> + '_ {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<i32>().ok())
}

/// Clamps a (possibly negative) integer into `u16` range.
fn clamp_u16(v: i32) -> u16 {
    v.clamp(0, u16::MAX as i32) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
; A synthetic fight.def screenpack
[Files]
sff  = fight.sff
font0 = font/f-4x6.fnt
font1 = font/jg.fnt
font2 = font/cbig.fnt

[Lifebar]
p1.pos        = 80, 33
p1.bg0.spr    = 0, 0
p1.bg0.offset = 0, 0
p1.bg1.spr    = 0, 2
p1.bg1.offset = 3, 3
p1.mid.spr    = 1, 0
p1.front.spr  = 2, 0
p1.front.offset = 4, 4
p1.range.x    = 0, 256
p2.pos        = 240, 33
p2.bg0.spr    = 0, 1
p2.front.spr  = 2, 1
p2.range.x    = 0, -256

[Powerbar]
p1.pos        = 80, 200
p1.bg0.spr    = 10, 0
p1.front.spr  = 12, 0
p1.range.x    = 0, 144
p1.level1.snd = 60, 0
p1.level2.snd = 60, 1
p1.level3.snd = 60, 2
p2.pos        = 240, 200
p2.front.spr  = 12, 1
p2.range.x    = 0, -144

[Round]
pos  = 160, 20
font = 2, 0, 0

[Time]
pos  = 160, 16
font = 1

[Combo]
pos  = 30, 80
font = 0

[Name]
p1.pos  = 20, 12
p1.font = 0
p2.pos  = 300, 12
p2.font = 0

[Face]
p1.pos    = 12, 12
p1.spr    = 9000, 0
p1.offset = 1, 1
p2.pos    = 308, 12
p2.spr    = 9000, 0

[UnknownSection]
mystery.key = should be ignored
"#;

    fn sample() -> ScreenpackLayout {
        let def = DefFile::from_str(SAMPLE).unwrap();
        ScreenpackLayout::parse(&def)
    }

    #[test]
    fn parses_files_section() {
        let l = sample();
        assert_eq!(l.sff, "fight.sff");
        assert_eq!(
            l.fonts,
            vec![
                "font/f-4x6.fnt".to_string(),
                "font/jg.fnt".to_string(),
                "font/cbig.fnt".to_string(),
            ]
        );
    }

    #[test]
    fn parses_p1_lifebar_layers_and_positions() {
        let l = sample();
        let lb = &l.p1_lifebar;
        assert_eq!(lb.pos, Pos::new(80, 33));
        // Two background layers authored (bg0, bg1) in z-order.
        assert_eq!(
            lb.bg_layers,
            vec![
                SpriteRef {
                    group: 0,
                    image: 0,
                    offset: Pos::new(0, 0)
                },
                SpriteRef {
                    group: 0,
                    image: 2,
                    offset: Pos::new(3, 3)
                },
            ]
        );
        assert_eq!(
            lb.mid,
            Some(SpriteRef {
                group: 1,
                image: 0,
                offset: Pos::default()
            })
        );
        assert_eq!(
            lb.front,
            Some(SpriteRef {
                group: 2,
                image: 0,
                offset: Pos::new(4, 4)
            })
        );
        assert_eq!(lb.range, (0, 256));
    }

    #[test]
    fn parses_p2_lifebar_with_mirrored_range_and_missing_mid() {
        let l = sample();
        let lb = &l.p2_lifebar;
        assert_eq!(lb.pos, Pos::new(240, 33));
        assert_eq!(
            lb.bg_layers,
            vec![SpriteRef {
                group: 0,
                image: 1,
                offset: Pos::default()
            }],
            "p2 has a single bg0 layer authored"
        );
        assert_eq!(lb.mid, None, "p2 has no mid layer authored");
        assert_eq!(
            lb.front,
            Some(SpriteRef {
                group: 2,
                image: 1,
                offset: Pos::default()
            })
        );
        assert_eq!(lb.range, (0, -256), "p2 fill range mirrors to the left");
    }

    #[test]
    fn bg_layers_collect_contiguous_slots_and_stop_at_gap() {
        // bg0, bg1, bg3 present; bg2 missing -> only bg0, bg1 collected, in order.
        let def = DefFile::from_str(
            "[Lifebar]\n\
             p1.bg0.spr = 0, 0\n\
             p1.bg1.spr = 0, 1\n\
             p1.bg3.spr = 0, 3\n",
        )
        .unwrap();
        let l = ScreenpackLayout::parse(&def);
        assert_eq!(
            l.p1_lifebar.bg_layers,
            vec![
                SpriteRef {
                    group: 0,
                    image: 0,
                    offset: Pos::default()
                },
                SpriteRef {
                    group: 0,
                    image: 1,
                    offset: Pos::default()
                },
            ],
            "collection stops at the first missing bgN (the non-contiguous bg3 is ignored)"
        );
    }

    #[test]
    fn no_bg_layers_when_none_authored() {
        let def = DefFile::from_str("[Lifebar]\np1.front.spr = 2, 0\n").unwrap();
        let l = ScreenpackLayout::parse(&def);
        assert!(l.p1_lifebar.bg_layers.is_empty());
    }

    #[test]
    fn parses_powerbar_with_level_sounds() {
        let l = sample();
        let pb = &l.p1_powerbar;
        assert_eq!(pb.pos, Pos::new(80, 200));
        assert_eq!(
            pb.bg_layers,
            vec![SpriteRef {
                group: 10,
                image: 0,
                offset: Pos::default()
            }]
        );
        assert_eq!(
            pb.front,
            Some(SpriteRef {
                group: 12,
                image: 0,
                offset: Pos::default()
            })
        );
        assert_eq!(pb.range, (0, 144));
        assert_eq!(pb.level_sounds, vec![(60, 0), (60, 1), (60, 2)]);
        // P2 has no level sounds authored.
        assert!(l.p2_powerbar.level_sounds.is_empty());
    }

    #[test]
    fn parses_round_time_combo_with_font_slots() {
        let l = sample();
        assert_eq!(l.round.pos, Pos::new(160, 20));
        assert_eq!(
            l.round.font, 2,
            "font slot is the first int of 'font = 2,0,0'"
        );
        assert_eq!(l.time.pos, Pos::new(160, 16));
        assert_eq!(l.time.font, 1);
        assert_eq!(l.combo.pos, Pos::new(30, 80));
        assert_eq!(l.combo.font, 0);
    }

    #[test]
    fn parses_names_and_faces() {
        let l = sample();
        assert_eq!(l.p1_name.pos, Pos::new(20, 12));
        assert_eq!(l.p2_name.pos, Pos::new(300, 12));
        assert_eq!(
            l.p1_face.spr,
            Some(SpriteRef {
                group: 9000,
                image: 0,
                offset: Pos::new(1, 1)
            })
        );
        assert_eq!(l.p1_face.pos, Pos::new(12, 12));
        // P2 face has a sprite but no offset key -> offset stays default.
        assert_eq!(
            l.p2_face.spr,
            Some(SpriteRef {
                group: 9000,
                image: 0,
                offset: Pos::default()
            })
        );
    }

    #[test]
    fn unknown_section_is_ignored_not_panicked() {
        // The [UnknownSection] in SAMPLE must not affect parsing or panic.
        let l = sample();
        assert_eq!(l.sff, "fight.sff");
    }

    #[test]
    fn empty_def_yields_defaults() {
        let def = DefFile::from_str("").unwrap();
        let l = ScreenpackLayout::parse(&def);
        assert_eq!(l, ScreenpackLayout::default());
        assert!(l.sff.is_empty());
        assert!(l.fonts.is_empty());
        assert_eq!(l.p1_lifebar.range, (0, 0));
    }

    #[test]
    fn malformed_position_falls_back_to_default() {
        let def =
            DefFile::from_str("[Lifebar]\np1.pos = not-a-number\np1.range.x = 0, 200\n").unwrap();
        let l = ScreenpackLayout::parse(&def);
        // Bad pos -> default (0,0); the valid range still parses.
        assert_eq!(l.p1_lifebar.pos, Pos::default());
        assert_eq!(l.p1_lifebar.range, (0, 200));
    }

    #[test]
    fn fonts_stop_at_first_gap() {
        // font0, font1 present, font2 absent, font3 present -> only 0,1 collected.
        let def =
            DefFile::from_str("[Files]\nfont0 = a.fnt\nfont1 = b.fnt\nfont3 = d.fnt\n").unwrap();
        let l = ScreenpackLayout::parse(&def);
        assert_eq!(l.fonts, vec!["a.fnt".to_string(), "b.fnt".to_string()]);
    }
}
