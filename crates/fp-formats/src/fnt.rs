//! # FNT — MUGEN bitmap font parser
//!
//! Parses MUGEN `.fnt` font files (FNT **v1**, the WinMUGEN-era format) into a
//! typed [`FntFont`]: a single palette-indexed glyph strip (decoded from an
//! embedded 8-bit PCX image) plus a per-character `(x, width)` map describing
//! where each glyph lives inside that strip, and the font's line height /
//! spacing.
//!
//! # FNT v1 binary layout
//!
//! ```text
//! offset  size  field
//! 0       12    signature  "ElecbyteFnt\0"
//! 12      4     version    (verlo3, verlo2, verlo1, verhi); verhi at offset 15
//! 16      4     pcx_offset   (u32 LE) file offset of the embedded PCX image
//! 20      4     pcx_length   (u32 LE) byte length of the PCX image
//! 24      4     text_offset  (u32 LE) file offset of the text section
//! 28      4     text_length  (u32 LE) byte length of the text section
//! 32      32    reserved (zeroed)
//! ```
//!
//! The **PCX block** is a standard 8-bit RLE PCX image (the glyph strip) with a
//! trailing 256-colour VGA palette (`0x0C` marker + 768 RGB bytes), exactly like
//! SFF v1 sprites — so we reuse [`crate::sff::decode_pcx_8bit`] and the same
//! trailing-palette convention.
//!
//! The **text block** is an INI-style section list:
//!
//! ```text
//! [Def]
//! Type    = variable        ; "variable" or "fixed"
//! Def     = 0,0             ; default glyph column (x, width) for missing chars
//! Size    = 0,0             ; (advertised glyph cell size; informational)
//! Spacing = 1,0            ; (x,y) extra spacing between glyphs / lines
//! Colors  = 0               ; (informational)
//! Offset  = 0,0             ; (informational draw offset)
//!
//! [Map]
//! ; char   x_start   width
//! A         0         8
//! B         8         8
//! ...
//! ```
//!
//! A `[Map]` line names a character (or a numeric ASCII code) and gives the
//! glyph's `x` start column and `width` within the PCX strip. The glyph's height
//! is the full PCX image height.
//!
//! # FNT v2 (MUGEN 1.0+ sprite-font)
//!
//! FNT **v2** (MUGEN 1.0+) replaces the embedded PCX glyph strip with a
//! reference to an **SFF** sprite-font: each glyph is a separate sprite. It
//! shares the same `ElecbyteFnt\0` signature; the only header difference is the
//! version-high byte at offset 15 (`2` for v2). Its text section is still
//! `[Def]` + `[Map]`, but each `[Map]` line maps a character to a single SFF
//! sprite **index** (and optional offset) rather than an `(x, width)` column.
//!
//! This module **detects** v2 ([`detect_fnt_version`]) and **parses its glyph
//! table** into [`FntV2Info`] ([`FntFont::inspect_v2`]) without panicking, so a
//! caller can report exactly which/how many glyphs a v2 font declares. Because
//! `fp-render`'s text path consumes a decoded *bitmap* strip (not an SFF
//! sprite-font), [`FntFont::from_bytes`] does **not** synthesize an `FntFont`
//! for v2: it warns and returns [`FpError::Unsupported`] (a safe fallback — the
//! caller falls back to the bitmap HUD font), never a crash. The glyph table is
//! still parsed first so the error message carries the declared glyph count.
//!
//! # Never crash on bad content
//!
//! Every read is bounds-checked. A truncated header, an unparseable PCX, a
//! missing palette, or a malformed `[Map]` line yields a `tracing::warn!` and a
//! safe default (an empty glyph map, a zeroed palette, a fallback glyph), never
//! a panic.

use std::collections::HashMap;
use std::path::Path;

use fp_core::{FpError, FpResult};

use crate::sff::decode_pcx_8bit;

/// The 12-byte signature shared by all FNT files.
const FNT_SIGNATURE: &[u8; 12] = b"ElecbyteFnt\0";

/// Size of the fixed FNT v1 header (signature + version + four offset/length
/// u32s + reserved), in bytes.
const FNT_HEADER_SIZE: usize = 64;

/// PCX manufacturer byte (`0x0A`) at the start of every PCX image.
const PCX_MANUFACTURER: u8 = 0x0A;

/// The `0x0C` marker byte that precedes a PCX image's trailing 256-colour VGA
/// palette.
const PCX_PALETTE_MARKER: u8 = 0x0C;

/// Bytes occupied by a trailing VGA palette: a `0x0C` marker byte followed by
/// 256 RGB triplets (768 bytes).
const PCX_PALETTE_BLOCK_SIZE: usize = 1 + 768;

/// Number of RGBA bytes in a fully expanded 256-colour palette (256 × 4).
const PALETTE_RGBA_SIZE: usize = 256 * 4;

/// Which FNT container version a file declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FntVersion {
    /// FNT v1 — embedded 8-bit PCX glyph strip + `[Def]`/`[Map]` text section
    /// (WinMUGEN era). The only version this module decodes.
    V1,
    /// FNT v2 — MUGEN 1.0+ SFF-backed sprite-font. Detected and its glyph table
    /// is parsed ([`FntFont::inspect_v2`]), but no bitmap [`FntFont`] is
    /// synthesised: [`FntFont::from_bytes`] warns and returns
    /// [`FpError::Unsupported`] for it (a safe fallback, never a crash).
    V2,
}

/// A parsed FNT **v2** glyph table and metrics.
///
/// FNT v2 fonts reference an external/embedded **SFF** sprite-font instead of an
/// inline PCX strip, so this engine cannot yet render them through the bitmap
/// `draw_text` path. [`FntFont::inspect_v2`] still parses the v2 `[Def]`/`[Map]`
/// text section into this struct — without panicking — so callers can detect a
/// v2 font and report how many glyphs it declares before falling back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FntV2Info {
    /// `[Def] Type` (e.g. `"variable"` / `"fixed"`), lowercased. Empty if absent.
    pub font_type: String,
    /// Per-character SFF sprite index, keyed by `char`. A v2 `[Map]` line names a
    /// character and gives the SFF sprite **index** of its glyph (not an
    /// `(x, width)` column as in v1).
    pub glyphs: HashMap<char, u16>,
    /// Extra horizontal spacing (pixels) after each glyph, from `[Def] Spacing`.
    pub spacing_x: i32,
    /// Extra vertical spacing (pixels) between lines, from `[Def] Spacing`.
    pub spacing_y: i32,
}

impl FntV2Info {
    /// Number of distinct mapped glyphs in the v2 glyph table.
    pub fn glyph_count(&self) -> usize {
        self.glyphs.len()
    }
}

/// Detects the [`FntVersion`] of `data` from its signature and version byte,
/// **without** attempting to decode the font body.
///
/// Both v1 and v2 begin with the same 12-byte `ElecbyteFnt\0` signature followed
/// by four version bytes; the high-order byte (offset 15) is `1` for v1 and `2`
/// for v2. Returns a parse error for a too-short buffer, a wrong signature, or an
/// unknown version byte — never panics.
pub fn detect_fnt_version(data: &[u8]) -> FpResult<FntVersion> {
    detect_version(data)
}

/// A single glyph's column within the font's glyph strip.
///
/// The glyph occupies pixels `[x .. x + width)` horizontally and the full strip
/// height vertically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyph {
    /// Left pixel column of the glyph within the strip.
    pub x: u16,
    /// Glyph width in pixels.
    pub width: u16,
}

/// A parsed MUGEN bitmap font.
///
/// Holds the decoded glyph strip as palette indices plus its 256-colour RGBA
/// palette, the per-character glyph map, and layout metrics. The image is a
/// single horizontal (or grid-free) strip: every glyph shares the full
/// [`height`](Self::height) and is addressed by its `(x, width)` column via
/// [`glyph`](Self::glyph).
#[derive(Debug, Clone)]
pub struct FntFont {
    /// Glyph strip pixels as palette indices, row-major, `width * height` bytes.
    pub pixels: Vec<u8>,
    /// Strip width in pixels.
    pub image_width: u16,
    /// Strip height in pixels — also every glyph's height.
    pub image_height: u16,
    /// 256-colour RGBA palette (1024 bytes) for the glyph strip. Index 0 is
    /// transparent (alpha 0), MUGEN convention.
    pub palette: Vec<u8>,
    /// Per-character glyph columns. Keyed by `char`.
    pub glyphs: HashMap<char, Glyph>,
    /// Fallback glyph used when a requested character has no map entry (the
    /// `[Def] Def = x,width` column). `None` renders missing chars as blanks.
    pub default_glyph: Option<Glyph>,
    /// Extra horizontal spacing (in pixels) inserted **after** each glyph's
    /// advance, from `[Def] Spacing = x,y`.
    pub spacing_x: i32,
    /// Extra vertical spacing (in pixels) between text lines, from
    /// `[Def] Spacing = x,y`.
    pub spacing_y: i32,
}

impl FntFont {
    /// Loads and parses an FNT file from the given path.
    pub fn load(path: &Path) -> FpResult<Self> {
        let data = std::fs::read(path)?;
        Self::from_bytes(&data)
    }

    /// Parses an FNT font from raw bytes already in memory.
    ///
    /// Detects the version from the signature/version bytes. FNT v1 is decoded
    /// fully; FNT v2 is rejected with a warning (returns an `Unsupported`
    /// error). Malformed-but-v1 content never panics: it produces an empty or
    /// best-effort font.
    pub fn from_bytes(data: &[u8]) -> FpResult<Self> {
        match detect_version(data)? {
            FntVersion::V1 => Self::from_bytes_v1(data),
            FntVersion::V2 => {
                // Parse the v2 glyph table (best-effort, never panics) so the
                // diagnostic reports how many glyphs the font declares, then
                // fall back: this engine renders bitmap strips, not SFF
                // sprite-fonts, so no `FntFont` is synthesised for v2.
                let info = Self::inspect_v2(data).unwrap_or_else(|_| FntV2Info {
                    font_type: String::new(),
                    glyphs: HashMap::new(),
                    spacing_x: 0,
                    spacing_y: 0,
                });
                tracing::warn!(
                    glyphs = info.glyph_count(),
                    font_type = %info.font_type,
                    "FNT v2 (MUGEN 1.0+ SFF sprite-font) is not yet renderable; skipping"
                );
                Err(FpError::Unsupported(format!(
                    "FNT v2 (MUGEN 1.0+ SFF sprite-font, {} glyphs) is not implemented",
                    info.glyph_count()
                )))
            }
        }
    }

    /// Parses an FNT **v2** font's `[Def]`/`[Map]` glyph table into
    /// [`FntV2Info`] without decoding (or requiring) its referenced SFF.
    ///
    /// This is the inspection entry point for v2 fonts: it confirms the file is
    /// v2, reads the fixed header for the text-section offsets, and parses the
    /// `[Def]` metrics + `[Map]` (`char -> sff index`) glyph table. Returns a
    /// parse error if the file is not v2 or its header is truncated; a malformed
    /// `[Map]` line is skipped with a warning rather than failing. Never panics.
    pub fn inspect_v2(data: &[u8]) -> FpResult<FntV2Info> {
        match detect_version(data)? {
            FntVersion::V2 => {}
            FntVersion::V1 => {
                return Err(FpError::parse("FNT", "not an FNT v2 font (got v1)"));
            }
        }
        // The fixed header layout (offsets/lengths) is shared with v1; in v2 the
        // first block holds the embedded SFF instead of a PCX, but the text
        // block at offset 24/length 28 is still the `[Def]`/`[Map]` section.
        let header = parse_v1_header(data)?;
        let text = slice_block(
            data,
            header.text_offset as usize,
            header.text_length as usize,
            "v2 text",
        );
        let text_str = decode_text(text);
        Ok(parse_v2_text_section(&text_str))
    }

    /// Parses an FNT **v1** font from raw bytes.
    fn from_bytes_v1(data: &[u8]) -> FpResult<Self> {
        let header = parse_v1_header(data)?;

        // --- Embedded PCX glyph strip ---
        let pcx = slice_block(
            data,
            header.pcx_offset as usize,
            header.pcx_length as usize,
            "PCX",
        );
        let (image_width, image_height) = pcx_dimensions(pcx);
        // Decode the 8-bit RLE PCX into palette indices. A malformed PCX yields a
        // best-effort (possibly empty) buffer rather than an error.
        let pixels = decode_pcx_8bit(pcx).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "FNT v1: PCX glyph strip failed to decode; using empty strip");
            Vec::new()
        });
        let palette = extract_pcx_palette(pcx);

        // --- Text section: [Def] + [Map] ---
        let text = slice_block(
            data,
            header.text_offset as usize,
            header.text_length as usize,
            "text",
        );
        let text_str = decode_text(text);
        let parsed = parse_text_section(&text_str);

        tracing::info!(
            "FNT v1: loaded {}x{} glyph strip, {} mapped glyphs",
            image_width,
            image_height,
            parsed.glyphs.len()
        );

        Ok(Self {
            pixels,
            image_width,
            image_height,
            palette,
            glyphs: parsed.glyphs,
            default_glyph: parsed.default_glyph,
            spacing_x: parsed.spacing_x,
            spacing_y: parsed.spacing_y,
        })
    }

    /// Returns the glyph column for `ch`, falling back to the font's default
    /// glyph (`[Def] Def`) when `ch` is unmapped. Returns `None` if neither the
    /// character nor a default glyph exists.
    pub fn glyph(&self, ch: char) -> Option<Glyph> {
        self.glyphs.get(&ch).copied().or(self.default_glyph)
    }

    /// Number of distinct mapped glyphs (excludes the default fallback).
    pub fn glyph_count(&self) -> usize {
        self.glyphs.len()
    }
}

/// Parsed FNT v1 fixed header.
#[derive(Debug, Clone, Copy)]
struct FntV1Header {
    pcx_offset: u32,
    pcx_length: u32,
    text_offset: u32,
    text_length: u32,
}

/// Parses the FNT v1 fixed header (offsets/lengths for the PCX + text blocks).
///
/// Assumes the signature has already been validated. Returns an error only when
/// the buffer is too small to hold the header.
fn parse_v1_header(data: &[u8]) -> FpResult<FntV1Header> {
    if data.len() < FNT_HEADER_SIZE {
        return Err(FpError::parse(
            "FNT",
            format!(
                "file too small for FNT v1 header: {} bytes (need {FNT_HEADER_SIZE})",
                data.len()
            ),
        ));
    }
    Ok(FntV1Header {
        pcx_offset: read_u32(data, 16),
        pcx_length: read_u32(data, 20),
        text_offset: read_u32(data, 24),
        text_length: read_u32(data, 28),
    })
}

/// Detects whether `data` is an FNT v1 or v2 file.
///
/// Both begin with the same 12-byte `ElecbyteFnt\0` signature followed by four
/// version bytes; the high-order byte (offset 15) is `1` for v1 and `2` for v2.
fn detect_version(data: &[u8]) -> FpResult<FntVersion> {
    if data.len() < 16 {
        return Err(FpError::parse(
            "FNT",
            format!(
                "file too small for FNT header: {} bytes (need 16)",
                data.len()
            ),
        ));
    }
    if &data[0..12] != FNT_SIGNATURE.as_slice() {
        return Err(FpError::parse(
            "FNT",
            "invalid file signature (expected 'ElecbyteFnt\\0')",
        ));
    }
    match data[15] {
        1 => Ok(FntVersion::V1),
        2 => Ok(FntVersion::V2),
        other => Err(FpError::parse(
            "FNT",
            format!("unsupported FNT version {other} (expected 1 or 2)"),
        )),
    }
}

/// Reads a little-endian `u32` at `pos`, returning `0` if out of range.
fn read_u32(data: &[u8], pos: usize) -> u32 {
    match data.get(pos..pos + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => 0,
    }
}

/// Returns the `[offset .. offset+length)` slice of `data`, clamping to the
/// buffer end (and warning) when the declared block runs past EOF. Returns an
/// empty slice if the offset itself is out of range.
fn slice_block<'a>(data: &'a [u8], offset: usize, length: usize, what: &str) -> &'a [u8] {
    if offset >= data.len() {
        tracing::warn!(offset, what, "FNT v1: block offset past end of file; empty");
        return &[];
    }
    let end = offset.saturating_add(length).min(data.len());
    if end < offset.saturating_add(length) {
        tracing::warn!(
            offset,
            length,
            what,
            "FNT v1: block extends past end of file; truncating"
        );
    }
    &data[offset..end]
}

/// Reads the width/height from a PCX image header.
///
/// Returns `(0, 0)` if the data is too short or not a recognizable PCX image.
fn pcx_dimensions(pcx: &[u8]) -> (u16, u16) {
    if pcx.len() < 12 || pcx[0] != PCX_MANUFACTURER {
        return (0, 0);
    }
    let xmin = u16::from_le_bytes([pcx[4], pcx[5]]);
    let ymin = u16::from_le_bytes([pcx[6], pcx[7]]);
    let xmax = u16::from_le_bytes([pcx[8], pcx[9]]);
    let ymax = u16::from_le_bytes([pcx[10], pcx[11]]);
    let width = xmax.saturating_sub(xmin).saturating_add(1);
    let height = ymax.saturating_sub(ymin).saturating_add(1);
    (width, height)
}

/// Extracts the trailing 256-colour VGA palette of an 8-bit PCX image and
/// expands it to 1024-byte RGBA (index 0 transparent, MUGEN convention).
///
/// Returns an all-zero (transparent) palette when the PCX is too short to carry
/// a trailing palette — the caller still gets a valid 1024-byte buffer.
fn extract_pcx_palette(pcx: &[u8]) -> Vec<u8> {
    let mut rgba = vec![0u8; PALETTE_RGBA_SIZE];
    if pcx.len() < PCX_PALETTE_BLOCK_SIZE {
        tracing::warn!("FNT v1: PCX too short for a trailing palette; using transparent palette");
        return rgba;
    }
    let marker_pos = pcx.len() - PCX_PALETTE_BLOCK_SIZE;
    if pcx[marker_pos] != PCX_PALETTE_MARKER {
        tracing::debug!("FNT v1: missing 0x0C palette marker; reading trailing 768 bytes anyway");
    }
    let rgb = &pcx[marker_pos + 1..];
    for i in 0..256 {
        let src = i * 3;
        let dst = i * 4;
        if src + 3 > rgb.len() {
            break;
        }
        rgba[dst] = rgb[src];
        rgba[dst + 1] = rgb[src + 1];
        rgba[dst + 2] = rgb[src + 2];
        rgba[dst + 3] = if i == 0 { 0 } else { 255 };
    }
    rgba
}

/// Decodes the text section to a `String`, tolerating a UTF-8 BOM and treating
/// non-UTF-8 bytes losslessly (Latin-1-ish via `from_utf8_lossy`).
fn decode_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    s.strip_prefix('\u{feff}').unwrap_or(&s).to_string()
}

/// The result of parsing the `[Def]`/`[Map]` text section.
#[derive(Debug, Default)]
struct ParsedText {
    glyphs: HashMap<char, Glyph>,
    default_glyph: Option<Glyph>,
    spacing_x: i32,
    spacing_y: i32,
}

/// Parses the FNT v1 text section (`[Def]` metrics + `[Map]` glyph columns).
///
/// Case-insensitive section/keys; `;`, `//`, and `#` comments are stripped;
/// CRLF-tolerant. Malformed `[Map]` lines are skipped with a warning. Pure and
/// unit-testable.
fn parse_text_section(text: &str) -> ParsedText {
    let mut out = ParsedText::default();
    let mut in_map = false;

    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(name) = section_name(line) {
            in_map = name.eq_ignore_ascii_case("map");
            continue;
        }

        if in_map {
            if let Some((ch, glyph)) = parse_map_line(line) {
                out.glyphs.insert(ch, glyph);
            }
            continue;
        }

        // `[Def]` (or pre-section) key = value metrics.
        if let Some((key, value)) = split_kv(line) {
            match key.to_ascii_lowercase().as_str() {
                "def" => {
                    if let Some((x, w)) = parse_pair(value) {
                        out.default_glyph = Some(Glyph {
                            x: clamp_u16(x),
                            width: clamp_u16(w),
                        });
                    }
                }
                "spacing" => {
                    if let Some((x, y)) = parse_pair(value) {
                        out.spacing_x = x;
                        out.spacing_y = y;
                    }
                }
                _ => {}
            }
        }
    }

    out
}

/// Returns the section name if `line` is a `[Name]` header, else `None`.
fn section_name(line: &str) -> Option<&str> {
    let l = line.trim();
    if l.starts_with('[') && l.ends_with(']') && l.len() >= 2 {
        Some(l[1..l.len() - 1].trim())
    } else {
        None
    }
}

/// Splits a `key = value` line on the **first** `=`, trimming both sides.
fn split_kv(line: &str) -> Option<(&str, &str)> {
    let pos = line.find('=')?;
    Some((line[..pos].trim(), line[pos + 1..].trim()))
}

/// Parses an `x, y` integer pair (whitespace- or comma-separated). Returns
/// `None` if fewer than two integers are present.
fn parse_pair(s: &str) -> Option<(i32, i32)> {
    let mut it = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<i32>().ok());
    let a = it.next()?;
    let b = it.next()?;
    Some((a, b))
}

/// Parses one `[Map]` line into `(char, Glyph)`.
///
/// The first token names the character: either a single literal character, or a
/// quoted token `"X"`, or a numeric ASCII code (e.g. `32` for space). The next
/// two tokens are the glyph's `x` start and `width`. Returns `None` (with a
/// warning) for unparseable lines.
fn parse_map_line(line: &str) -> Option<(char, Glyph)> {
    // Split off the character token first: it may itself be a quoted literal or
    // a bare character, so we cannot naively split on commas (the char could be
    // a comma). Tokenise on commas/whitespace but keep the first token raw.
    let bytes = line.as_bytes();
    // A leading quote means the character literal is quoted: "X" — the byte
    // after the opening quote is the character (handles space, comma, etc.).
    let (ch, rest) = if bytes.first() == Some(&b'"') {
        // Find the closing quote.
        let close = line[1..].find('"').map(|i| i + 1)?;
        let inner = &line[1..close];
        let c = inner.chars().next()?;
        (c, &line[close + 1..])
    } else {
        // First token up to a comma or whitespace.
        let end = line
            .find(|c: char| c == ',' || c.is_whitespace())
            .unwrap_or(line.len());
        let tok = &line[..end];
        let rest = &line[end..];
        // A purely numeric token is an ASCII/codepoint code; otherwise it is a
        // single literal character.
        let c = if let Ok(code) = tok.parse::<u32>() {
            char::from_u32(code)?
        } else {
            tok.chars().next()?
        };
        (c, rest)
    };

    // Remaining tokens: x_start, width.
    let mut nums = rest
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<i32>().ok());
    let x = match nums.next() {
        Some(v) => v,
        None => {
            tracing::warn!(line, "FNT v1: [Map] line missing x column; skipped");
            return None;
        }
    };
    let width = match nums.next() {
        Some(v) => v,
        None => {
            tracing::warn!(line, "FNT v1: [Map] line missing width; skipped");
            return None;
        }
    };
    Some((
        ch,
        Glyph {
            x: clamp_u16(x),
            width: clamp_u16(width),
        },
    ))
}

/// Parses an FNT **v2** text section (`[Def]` metrics + `[Map]` `char -> sff
/// index` glyph table) into [`FntV2Info`].
///
/// Mirrors [`parse_text_section`] (case-insensitive sections/keys; `;`/`//`/`#`
/// comments; CRLF-tolerant) but a `[Map]` line carries a single SFF sprite
/// **index** after the character, not the v1 `(x, width)` pair. Malformed lines
/// are skipped with a warning. Pure and unit-testable.
fn parse_v2_text_section(text: &str) -> FntV2Info {
    let mut info = FntV2Info {
        font_type: String::new(),
        glyphs: HashMap::new(),
        spacing_x: 0,
        spacing_y: 0,
    };
    let mut in_map = false;

    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(name) = section_name(line) {
            in_map = name.eq_ignore_ascii_case("map");
            continue;
        }

        if in_map {
            if let Some((ch, index)) = parse_v2_map_line(line) {
                info.glyphs.insert(ch, index);
            }
            continue;
        }

        if let Some((key, value)) = split_kv(line) {
            match key.to_ascii_lowercase().as_str() {
                "type" => info.font_type = value.trim().to_ascii_lowercase(),
                "spacing" => {
                    if let Some((x, y)) = parse_pair(value) {
                        info.spacing_x = x;
                        info.spacing_y = y;
                    }
                }
                _ => {}
            }
        }
    }

    info
}

/// Parses one FNT v2 `[Map]` line into `(char, sff_index)`.
///
/// The character token is parsed exactly like v1 (a bare/quoted literal or a
/// numeric ASCII code); the next token is the glyph's SFF sprite index. Returns
/// `None` (with a warning) for an unparseable line.
fn parse_v2_map_line(line: &str) -> Option<(char, u16)> {
    let bytes = line.as_bytes();
    let (ch, rest) = if bytes.first() == Some(&b'"') {
        let close = line[1..].find('"').map(|i| i + 1)?;
        let inner = &line[1..close];
        let c = inner.chars().next()?;
        (c, &line[close + 1..])
    } else {
        let end = line
            .find(|c: char| c == ',' || c.is_whitespace())
            .unwrap_or(line.len());
        let tok = &line[..end];
        let rest = &line[end..];
        let c = if let Ok(code) = tok.parse::<u32>() {
            char::from_u32(code)?
        } else {
            tok.chars().next()?
        };
        (c, rest)
    };

    let mut nums = rest
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<i32>().ok());
    let index = match nums.next() {
        Some(v) => v,
        None => {
            tracing::warn!(line, "FNT v2: [Map] line missing sprite index; skipped");
            return None;
        }
    };
    Some((ch, clamp_u16(index)))
}

/// Clamps a (possibly negative) integer into `u16` range.
fn clamp_u16(v: i32) -> u16 {
    v.clamp(0, u16::MAX as i32) as u16
}

/// Strips `;`, `//`, and `#` comments from a line (whichever appears first).
fn strip_comment(line: &str) -> &str {
    let mut cut = line.len();
    if let Some(p) = line.find(';') {
        cut = cut.min(p);
    }
    if let Some(p) = line.find("//") {
        cut = cut.min(p);
    }
    if let Some(p) = line.find('#') {
        cut = cut.min(p);
    }
    &line[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal 8-bit RLE PCX (`width`×`height`, all pixels = `value`)
    /// with a trailing VGA palette whose colour index 1 has the given R channel.
    fn make_pcx(width: u16, height: u16, value: u8, pal1_r: u8) -> Vec<u8> {
        let mut pcx = vec![0u8; 128];
        pcx[0] = PCX_MANUFACTURER;
        pcx[1] = 5; // version
        pcx[2] = 1; // RLE encoding
        pcx[3] = 8; // bits per pixel
                    // xmin=0, ymin=0, xmax=width-1, ymax=height-1
        pcx[8..10].copy_from_slice(&(width - 1).to_le_bytes());
        pcx[10..12].copy_from_slice(&(height - 1).to_le_bytes());
        pcx[65] = 1; // planes
        pcx[66..68].copy_from_slice(&width.to_le_bytes()); // bytes per line
                                                           // Body: per scanline, emit RLE runs covering `width` pixels of `value`.
        for _ in 0..height {
            let mut remaining = width;
            while remaining > 0 {
                let run = remaining.min(0x3F);
                pcx.push(0xC0 | run as u8);
                pcx.push(value);
                remaining -= run;
            }
        }
        // Trailing palette: 0x0C marker + 256 RGB triplets.
        pcx.push(PCX_PALETTE_MARKER);
        let mut pal = vec![0u8; 768];
        pal[3] = pal1_r; // colour index 1, R channel
        pcx.extend_from_slice(&pal);
        pcx
    }

    /// Assembles a synthetic FNT v1 file: header + PCX block + text block.
    fn make_fnt(pcx: &[u8], text: &str) -> Vec<u8> {
        let mut buf = vec![0u8; FNT_HEADER_SIZE];
        buf[0..12].copy_from_slice(FNT_SIGNATURE);
        buf[12] = 0;
        buf[13] = 0;
        buf[14] = 0;
        buf[15] = 1; // version major = 1

        let pcx_offset = FNT_HEADER_SIZE as u32;
        let text_offset = pcx_offset + pcx.len() as u32;
        buf[16..20].copy_from_slice(&pcx_offset.to_le_bytes());
        buf[20..24].copy_from_slice(&(pcx.len() as u32).to_le_bytes());
        buf[24..28].copy_from_slice(&text_offset.to_le_bytes());
        buf[28..32].copy_from_slice(&(text.len() as u32).to_le_bytes());

        buf.extend_from_slice(pcx);
        buf.extend_from_slice(text.as_bytes());
        buf
    }

    const SAMPLE_TEXT: &str = "\
[Def]
Type    = variable
Def     = 0,3
Spacing = 1,2

[Map]
A 0 8
B 8 6
32 14 5
";

    /// A synthetic FNT v2 text section: `[Def]` metrics + a `[Map]` table whose
    /// lines map a character to a single SFF sprite index.
    const SAMPLE_V2_TEXT: &str = "\
[Def]
Type    = variable
Size    = 12,12
Spacing = 2,3

[Map]
A 0
B 1
32 7
";

    /// Assembles a synthetic FNT **v2** file: v1-shaped fixed header (version
    /// byte = 2) + a stand-in first data block + the v2 text block. The first
    /// block stands in for the embedded SFF (its bytes are never decoded by the
    /// v2 inspection path, which only reads the text section).
    fn make_fnt_v2(data_block: &[u8], text: &str) -> Vec<u8> {
        let mut buf = vec![0u8; FNT_HEADER_SIZE];
        buf[0..12].copy_from_slice(FNT_SIGNATURE);
        buf[15] = 2; // version major = 2 (sprite-font)

        let data_offset = FNT_HEADER_SIZE as u32;
        let text_offset = data_offset + data_block.len() as u32;
        buf[16..20].copy_from_slice(&data_offset.to_le_bytes());
        buf[20..24].copy_from_slice(&(data_block.len() as u32).to_le_bytes());
        buf[24..28].copy_from_slice(&text_offset.to_le_bytes());
        buf[28..32].copy_from_slice(&(text.len() as u32).to_le_bytes());

        buf.extend_from_slice(data_block);
        buf.extend_from_slice(text.as_bytes());
        buf
    }

    #[test]
    fn detects_v1_and_v2() {
        let pcx = make_pcx(16, 10, 1, 200);
        let mut data = make_fnt(&pcx, SAMPLE_TEXT);
        assert_eq!(detect_version(&data).unwrap(), FntVersion::V1);
        assert_eq!(detect_fnt_version(&data).unwrap(), FntVersion::V1);
        data[15] = 2;
        assert_eq!(detect_version(&data).unwrap(), FntVersion::V2);
        assert_eq!(detect_fnt_version(&data).unwrap(), FntVersion::V2);
    }

    #[test]
    fn detect_fnt_version_on_real_v2_file() {
        // A genuine v2-shaped file (not just a v1 with a flipped byte) is
        // detected as v2 without panicking.
        let data = make_fnt_v2(&[0xAB; 32], SAMPLE_V2_TEXT);
        assert_eq!(detect_fnt_version(&data).unwrap(), FntVersion::V2);
    }

    #[test]
    fn detect_fnt_version_rejects_short_and_bad_sig() {
        assert!(detect_fnt_version(&[0u8; 8]).is_err());
        assert!(detect_fnt_version(&[0u8; FNT_HEADER_SIZE]).is_err());
    }

    #[test]
    fn v2_inspect_parses_glyph_table() {
        let data = make_fnt_v2(&[0xAB; 32], SAMPLE_V2_TEXT);
        let info = FntFont::inspect_v2(&data).unwrap();
        assert_eq!(info.font_type, "variable");
        assert_eq!(info.glyph_count(), 3);
        assert_eq!(info.glyphs.get(&'A'), Some(&0));
        assert_eq!(info.glyphs.get(&'B'), Some(&1));
        // "32" is the ASCII code for space, mapped to sprite index 7.
        assert_eq!(info.glyphs.get(&' '), Some(&7));
        assert_eq!(info.spacing_x, 2);
        assert_eq!(info.spacing_y, 3);
    }

    #[test]
    fn v2_inspect_rejects_v1() {
        let pcx = make_pcx(16, 10, 1, 200);
        let v1 = make_fnt(&pcx, SAMPLE_TEXT);
        // inspect_v2 on a v1 file is a (recoverable) error, not a panic.
        assert!(FntFont::inspect_v2(&v1).is_err());
    }

    #[test]
    fn v2_inspect_skips_malformed_map_lines() {
        // "B" has no index; the rest survive — no panic.
        let text = "[Map]\nA 0\nB\nC 2\n\"D\" 3\n";
        let data = make_fnt_v2(&[0u8; 4], text);
        let info = FntFont::inspect_v2(&data).unwrap();
        assert_eq!(info.glyph_count(), 3);
        assert_eq!(info.glyphs.get(&'A'), Some(&0));
        assert!(!info.glyphs.contains_key(&'B'));
        assert_eq!(info.glyphs.get(&'C'), Some(&2));
        assert_eq!(info.glyphs.get(&'D'), Some(&3));
    }

    #[test]
    fn v2_inspect_truncated_header_is_error_not_panic() {
        let mut buf = vec![0u8; 16];
        buf[0..12].copy_from_slice(FNT_SIGNATURE);
        buf[15] = 2;
        // Detected as v2, but too small for the fixed header -> recoverable err.
        assert_eq!(detect_fnt_version(&buf).unwrap(), FntVersion::V2);
        assert!(FntFont::inspect_v2(&buf).is_err());
    }

    #[test]
    fn from_bytes_on_real_v2_file_is_unsupported_not_panic() {
        // A genuine v2 file with a populated glyph table: from_bytes must report
        // Unsupported (safe fallback for the bitmap render path), never panic,
        // and the message should carry the parsed glyph count.
        let data = make_fnt_v2(&[0xAB; 32], SAMPLE_V2_TEXT);
        match FntFont::from_bytes(&data).unwrap_err() {
            FpError::Unsupported(msg) => {
                assert!(msg.contains("FNT v2"), "msg: {msg}");
                assert!(
                    msg.contains("3 glyphs"),
                    "msg should carry glyph count: {msg}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_signature() {
        let data = vec![0u8; FNT_HEADER_SIZE];
        assert!(detect_version(&data).is_err());
    }

    #[test]
    fn v2_is_unsupported_not_panic() {
        let pcx = make_pcx(16, 10, 1, 200);
        let mut data = make_fnt(&pcx, SAMPLE_TEXT);
        data[15] = 2;
        let err = FntFont::from_bytes(&data).unwrap_err();
        assert!(matches!(err, FpError::Unsupported(_)));
    }

    #[test]
    fn parses_image_dims_and_palette() {
        let pcx = make_pcx(22, 11, 1, 200);
        let data = make_fnt(&pcx, SAMPLE_TEXT);
        let font = FntFont::from_bytes(&data).unwrap();

        assert_eq!(font.image_width, 22);
        assert_eq!(font.image_height, 11);
        // Pixels decode to width*height of palette index 1.
        assert_eq!(font.pixels.len(), 22 * 11);
        assert!(font.pixels.iter().all(|&p| p == 1));
        // Palette is 1024 bytes; index 0 transparent, index 1 carries R=200.
        assert_eq!(font.palette.len(), PALETTE_RGBA_SIZE);
        assert_eq!(font.palette[3], 0, "index 0 alpha is transparent");
        assert_eq!(font.palette[4], 200, "index 1 R channel from PCX palette");
        assert_eq!(font.palette[7], 255, "index 1 alpha is opaque");
    }

    #[test]
    fn parses_glyph_map() {
        let pcx = make_pcx(22, 11, 1, 200);
        let data = make_fnt(&pcx, SAMPLE_TEXT);
        let font = FntFont::from_bytes(&data).unwrap();

        assert_eq!(font.glyph_count(), 3);
        assert_eq!(font.glyph('A'), Some(Glyph { x: 0, width: 8 }));
        assert_eq!(font.glyph('B'), Some(Glyph { x: 8, width: 6 }));
        // "32" is the ASCII code for space.
        assert_eq!(font.glyph(' '), Some(Glyph { x: 14, width: 5 }));
        // Metrics from [Def].
        assert_eq!(font.default_glyph, Some(Glyph { x: 0, width: 3 }));
        assert_eq!(font.spacing_x, 1);
        assert_eq!(font.spacing_y, 2);
    }

    #[test]
    fn unmapped_char_falls_back_to_default() {
        let pcx = make_pcx(22, 11, 1, 200);
        let data = make_fnt(&pcx, SAMPLE_TEXT);
        let font = FntFont::from_bytes(&data).unwrap();
        // 'Z' is unmapped -> default glyph (0,3).
        assert_eq!(font.glyph('Z'), Some(Glyph { x: 0, width: 3 }));
    }

    #[test]
    fn quoted_char_literal_handles_space() {
        let text = "[Map]\n\" \" 14 5\n\"A\" 0 8\n";
        let pcx = make_pcx(22, 11, 1, 200);
        let data = make_fnt(&pcx, text);
        let font = FntFont::from_bytes(&data).unwrap();
        assert_eq!(font.glyph(' '), Some(Glyph { x: 14, width: 5 }));
        assert_eq!(font.glyph('A'), Some(Glyph { x: 0, width: 8 }));
    }

    #[test]
    fn comma_separated_map_line() {
        let text = "[Map]\nA,0,8\nB,8,6\n";
        let pcx = make_pcx(22, 11, 1, 200);
        let data = make_fnt(&pcx, text);
        let font = FntFont::from_bytes(&data).unwrap();
        assert_eq!(font.glyph('A'), Some(Glyph { x: 0, width: 8 }));
        assert_eq!(font.glyph('B'), Some(Glyph { x: 8, width: 6 }));
    }

    #[test]
    fn comments_and_crlf_tolerated() {
        let text = "[Map]\r\nA 0 8 ; the letter A\r\nB 8 6 // comment\r\n# whole-line comment\r\n";
        let pcx = make_pcx(22, 11, 1, 200);
        let data = make_fnt(&pcx, text);
        let font = FntFont::from_bytes(&data).unwrap();
        assert_eq!(font.glyph_count(), 2);
        assert_eq!(font.glyph('A'), Some(Glyph { x: 0, width: 8 }));
    }

    #[test]
    fn malformed_map_line_skipped_not_panicked() {
        let text = "[Map]\nA 0 8\nB\nC 16\nD 24 8\n";
        let pcx = make_pcx(40, 11, 1, 200);
        let data = make_fnt(&pcx, text);
        let font = FntFont::from_bytes(&data).unwrap();
        // B (no nums) and C (one num) are dropped; A and D survive.
        assert_eq!(font.glyph_count(), 2);
        assert!(font.glyph('A').is_some());
        assert!(font.glyph('D').is_some());
        assert!(!font.glyphs.contains_key(&'B'));
        assert!(!font.glyphs.contains_key(&'C'));
    }

    #[test]
    fn truncated_header_is_error_not_panic() {
        let data = vec![0u8; 8];
        assert!(FntFont::from_bytes(&data).is_err());
    }

    #[test]
    fn block_offsets_past_eof_recover() {
        // Valid header/signature, but PCX/text offsets point past EOF.
        let mut buf = vec![0u8; FNT_HEADER_SIZE];
        buf[0..12].copy_from_slice(FNT_SIGNATURE);
        buf[15] = 1;
        buf[16..20].copy_from_slice(&9999u32.to_le_bytes()); // pcx_offset past EOF
        buf[20..24].copy_from_slice(&100u32.to_le_bytes());
        buf[24..28].copy_from_slice(&9999u32.to_le_bytes()); // text_offset past EOF
        buf[28..32].copy_from_slice(&100u32.to_le_bytes());
        // Must not panic; yields an empty font with a transparent palette.
        let font = FntFont::from_bytes(&buf).unwrap();
        assert_eq!(font.image_width, 0);
        assert_eq!(font.image_height, 0);
        assert!(font.pixels.is_empty());
        assert_eq!(font.glyph_count(), 0);
        assert_eq!(font.palette.len(), PALETTE_RGBA_SIZE);
    }

    #[test]
    fn no_default_glyph_returns_none_for_missing() {
        let text = "[Map]\nA 0 8\n";
        let pcx = make_pcx(16, 10, 1, 200);
        let data = make_fnt(&pcx, text);
        let font = FntFont::from_bytes(&data).unwrap();
        assert!(font.default_glyph.is_none());
        assert!(font.glyph('Q').is_none());
    }
}
