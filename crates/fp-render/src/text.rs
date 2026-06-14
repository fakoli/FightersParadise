//! Bitmap-font text rendering on top of the palette-indexed sprite pipeline.
//!
//! A parsed [`fp_formats::fnt::FntFont`] is uploaded once as a [`GlyphFont`]: its
//! glyph strip becomes an `R8Unorm` index texture (a one-row "atlas") and its
//! palette a 256×1 RGBA texture — the same pair the sprite renderer already
//! samples. Drawing a string then walks the glyphs, and for each one issues a
//! [`RenderFrame::draw_sprite_region`](crate::RenderFrame::draw_sprite_region)
//! sampling that glyph's column out of the strip.
//!
//! The placement math (string → sequence of `(char, src-rect, dst-x)`, advancing
//! the pen per glyph, with a missing-char fallback) lives in [`layout_text`],
//! which is pure and unit-tested. The GPU draw in `draw_text` is a thin loop
//! over its output.

use fp_formats::fnt::FntFont;

use crate::params::BlendMode;
use crate::texture::{PaletteTexture, SpriteTexture};

/// Parameters controlling how a string is drawn by
/// [`RenderFrame::draw_text`](crate::RenderFrame::draw_text).
///
/// `x`/`y` place the text's top-left origin in screen pixels; `scale` enlarges
/// the whole string uniformly (`1.0` = native glyph size); `alpha`/`blend`
/// apply per glyph.
#[derive(Debug, Clone, Copy)]
pub struct TextDrawParams {
    /// Left edge X of the text origin, in screen pixels.
    pub x: f32,
    /// Top edge Y of the text origin, in screen pixels.
    pub y: f32,
    /// Uniform scale factor applied to glyph size and pen advances.
    pub scale: f32,
    /// Opacity multiplier (0.0 transparent – 1.0 opaque).
    pub alpha: f32,
    /// Blend mode for each glyph quad.
    pub blend: BlendMode,
}

impl Default for TextDrawParams {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            alpha: 1.0,
            blend: BlendMode::Normal,
        }
    }
}

/// A GPU-resident bitmap font: the glyph strip as an index texture plus its
/// palette, together with the glyph map and layout metrics needed to place text.
///
/// Build one per loaded `.fnt` via [`GlyphFont::new`]; reuse it across frames.
pub struct GlyphFont {
    /// The glyph strip as an `R8Unorm` index texture (one row of glyphs).
    pub texture: SpriteTexture,
    /// The font's 256-colour palette as a 256×1 RGBA texture.
    pub palette: PaletteTexture,
    /// Strip width in pixels (the texture's `u` denominator).
    image_width: u32,
    /// Strip height in pixels — every glyph's height and the text line height.
    image_height: u32,
    /// Per-character glyph columns + metrics, kept from the parsed font.
    font: FntFont,
}

/// One positioned glyph produced by [`layout_text`].
///
/// `src` is the glyph's pixel rectangle within the strip; `dst_x`/`dst_y` are
/// the top-left destination pixel of where to draw it (relative to the text's
/// origin), and `width`/`height` are its destination size in pixels (here equal
/// to the source size — text layout does not scale per-glyph).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedGlyph {
    /// The character this glyph renders.
    pub ch: char,
    /// Source pixel rectangle in the strip: `[x, y, width, height]`.
    pub src: [u32; 4],
    /// Destination X (pixels) of the glyph's left edge, relative to text origin.
    pub dst_x: f32,
    /// Destination Y (pixels) of the glyph's top edge, relative to text origin.
    pub dst_y: f32,
    /// Glyph width in pixels.
    pub width: u32,
    /// Glyph height in pixels.
    pub height: u32,
}

impl GlyphFont {
    /// Uploads a parsed [`FntFont`] to the GPU.
    ///
    /// The glyph strip is uploaded as an `R8Unorm` index texture and the palette
    /// as a 256×1 RGBA texture. A font with a zero-area strip (e.g. a font whose
    /// PCX failed to decode) still produces a valid (1×1) `GlyphFont` so callers
    /// never have to special-case it — its glyphs simply draw nothing.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, font: FntFont) -> Self {
        let w = font.image_width.max(1) as u32;
        let h = font.image_height.max(1) as u32;

        // Guarantee the upload has exactly `w * h` bytes even if the decoded
        // strip was short/empty (never panic on bad content).
        let mut pixels = font.pixels.clone();
        pixels.resize((w as usize) * (h as usize), 0);

        let texture = SpriteTexture::new(device, queue, w, h, &pixels);

        // Palette is 1024 bytes (256 RGBA). Pad/truncate defensively.
        let mut pal = [0u8; 1024];
        let n = font.palette.len().min(1024);
        pal[..n].copy_from_slice(&font.palette[..n]);
        let palette = PaletteTexture::new(device, queue, &pal);

        Self {
            texture,
            palette,
            image_width: w,
            image_height: h,
            font,
        }
    }

    /// Strip height in pixels — the height of a single text line.
    pub fn line_height(&self) -> u32 {
        self.image_height
    }

    /// Strip width in pixels (the texture's `u` denominator).
    pub fn image_width(&self) -> u32 {
        self.image_width
    }

    /// Lays out `text` into positioned glyphs starting at the origin `(0, 0)`.
    ///
    /// See [`layout_text`] for the placement rules.
    pub fn layout(&self, text: &str) -> Vec<PlacedGlyph> {
        layout_text(&self.font, self.image_height as u16, text)
    }

    /// Normalized UV rectangle `[u_min, v_min, u_max, v_max]` for a placed
    /// glyph's source pixel rect within this font's strip texture.
    pub fn glyph_uv(&self, g: &PlacedGlyph) -> [f32; 4] {
        let tw = self.image_width as f32;
        let th = self.image_height as f32;
        let x0 = g.src[0] as f32;
        let y0 = g.src[1] as f32;
        let x1 = x0 + g.src[2] as f32;
        let y1 = y0 + g.src[3] as f32;
        [x0 / tw, y0 / th, x1 / tw, y1 / th]
    }
}

/// Computes the placement of every glyph in `text`: the pure string →
/// `(char, src-rect, dst-x)` math.
///
/// Rules:
/// - The pen starts at `x = 0`, `y = 0`.
/// - Each character resolves to its mapped glyph, or the font's default glyph
///   ([`FntFont::glyph`]); an unmapped char with **no** default is skipped
///   entirely (no advance, no rect).
/// - A glyph's source rect is `[glyph.x, 0, glyph.width, image_height]` and its
///   destination is the pen position; the pen then advances by
///   `glyph.width + spacing_x` (clamped to non-negative).
/// - `'\n'` resets `x` to 0 and advances `y` by `image_height + spacing_y`.
/// - Other ASCII control characters (`< 0x20`, except `\n`) are skipped.
///
/// Pure and deterministic — unit-tested without any GPU.
pub fn layout_text(font: &FntFont, image_height: u16, text: &str) -> Vec<PlacedGlyph> {
    let line_advance = (image_height as i32 + font.spacing_y).max(0) as f32;
    let mut out = Vec::new();
    let mut pen_x = 0.0f32;
    let mut pen_y = 0.0f32;

    for ch in text.chars() {
        if ch == '\n' {
            pen_x = 0.0;
            pen_y += line_advance;
            continue;
        }
        // Skip other control characters (tab, CR, etc.) without advancing.
        if (ch as u32) < 0x20 {
            continue;
        }

        let Some(glyph) = font.glyph(ch) else {
            // No mapping and no default glyph: emit nothing, no advance.
            continue;
        };

        out.push(PlacedGlyph {
            ch,
            src: [glyph.x as u32, 0, glyph.width as u32, image_height as u32],
            dst_x: pen_x,
            dst_y: pen_y,
            width: glyph.width as u32,
            height: image_height as u32,
        });

        let advance = (glyph.width as i32 + font.spacing_x).max(0) as f32;
        pen_x += advance;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_formats::fnt::{FntFont, Glyph};
    use std::collections::HashMap;

    /// Builds a synthetic font: glyphs of fixed metrics, `image_height = 10`.
    fn test_font(spacing_x: i32, spacing_y: i32, default_glyph: Option<Glyph>) -> FntFont {
        let mut glyphs = HashMap::new();
        glyphs.insert('A', Glyph { x: 0, width: 8 });
        glyphs.insert('B', Glyph { x: 8, width: 6 });
        glyphs.insert(' ', Glyph { x: 14, width: 5 });
        FntFont {
            pixels: vec![0u8; 20 * 10],
            image_width: 20,
            image_height: 10,
            palette: vec![0u8; 1024],
            glyphs,
            default_glyph,
            spacing_x,
            spacing_y,
        }
    }

    #[test]
    fn advances_pen_per_glyph() {
        let font = test_font(0, 0, None);
        let placed = layout_text(&font, font.image_height, "AB");
        assert_eq!(placed.len(), 2);
        // 'A' at x=0, src column [0,8].
        assert_eq!(placed[0].ch, 'A');
        assert_eq!(placed[0].dst_x, 0.0);
        assert_eq!(placed[0].src, [0, 0, 8, 10]);
        // 'B' advances by A's width (8) since spacing is 0.
        assert_eq!(placed[1].ch, 'B');
        assert_eq!(placed[1].dst_x, 8.0);
        assert_eq!(placed[1].src, [8, 0, 6, 10]);
    }

    #[test]
    fn spacing_x_adds_to_advance() {
        let font = test_font(2, 0, None);
        let placed = layout_text(&font, font.image_height, "AB");
        // 'B' now at 8 (A width) + 2 (spacing) = 10.
        assert_eq!(placed[1].dst_x, 10.0);
    }

    #[test]
    fn missing_char_uses_default_glyph() {
        let font = test_font(0, 0, Some(Glyph { x: 14, width: 5 }));
        let placed = layout_text(&font, font.image_height, "AZ");
        // 'Z' is unmapped -> falls back to the default glyph column.
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[1].ch, 'Z');
        assert_eq!(placed[1].src, [14, 0, 5, 10]);
        assert_eq!(placed[1].dst_x, 8.0); // after A's advance
    }

    #[test]
    fn missing_char_without_default_is_skipped() {
        let font = test_font(0, 0, None);
        let placed = layout_text(&font, font.image_height, "AZB");
        // 'Z' has no glyph and no default -> skipped, no advance.
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].ch, 'A');
        assert_eq!(placed[1].ch, 'B');
        // 'B' sits at A's advance (8), unaffected by the dropped 'Z'.
        assert_eq!(placed[1].dst_x, 8.0);
    }

    #[test]
    fn newline_resets_x_and_advances_y() {
        let font = test_font(0, 3, None);
        let placed = layout_text(&font, font.image_height, "A\nB");
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].dst_y, 0.0);
        // Second line: x back to 0, y advanced by image_height(10)+spacing_y(3).
        assert_eq!(placed[1].dst_x, 0.0);
        assert_eq!(placed[1].dst_y, 13.0);
    }

    #[test]
    fn control_chars_other_than_newline_are_skipped() {
        let font = test_font(0, 0, None);
        let placed = layout_text(&font, font.image_height, "A\tB\rA");
        // Tab and CR are skipped; we get A, B, A.
        let chars: Vec<char> = placed.iter().map(|g| g.ch).collect();
        assert_eq!(chars, vec!['A', 'B', 'A']);
        // No phantom advance from the control chars: B follows A's 8px advance.
        assert_eq!(placed[1].dst_x, 8.0);
    }

    #[test]
    fn empty_string_lays_out_nothing() {
        let font = test_font(0, 0, None);
        assert!(layout_text(&font, font.image_height, "").is_empty());
    }

    #[test]
    fn space_glyph_is_drawn_and_advances() {
        let font = test_font(0, 0, None);
        let placed = layout_text(&font, font.image_height, "A B");
        assert_eq!(placed.len(), 3);
        assert_eq!(placed[1].ch, ' ');
        assert_eq!(placed[1].dst_x, 8.0); // after A
        assert_eq!(placed[2].dst_x, 13.0); // after A(8) + space(5)
    }
}
