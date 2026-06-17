//! SFF v2 palette sub-header parser.
//!
//! Each palette entry is a 16-byte sub-header pointing to 768 bytes (256 * RGB)
//! of color data in the LData block. Palettes can be linked to share data with
//! another palette entry.

use fp_core::{FpError, FpResult};
use nom::number::complete::{le_u16, le_u32};

/// Size of a single palette sub-header in bytes.
pub const PALETTE_SUBHEADER_SIZE: usize = 16;

/// Size of RGBA palette data (256 colors * 4 bytes).
pub const PALETTE_RGBA_SIZE: usize = 1024;

/// Size of raw RGB palette data on disk (256 colors * 3 bytes).
const PALETTE_RGB_SIZE: usize = 768;

/// Bytes per color in an SFF v2 on-disk palette entry (R, G, B, A).
const PALETTE_RGBA_BYTES_PER_COLOR: usize = 4;

/// A parsed palette sub-header from an SFF v2 file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SffPalette {
    /// Palette group number.
    pub group: u16,
    /// Item number within the group.
    pub item: u16,
    /// Number of colors in this palette (typically 256).
    pub num_colors: u16,
    /// Index of the palette this one links to (shares color data).
    /// If equal to this palette's own index, the palette has its own data.
    pub linked_index: u16,
    /// Offset of color data within the LData block.
    pub data_offset: u32,
    /// Length of color data in bytes.
    pub data_length: u32,
}

/// Parses a single 16-byte palette sub-header.
pub fn parse_palette(input: &[u8]) -> FpResult<SffPalette> {
    if input.len() < PALETTE_SUBHEADER_SIZE {
        return Err(FpError::parse(
            "SFF",
            format!(
                "palette sub-header too small: {} bytes (need {})",
                input.len(),
                PALETTE_SUBHEADER_SIZE
            ),
        ));
    }

    let (rest, group) = le_u16(input).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read palette group")
    })?;
    let (rest, item) = le_u16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read palette item")
    })?;
    let (rest, num_colors) = le_u16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read palette num_colors")
    })?;
    let (rest, linked_index) = le_u16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read palette linked_index")
    })?;
    let (rest, data_offset) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read palette data_offset")
    })?;
    let (_rest, data_length) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read palette data_length")
    })?;

    Ok(SffPalette {
        group,
        item,
        num_colors,
        linked_index,
        data_offset,
        data_length,
    })
}

/// Parses all palette sub-headers from the palette sub-header block.
pub fn parse_all_palettes(data: &[u8], count: u32) -> FpResult<Vec<SffPalette>> {
    let count = count as usize;
    let needed = count * PALETTE_SUBHEADER_SIZE;
    if data.len() < needed {
        return Err(FpError::parse(
            "SFF",
            format!(
                "palette sub-header block too small: {} bytes for {count} palettes (need {needed})",
                data.len()
            ),
        ));
    }

    let mut palettes = Vec::with_capacity(count);
    for i in 0..count {
        let offset = i * PALETTE_SUBHEADER_SIZE;
        let palette = parse_palette(&data[offset..])?;
        palettes.push(palette);
    }
    Ok(palettes)
}

/// Converts 768 bytes of RGB palette data into a 1024-byte RGBA palette.
///
/// Index 0 is made transparent (alpha = 0); all other colors get alpha = 255.
pub fn rgb_to_rgba(rgb_data: &[u8]) -> FpResult<[u8; PALETTE_RGBA_SIZE]> {
    if rgb_data.len() < PALETTE_RGB_SIZE {
        return Err(FpError::parse(
            "SFF",
            format!(
                "palette RGB data too small: {} bytes (need {})",
                rgb_data.len(),
                PALETTE_RGB_SIZE
            ),
        ));
    }

    let mut rgba = [0u8; PALETTE_RGBA_SIZE];
    for i in 0..256 {
        let src = i * 3;
        let dst = i * 4;
        rgba[dst] = rgb_data[src];
        rgba[dst + 1] = rgb_data[src + 1];
        rgba[dst + 2] = rgb_data[src + 2];
        // Index 0 is transparent in MUGEN palettes
        rgba[dst + 3] = if i == 0 { 0 } else { 255 };
    }
    Ok(rgba)
}

/// Reads an SFF v2 palette into a 1024-byte (256 * RGBA) palette.
///
/// SFF v2 stores palette colors as **RGBA** quadruplets in the LData block
/// (4 bytes per color), unlike SFF v1's trailing-PCX **RGB** triplets. A palette
/// may carry fewer than 256 colors (`num_colors`, e.g. KFM's 32-color per-sprite
/// palettes); colors beyond `num_colors` (and beyond the supplied data) stay
/// transparent black so the result is always a full 256-entry palette.
///
/// Index 0 is forced transparent (alpha = 0) per the MUGEN convention — its RGB
/// is preserved but the GPU shader discards it. Every other in-range color is
/// fully opaque (alpha = 255): the on-disk 4th byte of an SFF v2 palette entry
/// is a reserved/padding byte (almost always `0`), **not** a usable per-color
/// alpha, so honoring it would render every sprite invisible. `num_colors` is
/// clamped to 256 and to the bytes actually available, so malformed sub-headers
/// never read out of bounds or panic; a too-short slice simply yields fewer
/// opaque colors (the remainder stay transparent black).
pub fn rgba_to_palette(rgba_data: &[u8], num_colors: u16) -> [u8; PALETTE_RGBA_SIZE] {
    let mut rgba = [0u8; PALETTE_RGBA_SIZE];

    // Clamp the requested color count to a full palette and to what the slice
    // can actually supply, so a short/garbage sub-header can't over-read.
    let available = rgba_data.len() / PALETTE_RGBA_BYTES_PER_COLOR;
    let count = (num_colors as usize).min(256).min(available);

    for i in 0..count {
        let src = i * PALETTE_RGBA_BYTES_PER_COLOR;
        let dst = i * 4;
        rgba[dst] = rgba_data[src];
        rgba[dst + 1] = rgba_data[src + 1];
        rgba[dst + 2] = rgba_data[src + 2];
        // Index 0 is transparent in MUGEN palettes; every other color is opaque.
        // The on-disk 4th byte is reserved padding, not a usable alpha value.
        rgba[dst + 3] = if i == 0 { 0 } else { 255 };
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_palette_bytes(group: u16, item: u16, linked: u16, offset: u32) -> Vec<u8> {
        let mut buf = vec![0u8; PALETTE_SUBHEADER_SIZE];
        buf[0..2].copy_from_slice(&group.to_le_bytes());
        buf[2..4].copy_from_slice(&item.to_le_bytes());
        buf[4..6].copy_from_slice(&256u16.to_le_bytes()); // num_colors
        buf[6..8].copy_from_slice(&linked.to_le_bytes());
        buf[8..12].copy_from_slice(&offset.to_le_bytes());
        buf[12..16].copy_from_slice(&768u32.to_le_bytes()); // data_length
        buf
    }

    #[test]
    fn parse_valid_palette() {
        let data = make_palette_bytes(1, 1, 0, 100);
        let pal = parse_palette(&data).unwrap();

        assert_eq!(pal.group, 1);
        assert_eq!(pal.item, 1);
        assert_eq!(pal.num_colors, 256);
        assert_eq!(pal.linked_index, 0);
        assert_eq!(pal.data_offset, 100);
        assert_eq!(pal.data_length, 768);
    }

    #[test]
    fn parse_multiple_palettes() {
        let mut block = Vec::new();
        block.extend_from_slice(&make_palette_bytes(1, 1, 0, 0));
        block.extend_from_slice(&make_palette_bytes(1, 2, 0, 768));

        let palettes = parse_all_palettes(&block, 2).unwrap();
        assert_eq!(palettes.len(), 2);
        assert_eq!(palettes[0].item, 1);
        assert_eq!(palettes[1].item, 2);
    }

    #[test]
    fn rgb_to_rgba_conversion() {
        let mut rgb = vec![0u8; 768];
        // Color 0: R=255, G=0, B=0 — but should be transparent
        rgb[0] = 255;
        rgb[1] = 0;
        rgb[2] = 0;
        // Color 1: R=0, G=255, B=0 — opaque
        rgb[3] = 0;
        rgb[4] = 255;
        rgb[5] = 0;

        let rgba = rgb_to_rgba(&rgb).unwrap();

        // Index 0: transparent
        assert_eq!(rgba[0], 255); // R
        assert_eq!(rgba[1], 0); // G
        assert_eq!(rgba[2], 0); // B
        assert_eq!(rgba[3], 0); // A = transparent

        // Index 1: opaque
        assert_eq!(rgba[4], 0); // R
        assert_eq!(rgba[5], 255); // G
        assert_eq!(rgba[6], 0); // B
        assert_eq!(rgba[7], 255); // A = opaque
    }

    #[test]
    fn rgb_to_rgba_rejects_short_data() {
        let rgb = vec![0u8; 100];
        let err = rgb_to_rgba(&rgb).unwrap_err();
        assert!(err.to_string().contains("too small"));
    }

    #[test]
    fn rgba_to_palette_full_256() {
        // 256 RGBA colors. Color 0 black, color 1 green, with explicit padding.
        let mut data = vec![0u8; 1024];
        // color 0: black, on-disk alpha 5 (must be forced to 0 == transparent)
        data[0] = 0;
        data[1] = 0;
        data[2] = 0;
        data[3] = 5;
        // color 1: green, on-disk alpha 0 (must be forced to 255 == opaque)
        data[4] = 0;
        data[5] = 255;
        data[6] = 0;
        data[7] = 0;

        let pal = rgba_to_palette(&data, 256);

        // Index 0: transparent regardless of stored alpha.
        assert_eq!(&pal[0..4], &[0, 0, 0, 0]);
        // Index 1: green, opaque regardless of stored padding byte.
        assert_eq!(&pal[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn rgba_to_palette_partial_count() {
        // A 32-color palette (KFM's per-sprite size): only 128 bytes of data.
        let mut data = vec![0u8; 32 * 4];
        // color 1: blue
        data[4] = 0;
        data[5] = 0;
        data[6] = 255;
        data[7] = 0;
        // color 31 (last in range): white
        let last = 31 * 4;
        data[last] = 255;
        data[last + 1] = 255;
        data[last + 2] = 255;
        data[last + 3] = 0;

        let pal = rgba_to_palette(&data, 32);

        // The returned palette is always a full 256 entries.
        assert_eq!(pal.len(), PALETTE_RGBA_SIZE);
        // In-range colors decode and are forced opaque (except index 0).
        assert_eq!(&pal[4..8], &[0, 0, 255, 255]); // color 1: blue, opaque
        assert_eq!(&pal[124..128], &[255, 255, 255, 255]); // color 31: white, opaque
                                                           // Out-of-range colors (>= 32) stay transparent black.
        assert_eq!(&pal[128..132], &[0, 0, 0, 0]); // color 32: untouched
        assert_eq!(&pal[1020..1024], &[0, 0, 0, 0]); // color 255: untouched
    }

    #[test]
    fn rgba_to_palette_clamps_short_data() {
        // num_colors claims 256 but only 2 colors of data are present: the
        // decoder must clamp to what's available rather than reading past the
        // slice (no panic, no garbage).
        let mut data = vec![0u8; 8]; // 2 colors
        data[4] = 200;
        data[5] = 100;
        data[6] = 50;
        data[7] = 0; // color 1
        let pal = rgba_to_palette(&data, 256);

        assert_eq!(&pal[0..4], &[0, 0, 0, 0]); // color 0: transparent
        assert_eq!(&pal[4..8], &[200, 100, 50, 255]); // color 1: opaque
                                                      // Everything past the supplied data stays transparent black.
        assert_eq!(&pal[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn rgba_to_palette_clamps_oversized_count() {
        // num_colors larger than 256 must clamp to 256 without overflow.
        let data = vec![1u8; 4096];
        let pal = rgba_to_palette(&data, u16::MAX);
        assert_eq!(pal.len(), PALETTE_RGBA_SIZE);
        // Index 0 transparent; index 255 fully populated & opaque.
        assert_eq!(pal[3], 0);
        assert_eq!(&pal[1020..1024], &[1, 1, 1, 255]);
    }

    #[test]
    fn rgba_to_palette_empty_data_is_safe() {
        let pal = rgba_to_palette(&[], 256);
        assert_eq!(pal, [0u8; PALETTE_RGBA_SIZE]);
    }
}
