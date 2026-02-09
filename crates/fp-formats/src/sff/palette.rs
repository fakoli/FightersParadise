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

/// A parsed palette sub-header from an SFF v2 file.
#[derive(Debug, Clone)]
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
}
