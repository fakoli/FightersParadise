//! # SFF — Sprite File Format Parser
//!
//! Parses MUGEN SFF v2 sprite container files. An SFF file contains all sprites
//! for a character or stage, stored as 256-color indexed images with shared palettes.
//!
//! The SFF v2 format (introduced in MUGEN 1.0) uses a header-based structure with
//! separate data blocks for sprite pixels and palette colors. Sprites can reference
//! shared palettes and can be "linked" to share pixel data with other sprites.
//!
//! # Usage
//!
//! ```no_run
//! use std::path::Path;
//! use fp_formats::sff::SffFile;
//!
//! let sff = SffFile::load(Path::new("kfm.sff")).unwrap();
//! // Look up sprite (group=0, image=0)
//! if let Some(sprite) = sff.sprite(0, 0) {
//!     println!("{}x{}", sprite.width, sprite.height);
//! }
//! ```

mod compression;
mod header;
mod palette;
mod sprite;

pub use compression::{decompress_rle5, decompress_rle8, decompress_lz5, decompress_png};
pub use header::SffHeader;
pub use palette::{SffPalette, PALETTE_RGBA_SIZE};
pub use sprite::{SffSprite, SpriteFormat};

use fp_core::{FpError, FpResult};
use std::path::Path;

/// A fully loaded SFF v2 file.
///
/// Contains the parsed header, all sprite and palette sub-headers, and the raw
/// data blocks needed to decompress sprite pixels and resolve palette colors.
#[derive(Debug)]
pub struct SffFile {
    /// The parsed file header.
    pub header: SffHeader,
    /// All sprite sub-headers, in file order.
    pub sprites: Vec<SffSprite>,
    /// All palette sub-headers, in file order.
    pub palettes: Vec<SffPalette>,
    /// Raw literal data block (LData) — uncompressed palette and sprite data.
    ldata: Vec<u8>,
    /// Raw translate data block (TData) — compressed sprite data.
    tdata: Vec<u8>,
}

impl SffFile {
    /// Loads and parses an SFF v2 file from the given path.
    ///
    /// Reads the entire file into memory, parses the header, then all sprite
    /// and palette sub-headers. The raw data blocks are retained for on-demand
    /// sprite decompression.
    pub fn load(path: &Path) -> FpResult<Self> {
        let data = std::fs::read(path)?;
        Self::from_bytes(&data)
    }

    /// Parses an SFF v2 file from raw bytes already in memory.
    pub fn from_bytes(data: &[u8]) -> FpResult<Self> {
        let file_header = header::parse_header(data)?;

        // Parse sprite sub-headers
        let sprite_start = file_header.sprite_offset as usize;
        let sprite_end = sprite_start + file_header.sprite_length as usize;
        if sprite_end > data.len() {
            return Err(FpError::parse(
                "SFF",
                format!(
                    "sprite sub-header block extends past end of file ({sprite_end} > {})",
                    data.len()
                ),
            ));
        }
        let sprites =
            sprite::parse_all_sprites(&data[sprite_start..sprite_end], file_header.num_sprites)?;

        // Determine number of palettes from the palette block size
        let pal_start = file_header.palette_offset as usize;
        let pal_end = pal_start + file_header.palette_length as usize;
        if pal_end > data.len() {
            return Err(FpError::parse(
                "SFF",
                format!(
                    "palette sub-header block extends past end of file ({pal_end} > {})",
                    data.len()
                ),
            ));
        }
        let num_palettes =
            (file_header.palette_length as usize) / palette::PALETTE_SUBHEADER_SIZE;
        let palettes =
            palette::parse_all_palettes(&data[pal_start..pal_end], num_palettes as u32)?;

        // Extract LData block
        let ldata_start = file_header.ldata_offset as usize;
        let ldata_end = ldata_start + file_header.ldata_length as usize;
        let ldata = if ldata_end <= data.len() {
            data[ldata_start..ldata_end].to_vec()
        } else {
            tracing::warn!(
                ldata_end,
                file_len = data.len(),
                "LData block extends past end of file, truncating"
            );
            if ldata_start < data.len() {
                data[ldata_start..].to_vec()
            } else {
                Vec::new()
            }
        };

        // Extract TData block
        let tdata_start = file_header.tdata_offset as usize;
        let tdata_end = tdata_start + file_header.tdata_length as usize;
        let tdata = if tdata_end <= data.len() {
            data[tdata_start..tdata_end].to_vec()
        } else {
            tracing::warn!(
                tdata_end,
                file_len = data.len(),
                "TData block extends past end of file, truncating"
            );
            if tdata_start < data.len() {
                data[tdata_start..].to_vec()
            } else {
                Vec::new()
            }
        };

        Ok(Self {
            header: file_header,
            sprites,
            palettes,
            ldata,
            tdata,
        })
    }

    /// Looks up a sprite by group and image number.
    ///
    /// Returns `None` if no sprite with the given group/image pair exists.
    pub fn sprite(&self, group: u16, image: u16) -> Option<&SffSprite> {
        self.sprites
            .iter()
            .find(|s| s.group == group && s.image == image)
    }

    /// Decompresses the pixel data for the sprite at the given index.
    ///
    /// If the sprite is linked to another sprite, follows the link to obtain
    /// the actual pixel data. Returns the decompressed pixel data as a flat
    /// byte vector of palette indices (for 8-bit sprites).
    pub fn decode_sprite(&self, index: usize) -> FpResult<Vec<u8>> {
        let sprite = self.sprites.get(index).ok_or_else(|| {
            FpError::not_found("sprite", format!("index {index}"))
        })?;

        // Follow link if this sprite doesn't have its own data
        let data_sprite = if sprite.linked_index as usize != index && sprite.data_length == 0 {
            let linked = sprite.linked_index as usize;
            self.sprites.get(linked).ok_or_else(|| {
                FpError::parse(
                    "SFF",
                    format!("sprite {index} links to non-existent sprite {linked}"),
                )
            })?
        } else {
            sprite
        };

        // Get the raw compressed data from the appropriate data block
        let data_block = if data_sprite.uses_tdata() {
            &self.tdata
        } else {
            &self.ldata
        };

        let start = data_sprite.data_offset as usize;
        let end = start + data_sprite.data_length as usize;
        if end > data_block.len() {
            return Err(FpError::parse(
                "SFF",
                format!(
                    "sprite data [{start}..{end}] out of range for data block ({} bytes)",
                    data_block.len()
                ),
            ));
        }

        let compressed = &data_block[start..end];

        match data_sprite.format {
            SpriteFormat::Raw => Ok(compressed.to_vec()),
            SpriteFormat::Rle8 => compression::decompress_rle8(compressed),
            SpriteFormat::Rle5 => compression::decompress_rle5(compressed),
            SpriteFormat::Lz5 => compression::decompress_lz5(compressed),
            SpriteFormat::Png8 | SpriteFormat::Png24 | SpriteFormat::Png32 => {
                compression::decompress_png(compressed)
            }
            SpriteFormat::Invalid => Err(FpError::parse(
                "SFF",
                format!("sprite {index} has invalid format byte (1)"),
            )),
        }
    }

    /// Returns the RGBA palette data for the palette at the given index.
    ///
    /// Follows linked palettes if necessary. The returned array is 1024 bytes
    /// (256 colors * 4 bytes RGBA), with index 0 having alpha = 0 (transparent).
    pub fn palette(&self, index: usize) -> FpResult<[u8; PALETTE_RGBA_SIZE]> {
        let pal = self.palettes.get(index).ok_or_else(|| {
            FpError::not_found("palette", format!("index {index}"))
        })?;

        // Follow link if this palette doesn't have its own data
        let data_pal = if pal.linked_index as usize != index && pal.data_length == 0 {
            let linked = pal.linked_index as usize;
            self.palettes.get(linked).ok_or_else(|| {
                FpError::parse(
                    "SFF",
                    format!("palette {index} links to non-existent palette {linked}"),
                )
            })?
        } else {
            pal
        };

        let start = data_pal.data_offset as usize;
        let end = start + data_pal.data_length as usize;
        if end > self.ldata.len() {
            return Err(FpError::parse(
                "SFF",
                format!(
                    "palette data [{start}..{end}] out of range for LData ({} bytes)",
                    self.ldata.len()
                ),
            ));
        }

        palette::rgb_to_rgba(&self.ldata[start..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal synthetic SFF v2 file with one sprite and one palette.
    fn make_test_sff() -> Vec<u8> {
        // Layout:
        // [0..512)     header
        // [512..540)   1 sprite sub-header (28 bytes)
        // [540..556)   1 palette sub-header (16 bytes)
        // [556..1324)  LData: 768 bytes of RGB palette data
        // [1324..1335) TData: RLE8 compressed sprite data

        let sprite_offset: u32 = 512;
        let palette_offset: u32 = 540;
        let ldata_offset: u32 = 556;
        let ldata_length: u32 = 768;
        let tdata_offset: u32 = 556 + 768; // 1324
        // RLE8 data: size prefix (4 bytes) + 2 byte run = 6 bytes
        let tdata_length: u32 = 6;

        let total = tdata_offset as usize + tdata_length as usize;
        let mut buf = vec![0u8; total];

        // --- Header (real MUGEN 1.0 SFF v2 layout) ---
        // Offsets 16..36 are five reserved u32s (left zeroed). The directory
        // fields begin at offset 36 and store COUNTS, not byte-lengths, for the
        // sprite/palette tables — mirroring `header::make_test_header`.
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[12] = 0; buf[13] = 0; buf[14] = 1; buf[15] = 2; // version 2.1.0.0
        // sprite_offset @36
        buf[36..40].copy_from_slice(&sprite_offset.to_le_bytes());
        // num_sprites @40 (count, not byte length)
        buf[40..44].copy_from_slice(&1u32.to_le_bytes());
        // palette_offset @44
        buf[44..48].copy_from_slice(&palette_offset.to_le_bytes());
        // num_palettes @48 (count, not byte length)
        buf[48..52].copy_from_slice(&1u32.to_le_bytes());
        // ldata_offset @52
        buf[52..56].copy_from_slice(&ldata_offset.to_le_bytes());
        // ldata_length @56
        buf[56..60].copy_from_slice(&ldata_length.to_le_bytes());
        // tdata_offset @60
        buf[60..64].copy_from_slice(&tdata_offset.to_le_bytes());
        // tdata_length @64
        buf[64..68].copy_from_slice(&tdata_length.to_le_bytes());

        // --- Sprite sub-header at offset 512 ---
        let s = sprite_offset as usize;
        buf[s..s+2].copy_from_slice(&0u16.to_le_bytes());     // group
        buf[s+2..s+4].copy_from_slice(&0u16.to_le_bytes());   // image
        buf[s+4..s+6].copy_from_slice(&2u16.to_le_bytes());   // width = 2
        buf[s+6..s+8].copy_from_slice(&2u16.to_le_bytes());   // height = 2
        buf[s+8..s+10].copy_from_slice(&0i16.to_le_bytes());  // axis_x
        buf[s+10..s+12].copy_from_slice(&0i16.to_le_bytes()); // axis_y
        buf[s+12..s+14].copy_from_slice(&0u16.to_le_bytes()); // linked_index = self
        buf[s+14] = 2; // format = RLE8
        buf[s+15] = 8; // color_depth = 8
        buf[s+16..s+20].copy_from_slice(&0u32.to_le_bytes()); // data_offset within TData
        buf[s+20..s+24].copy_from_slice(&tdata_length.to_le_bytes()); // data_length
        buf[s+24..s+26].copy_from_slice(&0u16.to_le_bytes()); // palette_index
        buf[s+26..s+28].copy_from_slice(&1u16.to_le_bytes()); // flags: bit0=1 -> TData

        // --- Palette sub-header at offset 540 ---
        let p = palette_offset as usize;
        buf[p..p+2].copy_from_slice(&1u16.to_le_bytes());     // group
        buf[p+2..p+4].copy_from_slice(&1u16.to_le_bytes());   // item
        buf[p+4..p+6].copy_from_slice(&256u16.to_le_bytes()); // num_colors
        buf[p+6..p+8].copy_from_slice(&0u16.to_le_bytes());   // linked_index = self
        buf[p+8..p+12].copy_from_slice(&0u32.to_le_bytes());  // data_offset in LData
        buf[p+12..p+16].copy_from_slice(&768u32.to_le_bytes()); // data_length

        // --- LData: palette RGB data at offset 556 ---
        // Color 0: black (transparent). Color 1: red.
        let l = ldata_offset as usize;
        buf[l] = 0; buf[l+1] = 0; buf[l+2] = 0;     // color 0: black
        buf[l+3] = 255; buf[l+4] = 0; buf[l+5] = 0;  // color 1: red

        // --- TData: RLE8 data at offset 1324 ---
        // Decompressed size = 4 (2x2 pixels), run of 4 x color 1
        let t = tdata_offset as usize;
        buf[t..t+4].copy_from_slice(&4u32.to_le_bytes()); // decompressed size = 4
        buf[t+4] = 0x44; // bit6 set, lower 6 bits = 4 -> run of 4
        buf[t+5] = 0x01; // color = 1

        buf
    }

    #[test]
    fn load_synthetic_sff() {
        let data = make_test_sff();
        let sff = SffFile::from_bytes(&data).unwrap();

        assert_eq!(sff.sprites.len(), 1);
        assert_eq!(sff.palettes.len(), 1);
        assert_eq!(sff.header.num_sprites, 1);
    }

    #[test]
    fn lookup_sprite_by_group_image() {
        let data = make_test_sff();
        let sff = SffFile::from_bytes(&data).unwrap();

        let sprite = sff.sprite(0, 0).unwrap();
        assert_eq!(sprite.width, 2);
        assert_eq!(sprite.height, 2);

        assert!(sff.sprite(99, 99).is_none());
    }

    #[test]
    fn decode_sprite_pixels() {
        let data = make_test_sff();
        let sff = SffFile::from_bytes(&data).unwrap();

        let pixels = sff.decode_sprite(0).unwrap();
        assert_eq!(pixels.len(), 4); // 2x2
        assert!(pixels.iter().all(|&p| p == 1)); // all color index 1
    }

    #[test]
    fn get_palette_rgba() {
        let data = make_test_sff();
        let sff = SffFile::from_bytes(&data).unwrap();

        let rgba = sff.palette(0).unwrap();
        // Color 0: black, transparent
        assert_eq!(rgba[0], 0); // R
        assert_eq!(rgba[3], 0); // A = transparent
        // Color 1: red, opaque
        assert_eq!(rgba[4], 255); // R
        assert_eq!(rgba[5], 0);   // G
        assert_eq!(rgba[6], 0);   // B
        assert_eq!(rgba[7], 255); // A = opaque
    }
}
