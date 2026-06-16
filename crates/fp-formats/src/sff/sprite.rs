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

    /// A safe, zero-sized placeholder sprite used in place of a malformed entry.
    ///
    /// When a sprite sub-header cannot be parsed (truncated block, unknown format
    /// byte, etc.) the reader keeps a placeholder at that position rather than
    /// dropping the entry — `linked_index` values are **positional indices** into
    /// the sprite list, so removing an entry would shift every later index and
    /// corrupt the rest of the set. The placeholder owns no pixel data
    /// (`data_length == 0`) and links to itself, so it decodes to an empty buffer
    /// and is silently skipped by the renderer instead of garbling.
    pub fn placeholder(index: usize) -> Self {
        SffSprite {
            group: u16::MAX,
            image: u16::MAX,
            width: 0,
            height: 0,
            axis_x: 0,
            axis_y: 0,
            // Link to self so the decode path treats it as data-owning (and thus
            // never chases a link to a real sprite's pixels).
            linked_index: index as u16,
            format: SpriteFormat::Raw,
            color_depth: 8,
            data_offset: 0,
            data_length: 0,
            palette_index: 0,
            flags: 0,
        }
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
///
/// Robust to malformed entries: a sub-header that runs past the end of the block,
/// or that fails to parse (e.g. an unknown format byte), is replaced in place by a
/// safe [`SffSprite::placeholder`] and the walk continues, so the remaining valid
/// sprites still decode. The returned `Vec` always has exactly `count` entries so
/// positional `linked_index` references stay aligned. The whole parse only errors
/// out when *nothing* can be read at all.
pub fn parse_all_sprites(data: &[u8], count: u32) -> FpResult<Vec<SffSprite>> {
    let count = count as usize;

    let mut sprites = Vec::with_capacity(count);
    let mut bad = 0usize;
    for i in 0..count {
        let offset = i * SPRITE_SUBHEADER_SIZE;
        // Bounds-check this entry against the block before slicing. A sub-header
        // whose offset runs past the end of the block (truncated/garbage count)
        // becomes a placeholder rather than aborting the entire set.
        let sprite = match data.get(offset..offset + SPRITE_SUBHEADER_SIZE) {
            Some(slice) => match parse_sprite(slice) {
                Ok(sprite) => sprite,
                Err(err) => {
                    bad += 1;
                    tracing::warn!(
                        index = i,
                        error = %err,
                        "SFF v2: malformed sprite sub-header; substituting placeholder and continuing"
                    );
                    SffSprite::placeholder(i)
                }
            },
            None => {
                bad += 1;
                tracing::warn!(
                    index = i,
                    offset,
                    block_len = data.len(),
                    "SFF v2: sprite sub-header offset out of range; substituting placeholder and continuing"
                );
                SffSprite::placeholder(i)
            }
        };
        sprites.push(sprite);
    }

    if bad == count && count > 0 {
        return Err(FpError::parse(
            "SFF",
            format!("no valid sprite sub-headers in block ({count} declared, all malformed)"),
        ));
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

    /// T037: a declared count whose last sub-header runs past the end of the
    /// block must not abort the whole parse — the in-range sprite is parsed and
    /// the out-of-range one becomes a placeholder, keeping positional indices.
    #[test]
    fn out_of_range_subheader_offset_yields_placeholder() {
        // One valid 28-byte sub-header, but the block claims TWO sprites — the
        // second sub-header's bytes are entirely missing (offset out of range).
        let block = make_sprite_bytes(7, 3, 64, 64, 2, 0);
        assert_eq!(block.len(), SPRITE_SUBHEADER_SIZE);

        let sprites = parse_all_sprites(&block, 2).expect("parse must not abort on one bad entry");
        // Count is preserved so `linked_index` positions stay aligned.
        assert_eq!(sprites.len(), 2);

        // Sprite 0 is the real one.
        assert_eq!(sprites[0].group, 7);
        assert_eq!(sprites[0].image, 3);
        assert_eq!(sprites[0].width, 64);

        // Sprite 1 is the safe placeholder: zero-sized, no pixel data, self-linked.
        assert_eq!(sprites[1].width, 0);
        assert_eq!(sprites[1].height, 0);
        assert_eq!(sprites[1].data_length, 0);
        assert_eq!(sprites[1].linked_index, 1);
    }

    /// A sub-header with an unknown format byte is skipped (placeholder) instead
    /// of failing the whole set — the surrounding valid sprites survive.
    #[test]
    fn unknown_format_subheader_is_skipped_not_fatal() {
        let mut block = Vec::new();
        block.extend_from_slice(&make_sprite_bytes(0, 0, 64, 64, 2, 0)); // valid RLE8
        block.extend_from_slice(&make_sprite_bytes(0, 1, 32, 32, 99, 0)); // unknown fmt
        block.extend_from_slice(&make_sprite_bytes(0, 2, 16, 16, 0, 0)); // valid Raw

        let sprites = parse_all_sprites(&block, 3).unwrap();
        assert_eq!(sprites.len(), 3);
        assert_eq!(sprites[0].image, 0);
        assert_eq!(sprites[0].format, SpriteFormat::Rle8);
        // Middle entry replaced by a placeholder.
        assert_eq!(sprites[1].width, 0);
        assert_eq!(sprites[1].format, SpriteFormat::Raw);
        assert_eq!(sprites[1].linked_index, 1);
        // The sprite *after* the bad one is intact at its correct position.
        assert_eq!(sprites[2].image, 2);
        assert_eq!(sprites[2].width, 16);
    }

    /// When *every* declared sub-header is malformed the parse still errors,
    /// because there is genuinely nothing to render.
    #[test]
    fn all_subheaders_malformed_is_fatal() {
        // Declares two sprites but provides no sub-header bytes at all.
        let err = parse_all_sprites(&[], 2).unwrap_err();
        assert!(err.to_string().contains("no valid sprite sub-headers"));
    }
}
