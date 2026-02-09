//! SFF v2 sprite sub-header parser.
//!
//! Each sprite entry is a 28-byte sub-header describing the sprite's dimensions,
//! axis position, compression format, and location within the data blocks.

use fp_core::{FpError, FpResult};
use nom::number::complete::{le_i16, le_u16, le_u32, le_u8};

/// Size of a single sprite sub-header in bytes.
pub const SPRITE_SUBHEADER_SIZE: usize = 28;

/// Compression format used to encode sprite pixel data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SpriteFormat {
    /// Uncompressed raw pixel data.
    Raw = 0,
    /// Invalid / reserved format.
    Invalid = 1,
    /// Run-length encoded, 8-bit.
    Rle8 = 2,
    /// Run-length encoded, 5-bit (with embedded palette index).
    Rle5 = 3,
    /// LZ-based compression with 5-bit tokens.
    Lz5 = 4,
    /// PNG with 8-bit indexed color.
    Png8 = 10,
    /// PNG with 24-bit direct color.
    Png24 = 11,
    /// PNG with 32-bit direct color (with alpha).
    Png32 = 12,
}

impl SpriteFormat {
    /// Converts a raw byte to a `SpriteFormat`, returning `None` for unknown values.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Raw),
            1 => Some(Self::Invalid),
            2 => Some(Self::Rle8),
            3 => Some(Self::Rle5),
            4 => Some(Self::Lz5),
            10 => Some(Self::Png8),
            11 => Some(Self::Png24),
            12 => Some(Self::Png32),
            _ => None,
        }
    }
}

/// A parsed sprite sub-header from an SFF v2 file.
#[derive(Debug, Clone)]
pub struct SffSprite {
    /// Sprite group number.
    pub group: u16,
    /// Image number within the group.
    pub image: u16,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// X-axis offset (hotspot).
    pub axis_x: i16,
    /// Y-axis offset (hotspot).
    pub axis_y: i16,
    /// Index of the sprite this one is linked to (shares pixel data).
    /// If equal to this sprite's own index, the sprite has its own data.
    pub linked_index: u16,
    /// Compression format of the pixel data.
    pub format: SpriteFormat,
    /// Color depth in bits (8 for indexed, 24 or 32 for direct color).
    pub color_depth: u8,
    /// Offset of pixel data within the data block (LData or TData).
    pub data_offset: u32,
    /// Length of the pixel data in bytes.
    pub data_length: u32,
    /// Index into the palette table.
    pub palette_index: u16,
    /// Flags bit field. Bit 0: 0 = use LData, 1 = use TData.
    pub flags: u16,
}

impl SffSprite {
    /// Returns `true` if this sprite's pixel data is stored in the TData block.
    pub fn uses_tdata(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// Parses a single 28-byte sprite sub-header.
pub fn parse_sprite(input: &[u8]) -> FpResult<SffSprite> {
    if input.len() < SPRITE_SUBHEADER_SIZE {
        return Err(FpError::parse(
            "SFF",
            format!(
                "sprite sub-header too small: {} bytes (need {})",
                input.len(),
                SPRITE_SUBHEADER_SIZE
            ),
        ));
    }

    let (rest, group) = le_u16(input).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite group")
    })?;
    let (rest, image) = le_u16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite image")
    })?;
    let (rest, width) = le_u16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite width")
    })?;
    let (rest, height) = le_u16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite height")
    })?;
    let (rest, axis_x) = le_i16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite axis_x")
    })?;
    let (rest, axis_y) = le_i16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite axis_y")
    })?;
    let (rest, linked_index) = le_u16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite linked_index")
    })?;
    let (rest, format_byte) = le_u8(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite format")
    })?;
    let (rest, color_depth) = le_u8(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite color_depth")
    })?;
    let (rest, data_offset) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite data_offset")
    })?;
    let (rest, data_length) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite data_length")
    })?;
    let (rest, palette_index) = le_u16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite palette_index")
    })?;
    let (_rest, flags) = le_u16(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite flags")
    })?;

    let format = SpriteFormat::from_byte(format_byte).ok_or_else(|| {
        FpError::parse("SFF", format!("unknown sprite format byte: {format_byte}"))
    })?;

    Ok(SffSprite {
        group,
        image,
        width,
        height,
        axis_x,
        axis_y,
        linked_index,
        format,
        color_depth,
        data_offset,
        data_length,
        palette_index,
        flags,
    })
}

/// Parses all sprite sub-headers from the sprite sub-header block.
pub fn parse_all_sprites(data: &[u8], count: u32) -> FpResult<Vec<SffSprite>> {
    let count = count as usize;
    let needed = count * SPRITE_SUBHEADER_SIZE;
    if data.len() < needed {
        return Err(FpError::parse(
            "SFF",
            format!(
                "sprite sub-header block too small: {} bytes for {count} sprites (need {needed})",
                data.len()
            ),
        ));
    }

    let mut sprites = Vec::with_capacity(count);
    for i in 0..count {
        let offset = i * SPRITE_SUBHEADER_SIZE;
        let sprite = parse_sprite(&data[offset..])?;
        sprites.push(sprite);
    }
    Ok(sprites)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sprite_bytes(
        group: u16,
        image: u16,
        width: u16,
        height: u16,
        format: u8,
        flags: u16,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; SPRITE_SUBHEADER_SIZE];
        buf[0..2].copy_from_slice(&group.to_le_bytes());
        buf[2..4].copy_from_slice(&image.to_le_bytes());
        buf[4..6].copy_from_slice(&width.to_le_bytes());
        buf[6..8].copy_from_slice(&height.to_le_bytes());
        buf[8..10].copy_from_slice(&5i16.to_le_bytes()); // axis_x
        buf[10..12].copy_from_slice(&10i16.to_le_bytes()); // axis_y
        buf[12..14].copy_from_slice(&0u16.to_le_bytes()); // linked_index
        buf[14] = format;
        buf[15] = 8; // color_depth
        buf[16..20].copy_from_slice(&100u32.to_le_bytes()); // data_offset
        buf[20..24].copy_from_slice(&256u32.to_le_bytes()); // data_length
        buf[24..26].copy_from_slice(&0u16.to_le_bytes()); // palette_index
        buf[26..28].copy_from_slice(&flags.to_le_bytes());
        buf
    }

    #[test]
    fn parse_valid_sprite() {
        let data = make_sprite_bytes(0, 1, 128, 64, 2, 0);
        let sprite = parse_sprite(&data).unwrap();

        assert_eq!(sprite.group, 0);
        assert_eq!(sprite.image, 1);
        assert_eq!(sprite.width, 128);
        assert_eq!(sprite.height, 64);
        assert_eq!(sprite.axis_x, 5);
        assert_eq!(sprite.axis_y, 10);
        assert_eq!(sprite.format, SpriteFormat::Rle8);
        assert_eq!(sprite.color_depth, 8);
        assert_eq!(sprite.data_offset, 100);
        assert_eq!(sprite.data_length, 256);
        assert!(!sprite.uses_tdata());
    }

    #[test]
    fn sprite_uses_tdata_flag() {
        let data = make_sprite_bytes(0, 0, 32, 32, 0, 1);
        let sprite = parse_sprite(&data).unwrap();
        assert!(sprite.uses_tdata());
    }

    #[test]
    fn reject_unknown_format() {
        let data = make_sprite_bytes(0, 0, 32, 32, 99, 0);
        let err = parse_sprite(&data).unwrap_err();
        assert!(err.to_string().contains("unknown sprite format"));
    }

    #[test]
    fn parse_multiple_sprites() {
        let mut block = Vec::new();
        block.extend_from_slice(&make_sprite_bytes(0, 0, 64, 64, 2, 0));
        block.extend_from_slice(&make_sprite_bytes(0, 1, 32, 32, 0, 1));

        let sprites = parse_all_sprites(&block, 2).unwrap();
        assert_eq!(sprites.len(), 2);
        assert_eq!(sprites[0].group, 0);
        assert_eq!(sprites[0].image, 0);
        assert_eq!(sprites[1].group, 0);
        assert_eq!(sprites[1].image, 1);
    }
}
