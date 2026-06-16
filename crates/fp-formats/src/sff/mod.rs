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
mod v1;

pub use compression::{
    decode_png, decompress_lz5, decompress_png, decompress_rle5, decompress_rle8, DecodedPng,
};
pub use header::SffHeader;
pub use palette::{SffPalette, PALETTE_RGBA_SIZE};
pub use sprite::{SffSprite, SpriteFormat};
pub use v1::{decode_pcx_8bit, SffV1Header};

use fp_core::{FpError, FpResult};
use std::path::Path;

/// Which SFF container format a loaded file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SffVersion {
    /// SFF v1 — inline PCX images (MUGEN 2002 / WinMUGEN era).
    V1,
    /// SFF v2 — LData/TData blocks with RLE/LZ5/PNG sprites (MUGEN 1.0+).
    V2,
}

/// A fully loaded SFF file (v1 or v2).
///
/// Contains the parsed header, all sprite and palette sub-headers, and the raw
/// data blocks needed to decompress sprite pixels and resolve palette colors.
/// For SFF v1 the sprites are inline PCX images stored in the single backing
/// buffer; for SFF v2 they live in the LData/TData blocks.
#[derive(Debug)]
pub struct SffFile {
    /// Which container format this file uses.
    pub version: SffVersion,
    /// The parsed file header.
    ///
    /// For SFF v1 the version fields are populated and the v2-specific block
    /// offsets are left zeroed (v1 has no LData/TData blocks).
    pub header: SffHeader,
    /// All sprite sub-headers, in file order.
    pub sprites: Vec<SffSprite>,
    /// All palette sub-headers, in file order.
    ///
    /// For SFF v2 these come from the file's palette table. For SFF v1, where each
    /// sprite carries its own 256-colour palette as the trailing bytes of its
    /// inline PCX image, one entry is synthesized per sprite that owns pixel data
    /// with an extractable trailing palette, and each sprite's `palette_index`
    /// points at the entry it should use. Only data-less (linked) sprites — or
    /// data-owning sprites whose PCX is too short/corrupt to yield a palette —
    /// reuse the previous entry; the sub-header's byte-18 "shared" flag is **not**
    /// consulted (see [`v1::parse_v1_sprites`]).
    pub palettes: Vec<SffPalette>,
    /// Raw literal data block (LData) — uncompressed palette and sprite data.
    /// For SFF v1 this holds the entire file (sprites are inline PCX images).
    ldata: Vec<u8>,
    /// Raw translate data block (TData) — compressed sprite data (SFF v2 only).
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

    /// Parses an SFF file (v1 or v2) from raw bytes already in memory.
    ///
    /// The container version is detected from the header's major-version byte:
    /// v1 files take the inline-PCX path, v2 files the LData/TData path.
    pub fn from_bytes(data: &[u8]) -> FpResult<Self> {
        match detect_version(data)? {
            SffVersion::V1 => Self::from_bytes_v1(data),
            SffVersion::V2 => Self::from_bytes_v2(data),
        }
    }

    /// Parses an SFF v1 file (inline PCX images) from raw bytes.
    fn from_bytes_v1(data: &[u8]) -> FpResult<Self> {
        let v1_header = v1::parse_v1_header(data)?;
        let (sprites, palettes) = v1::parse_v1_sprites(data, &v1_header)?;

        // Synthesize an `SffHeader` so the public `header` field stays valid.
        // SFF v1 has no separate LData/TData blocks, so those offsets are zero.
        let header = SffHeader {
            version_major: 1,
            version_minor1: 0,
            version_minor2: 0,
            version_minor3: 0,
            num_groups: v1_header.num_groups,
            num_sprites: v1_header.num_images,
            sprite_offset: v1_header.first_subheader_offset,
            sprite_length: 0,
            palette_offset: 0,
            palette_length: 0,
            ldata_offset: 0,
            ldata_length: data.len() as u32,
            tdata_offset: 0,
            tdata_length: 0,
        };

        tracing::info!(
            "SFF v1: loaded {} sprites, {} palettes",
            sprites.len(),
            palettes.len()
        );

        Ok(Self {
            version: SffVersion::V1,
            header,
            sprites,
            // Per-sprite trailing PCX palettes, extracted by `parse_v1_sprites`.
            palettes,
            // The whole file is the backing buffer; sprite offsets are absolute.
            ldata: data.to_vec(),
            tdata: Vec::new(),
        })
    }

    /// Parses an SFF v2 file from raw bytes already in memory.
    ///
    /// Robust to malformed/edge sub-headers: a single sprite whose sub-header is
    /// out of range or otherwise unparseable is replaced by a safe placeholder
    /// (see [`sprite::parse_all_sprites`]) so the file's *other* sprites still
    /// load and decode instead of the whole character degrading to a garbled
    /// block. A bad per-sprite **data** offset is likewise non-fatal — it surfaces
    /// as a recoverable error from [`Self::decode_sprite`]/[`Self::decode_sprite_rgba`]
    /// for that one sprite only.
    fn from_bytes_v2(data: &[u8]) -> FpResult<Self> {
        let file_header = header::parse_header(data)?;

        // Parse sprite sub-headers. The declared count can be larger than the
        // sub-header block actually holds (truncated/garbage table); clamp the
        // slice to the bytes that exist and let `parse_all_sprites` substitute a
        // placeholder for each out-of-range entry rather than aborting the whole
        // file — the valid sprites still load.
        let sprite_start = file_header.sprite_offset as usize;
        let sprite_end = (sprite_start + file_header.sprite_length as usize).min(data.len());
        let sprite_block = if sprite_start <= sprite_end {
            &data[sprite_start..sprite_end]
        } else {
            tracing::warn!(
                sprite_start,
                file_len = data.len(),
                "SFF v2: sprite sub-header block starts past end of file; treating as empty"
            );
            &[]
        };
        if sprite_end < sprite_start + file_header.sprite_length as usize {
            tracing::warn!(
                declared = file_header.num_sprites,
                available_bytes = sprite_block.len(),
                "SFF v2: sprite sub-header block truncated; out-of-range sprites become placeholders"
            );
        }
        let sprites = sprite::parse_all_sprites(sprite_block, file_header.num_sprites)?;

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
        let num_palettes = (file_header.palette_length as usize) / palette::PALETTE_SUBHEADER_SIZE;
        let palettes = palette::parse_all_palettes(&data[pal_start..pal_end], num_palettes as u32)?;

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
            version: SffVersion::V2,
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
    ///
    /// Truecolor PNG24/PNG32 sprites have no palette indices and so cannot be
    /// represented here; this returns a recoverable error for them. Use
    /// [`Self::decode_sprite_rgba`] to obtain their pixels as RGBA.
    pub fn decode_sprite(&self, index: usize) -> FpResult<Vec<u8>> {
        let sprite = self
            .sprites
            .get(index)
            .ok_or_else(|| FpError::not_found("sprite", format!("index {index}")))?;

        // SFF v1 stores each sprite as an inline PCX image inside the backing
        // buffer (`ldata` holds the whole file). Decode that directly.
        if self.version == SffVersion::V1 {
            return self.decode_sprite_v1(index, sprite);
        }

        let (data_sprite, compressed) = self.sprite_compressed_bytes(index, sprite)?;

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

    /// Resolves a sprite's raw compressed bytes, following a data link if needed.
    ///
    /// Returns the (possibly linked) sprite that actually owns the data along with
    /// a slice of its raw bytes within the appropriate data block (LData/TData).
    /// SFF v1 sprites are handled separately and must not call this.
    fn sprite_compressed_bytes<'a>(
        &'a self,
        index: usize,
        sprite: &'a SffSprite,
    ) -> FpResult<(&'a SffSprite, &'a [u8])> {
        // Follow link if this sprite doesn't have its own data.
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

        // Get the raw compressed data from the appropriate data block.
        let data_block = if data_sprite.uses_tdata() {
            &self.tdata
        } else {
            &self.ldata
        };

        // Use checked arithmetic for the end offset: a garbage `data_offset`
        // near `u32::MAX` plus a large `data_length` would otherwise overflow on
        // 32-bit targets. An overflow is treated the same as out-of-range — a
        // recoverable per-sprite error the caller skips, never a panic.
        let start = data_sprite.data_offset as usize;
        let end = start.checked_add(data_sprite.data_length as usize);
        match end {
            Some(end) if end <= data_block.len() => Ok((data_sprite, &data_block[start..end])),
            _ => Err(FpError::parse(
                "SFF",
                format!(
                    "sprite data [{start}..+{}] out of range for data block ({} bytes)",
                    data_sprite.data_length,
                    data_block.len()
                ),
            )),
        }
    }

    /// Decodes the sprite at `index` to flat RGBA pixels (`width * height * 4`).
    ///
    /// This resolves the sprite end to end, applying the appropriate palette:
    /// - **Indexed sprites** (Raw/RLE8/RLE5/LZ5, SFF v1 PCX, and indexed PNG8):
    ///   the decoded palette indices are mapped through the sprite's palette.
    ///   PNG8 uses its *embedded* `PLTE` palette; all other indexed formats use
    ///   the SFF palette table entry at the sprite's `palette_index`.
    /// - **Truecolor PNG24 / PNG32**: returned directly as the PNG's RGBA (PNG24
    ///   gets opaque alpha), since these carry no palette.
    ///
    /// Index 0 of an indexed palette is transparent (MUGEN convention). Never
    /// panics; bad content yields a recoverable [`FpError`].
    pub fn decode_sprite_rgba(&self, index: usize) -> FpResult<Vec<u8>> {
        let sprite = self
            .sprites
            .get(index)
            .ok_or_else(|| FpError::not_found("sprite", format!("index {index}")))?;

        // SFF v1: indexed PCX through the sprite's extracted palette.
        if self.version == SffVersion::V1 {
            let indices = self.decode_sprite_v1(index, sprite)?;
            let pal = self.palette(sprite.palette_index as usize)?;
            return Ok(indices_to_rgba(&indices, &pal));
        }

        let (data_sprite, compressed) = self.sprite_compressed_bytes(index, sprite)?;

        match data_sprite.format {
            SpriteFormat::Png8 | SpriteFormat::Png24 | SpriteFormat::Png32 => {
                match compression::decode_png(compressed)? {
                    compression::DecodedPng::Indexed {
                        pixels, palette, ..
                    } => Ok(indices_to_rgba(&pixels, &palette)),
                    compression::DecodedPng::TrueColor { rgba, .. } => Ok(rgba),
                }
            }
            SpriteFormat::Invalid => Err(FpError::parse(
                "SFF",
                format!("sprite {index} has invalid format byte (1)"),
            )),
            // Raw/RLE8/RLE5/LZ5: decode indices, then apply the SFF palette.
            _ => {
                let indices = self.decode_sprite(index)?;
                let pal = self.palette(data_sprite.palette_index as usize)?;
                Ok(indices_to_rgba(&indices, &pal))
            }
        }
    }

    /// Decodes the sprite at `index` to RGBA using a caller-supplied external
    /// palette instead of the sprite's own SFF palette.
    ///
    /// This is the format-layer hook for MUGEN's **alternate-color** mechanism:
    /// a character's `pal1`..`pal12` `.act` files (see [`crate::act::ActPalette`])
    /// supply a replacement 256-colour table that re-tints the same indexed
    /// pixels — that is how palette-swapped player 2 colours are produced. Pass
    /// the chosen palette's 1024-byte RGBA buffer (e.g. `ActPalette::rgba`) and
    /// the indexed sprite is remapped through it.
    ///
    /// Applies only to **indexed** sprites (Raw/RLE8/RLE5/LZ5, SFF v1 PCX, and
    /// indexed PNG8) — those are the formats an external palette can re-tint.
    /// Truecolor PNG24/PNG32 sprites carry their own colour and ignore the
    /// palette entirely, decoding exactly as [`Self::decode_sprite_rgba`] would.
    ///
    /// `palette` should be 1024 bytes (256 × RGBA); a shorter buffer simply
    /// renders the out-of-range indices transparent (it never panics). Index 0
    /// remains transparent per the MUGEN convention encoded in the palette's
    /// own alpha byte.
    pub fn decode_sprite_rgba_with_palette(
        &self,
        index: usize,
        palette: &[u8],
    ) -> FpResult<Vec<u8>> {
        let sprite = self
            .sprites
            .get(index)
            .ok_or_else(|| FpError::not_found("sprite", format!("index {index}")))?;

        // SFF v1: inline PCX indices remapped through the external palette.
        if self.version == SffVersion::V1 {
            let indices = self.decode_sprite_v1(index, sprite)?;
            return Ok(indices_to_rgba(&indices, palette));
        }

        let (data_sprite, compressed) = self.sprite_compressed_bytes(index, sprite)?;

        match data_sprite.format {
            // Indexed PNG8 keeps its decoded indices but re-tints through the
            // external palette; truecolor PNGs carry their own colour and so are
            // returned unchanged (an external palette cannot re-tint them).
            SpriteFormat::Png8 | SpriteFormat::Png24 | SpriteFormat::Png32 => {
                match compression::decode_png(compressed)? {
                    compression::DecodedPng::Indexed { pixels, .. } => {
                        Ok(indices_to_rgba(&pixels, palette))
                    }
                    compression::DecodedPng::TrueColor { rgba, .. } => Ok(rgba),
                }
            }
            SpriteFormat::Invalid => Err(FpError::parse(
                "SFF",
                format!("sprite {index} has invalid format byte (1)"),
            )),
            // Raw/RLE8/RLE5/LZ5: decode indices, then apply the external palette.
            _ => {
                let indices = self.decode_sprite(index)?;
                Ok(indices_to_rgba(&indices, palette))
            }
        }
    }

    /// Decodes a single SFF v1 sprite (inline 8-bit PCX image).
    ///
    /// Linked sprites (zero data length) resolve through `linked_index` to the
    /// referenced sprite's PCX data.
    fn decode_sprite_v1(&self, index: usize, sprite: &SffSprite) -> FpResult<Vec<u8>> {
        // Resolve links: a zero-length sprite reuses an earlier sprite's pixels.
        let data_sprite = if sprite.data_length == 0 {
            let linked = sprite.linked_index as usize;
            self.sprites.get(linked).ok_or_else(|| {
                FpError::parse(
                    "SFF",
                    format!("v1 sprite {index} links to non-existent sprite {linked}"),
                )
            })?
        } else {
            sprite
        };

        let start = data_sprite.data_offset as usize;
        let end = start + data_sprite.data_length as usize;
        if end > self.ldata.len() {
            return Err(FpError::parse(
                "SFF",
                format!(
                    "v1 sprite data [{start}..{end}] out of range for file ({} bytes)",
                    self.ldata.len()
                ),
            ));
        }

        v1::decode_pcx_8bit(&self.ldata[start..end])
    }

    /// Returns the RGBA palette data for the palette at the given index.
    ///
    /// Follows linked palettes if necessary. The returned array is 1024 bytes
    /// (256 colors * 4 bytes RGBA), with index 0 having alpha = 0 (transparent).
    ///
    /// The on-disk encoding is **version-dependent**:
    /// - **SFF v1** palettes are the trailing 256-color VGA palette of an inline
    ///   PCX image: 768 bytes of **RGB** triplets, expanded to RGBA here.
    /// - **SFF v2** palettes are stored as `num_colors` **RGBA** quadruplets
    ///   (4 bytes per color) in the LData block — a palette may carry fewer than
    ///   256 colors (e.g. KFM's 32-color per-sprite palettes). Reading these
    ///   through the v1 RGB path is the bug that rendered v2 characters as black
    ///   silhouettes; the version split below is the fix.
    pub fn palette(&self, index: usize) -> FpResult<[u8; PALETTE_RGBA_SIZE]> {
        let pal = self
            .palettes
            .get(index)
            .ok_or_else(|| FpError::not_found("palette", format!("index {index}")))?;

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

        // A zero-length palette is the synthesized safe default (e.g. an SFF v1
        // sprite whose PCX carried no extractable palette). Resolve it to an
        // all-black, fully-transparent palette rather than erroring.
        if data_pal.data_length == 0 {
            return Ok([0u8; PALETTE_RGBA_SIZE]);
        }

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

        let raw = &self.ldata[start..end];
        match self.version {
            // SFF v1: 768-byte RGB trailing PCX palette. Unchanged from before.
            SffVersion::V1 => palette::rgb_to_rgba(raw),
            // SFF v2: `num_colors` RGBA quadruplets. The sub-header's
            // `data_length` already sizes the slice (e.g. 128 bytes for a
            // 32-color palette); `rgba_to_palette` clamps internally so a short
            // or oversized sub-header is handled safely.
            SffVersion::V2 => Ok(palette::rgba_to_palette(raw, data_pal.num_colors)),
        }
    }
}

/// Maps palette indices to RGBA using a 1024-byte (256 * RGBA) palette.
///
/// Each index selects four bytes from `palette`; out-of-range indices (palette
/// shorter than 256 colours) fall back to transparent black. Used by
/// [`SffFile::decode_sprite_rgba`].
fn indices_to_rgba(indices: &[u8], palette: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(indices.len() * 4);
    for &idx in indices {
        let base = idx as usize * 4;
        if base + 4 <= palette.len() {
            rgba.extend_from_slice(&palette[base..base + 4]);
        } else {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        }
    }
    rgba
}

/// The `ElecbyteSpr\0` signature shared by both SFF v1 and v2 files.
const SFF_SIGNATURE: &[u8; 12] = b"ElecbyteSpr\0";

/// Detects whether `data` is an SFF v1 or v2 file.
///
/// Both formats begin with the same 12-byte signature followed by four version
/// bytes; the high-order byte (offset 15) is `1` for v1 and `2` for v2.
fn detect_version(data: &[u8]) -> FpResult<SffVersion> {
    if data.len() < 16 {
        return Err(FpError::parse(
            "SFF",
            format!(
                "file too small for SFF header: {} bytes (need 16)",
                data.len()
            ),
        ));
    }
    if &data[0..12] != SFF_SIGNATURE.as_slice() {
        return Err(FpError::parse(
            "SFF",
            "invalid file signature (expected 'ElecbyteSpr\\0')",
        ));
    }
    match data[15] {
        1 => Ok(SffVersion::V1),
        2 => Ok(SffVersion::V2),
        other => Err(FpError::parse(
            "SFF",
            format!("unsupported SFF version {other} (expected 1 or 2)"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal synthetic SFF v2 file with one sprite and one palette.
    ///
    /// The palette is a full 256-color **RGBA** table (1024 bytes), matching the
    /// real SFF v2 on-disk encoding (4 bytes/color) rather than v1's RGB.
    fn make_test_sff() -> Vec<u8> {
        // Layout:
        // [0..512)     header
        // [512..540)   1 sprite sub-header (28 bytes)
        // [540..556)   1 palette sub-header (16 bytes)
        // [556..1580)  LData: 1024 bytes of RGBA palette data (256 * 4)
        // [1580..1586) TData: RLE8 compressed sprite data

        let sprite_offset: u32 = 512;
        let palette_offset: u32 = 540;
        let ldata_offset: u32 = 556;
        let ldata_length: u32 = 1024; // 256 colors * 4 bytes (RGBA)
        let tdata_offset: u32 = 556 + 1024; // 1580
                                            // RLE8 data: size prefix (4 bytes) + 2 byte run = 6 bytes
        let tdata_length: u32 = 6;

        let total = tdata_offset as usize + tdata_length as usize;
        let mut buf = vec![0u8; total];

        // --- Header (real MUGEN 1.0 SFF v2 layout) ---
        // Offsets 16..36 are five reserved u32s (left zeroed). The directory
        // fields begin at offset 36 and store COUNTS, not byte-lengths, for the
        // sprite/palette tables — mirroring `header::make_test_header`.
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[12] = 0;
        buf[13] = 0;
        buf[14] = 1;
        buf[15] = 2; // version 2.1.0.0
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
        buf[s..s + 2].copy_from_slice(&0u16.to_le_bytes()); // group
        buf[s + 2..s + 4].copy_from_slice(&0u16.to_le_bytes()); // image
        buf[s + 4..s + 6].copy_from_slice(&2u16.to_le_bytes()); // width = 2
        buf[s + 6..s + 8].copy_from_slice(&2u16.to_le_bytes()); // height = 2
        buf[s + 8..s + 10].copy_from_slice(&0i16.to_le_bytes()); // axis_x
        buf[s + 10..s + 12].copy_from_slice(&0i16.to_le_bytes()); // axis_y
        buf[s + 12..s + 14].copy_from_slice(&0u16.to_le_bytes()); // linked_index = self
        buf[s + 14] = 2; // format = RLE8
        buf[s + 15] = 8; // color_depth = 8
        buf[s + 16..s + 20].copy_from_slice(&0u32.to_le_bytes()); // data_offset within TData
        buf[s + 20..s + 24].copy_from_slice(&tdata_length.to_le_bytes()); // data_length
        buf[s + 24..s + 26].copy_from_slice(&0u16.to_le_bytes()); // palette_index
        buf[s + 26..s + 28].copy_from_slice(&1u16.to_le_bytes()); // flags: bit0=1 -> TData

        // --- Palette sub-header at offset 540 ---
        let p = palette_offset as usize;
        buf[p..p + 2].copy_from_slice(&1u16.to_le_bytes()); // group
        buf[p + 2..p + 4].copy_from_slice(&1u16.to_le_bytes()); // item
        buf[p + 4..p + 6].copy_from_slice(&256u16.to_le_bytes()); // num_colors
        buf[p + 6..p + 8].copy_from_slice(&0u16.to_le_bytes()); // linked_index = self
        buf[p + 8..p + 12].copy_from_slice(&0u32.to_le_bytes()); // data_offset in LData
        buf[p + 12..p + 16].copy_from_slice(&1024u32.to_le_bytes()); // data_length (RGBA)

        // --- LData: palette RGBA data at offset 556 ---
        // SFF v2 stores 4 bytes per color. Color 0: black (forced transparent).
        // Color 1: red. The on-disk 4th byte is reserved padding (set to 0 here
        // on purpose) — the decoder must force non-zero indices to opaque, not
        // honor this stored byte, or every sprite would render invisible.
        let l = ldata_offset as usize;
        buf[l] = 0;
        buf[l + 1] = 0;
        buf[l + 2] = 0;
        buf[l + 3] = 0; // color 0: black
        buf[l + 4] = 255;
        buf[l + 5] = 0;
        buf[l + 6] = 0;
        buf[l + 7] = 0; // color 1: red, pad=0

        // --- TData: RLE8 data at offset 1324 ---
        // Decompressed size = 4 (2x2 pixels), run of 4 x color 1
        let t = tdata_offset as usize;
        buf[t..t + 4].copy_from_slice(&4u32.to_le_bytes()); // decompressed size = 4
        buf[t + 4] = 0x44; // bit6 set, lower 6 bits = 4 -> run of 4
        buf[t + 5] = 0x01; // color = 1

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
        assert_eq!(rgba[5], 0); // G
        assert_eq!(rgba[6], 0); // B
                                // A = opaque even though the on-disk 4th byte (padding) was 0: the v2
                                // decoder must not honor the stored alpha or every sprite goes invisible.
        assert_eq!(rgba[7], 255);
    }

    /// Regression for the KFM "black silhouette" bug (task #32): an SFF **v2**
    /// palette is RGBA-encoded (4 bytes/color) and must NOT be read through the
    /// v1 RGB→RGBA path. With a single non-black, opaque color the resolved
    /// palette must expose at least one fully-opaque, non-black, non-index-0
    /// color — exactly the acceptance criterion for color rendering.
    #[test]
    fn v2_palette_rgba_is_non_degenerate() {
        let data = make_test_sff();
        let sff = SffFile::from_bytes(&data).unwrap();
        assert_eq!(sff.version, SffVersion::V2);

        let pal = sff.palette(0).unwrap();
        assert_eq!(pal.len(), PALETTE_RGBA_SIZE);

        // Index 0 must be transparent (alpha 0).
        assert_eq!(pal[3], 0, "index 0 must be transparent");

        // There must be at least one non-index-0 color that is non-black AND
        // fully opaque. (The fixture's color 1 is red.)
        let has_real_color = (1..256).any(|i| {
            let b = i * 4;
            let (r, g, bl, a) = (pal[b], pal[b + 1], pal[b + 2], pal[b + 3]);
            (r != 0 || g != 0 || bl != 0) && a == 255
        });
        assert!(
            has_real_color,
            "v2 palette must have a non-black, fully-opaque color besides index 0"
        );

        // Spot-check the exact bytes for color index 1 (red, opaque).
        assert_eq!(&pal[4..8], &[255, 0, 0, 255]);
    }

    /// Compat-matrix gap #1: SFF **v2.00** vs **v2.01** decode parity. The two
    /// minor revisions share an identical container/decode layout — only the
    /// minor-version byte differs — so a file decoded as v2.00 and the same file
    /// decoded as v2.01 must yield byte-identical sprites and palettes. This
    /// locks the "v2.01 = v2.00 for our purposes" claim into the test suite.
    #[test]
    fn sff_v2_minor_versions_decode_identically() {
        // The fixture is version 2.1.0.0 (minor1 byte at offset 14 = 1).
        let mut v200 = make_test_sff();
        v200[14] = 0; // minor1 = 0  -> "SFF v2.00"
        let mut v201 = make_test_sff();
        v201[14] = 1; // minor1 = 1  -> "SFF v2.01"

        let a = SffFile::from_bytes(&v200).expect("v2.00 must load");
        let b = SffFile::from_bytes(&v201).expect("v2.01 must load");

        // The detected major version is the same; the minor byte is the only
        // difference and it must not change decoding.
        assert_eq!(a.version, SffVersion::V2);
        assert_eq!(b.version, SffVersion::V2);
        assert_eq!(a.header.version_minor1, 0);
        assert_eq!(b.header.version_minor1, 1);

        // Decoded pixels and resolved palette are byte-for-byte identical.
        assert_eq!(
            a.decode_sprite(0).unwrap(),
            b.decode_sprite(0).unwrap(),
            "v2.00 and v2.01 must decode the same sprite pixels"
        );
        assert_eq!(
            a.decode_sprite_rgba(0).unwrap(),
            b.decode_sprite_rgba(0).unwrap(),
            "v2.00 and v2.01 must resolve the same RGBA sprite"
        );
        assert_eq!(
            a.palette(0).unwrap(),
            b.palette(0).unwrap(),
            "v2.00 and v2.01 must resolve the same palette"
        );
    }

    /// Compat-matrix gap #6: external-palette (alt-color / `.act`) decode. An
    /// indexed sprite must re-tint through a caller-supplied palette via
    /// [`SffFile::decode_sprite_rgba_with_palette`] without touching the SFF's
    /// own palette — the format-layer hook for `pal1`..`pal12` palette swaps.
    #[test]
    fn decode_indexed_sprite_with_external_palette() {
        let data = make_test_sff();
        let sff = SffFile::from_bytes(&data).unwrap();

        // Build an external 256-color RGBA palette where index 1 is green
        // (the in-SFF palette has index 1 = red). The 2x2 sprite is all index 1.
        let mut ext = vec![0u8; PALETTE_RGBA_SIZE];
        ext[4] = 0; // R
        ext[5] = 255; // G
        ext[6] = 0; // B
        ext[7] = 255; // A (opaque)

        let rgba = sff.decode_sprite_rgba_with_palette(0, &ext).unwrap();
        assert_eq!(rgba.len(), 2 * 2 * 4);
        // Every pixel must take the EXTERNAL palette's green, not the SFF's red.
        for px in rgba.chunks_exact(4) {
            assert_eq!(px, &[0, 255, 0, 255], "pixel must use the external palette");
        }

        // Sanity: the default (in-SFF) path still yields the SFF's red, proving
        // the external palette did not mutate or shadow the file's own table.
        let default_rgba = sff.decode_sprite_rgba(0).unwrap();
        assert_eq!(&default_rgba[0..4], &[255, 0, 0, 255]);
    }

    /// Builds a synthetic SFF v2 file with TWO sprites that share one palette,
    /// where the **second** sprite's `data_offset` is deliberately out of range
    /// for the TData block. Sprite 0 is a valid 2x2 RLE8 sprite; sprite 1 points
    /// past the end of TData and must therefore be skipped, not garble the set.
    ///
    /// Layout:
    /// ```text
    /// [0..512)      header
    /// [512..540)    sprite 0 sub-header (28 bytes)  -> valid, TData offset 0
    /// [540..568)    sprite 1 sub-header (28 bytes)  -> bad, TData offset 0xFF00
    /// [568..584)    palette sub-header (16 bytes)
    /// [584..1608)   LData: 1024 bytes RGBA palette
    /// [1608..1614)  TData: RLE8 data for sprite 0 only
    /// ```
    fn make_sff_with_bad_offset_sprite() -> Vec<u8> {
        let sprite_offset: u32 = 512;
        let palette_offset: u32 = 568;
        let ldata_offset: u32 = 584;
        let ldata_length: u32 = 1024;
        let tdata_offset: u32 = 584 + 1024; // 1608
        let tdata_length: u32 = 6;

        let total = tdata_offset as usize + tdata_length as usize;
        let mut buf = vec![0u8; total];

        // --- Header ---
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[14] = 1;
        buf[15] = 2; // version 2.1.0.0
        buf[36..40].copy_from_slice(&sprite_offset.to_le_bytes());
        buf[40..44].copy_from_slice(&2u32.to_le_bytes()); // num_sprites = 2
        buf[44..48].copy_from_slice(&palette_offset.to_le_bytes());
        buf[48..52].copy_from_slice(&1u32.to_le_bytes()); // num_palettes = 1
        buf[52..56].copy_from_slice(&ldata_offset.to_le_bytes());
        buf[56..60].copy_from_slice(&ldata_length.to_le_bytes());
        buf[60..64].copy_from_slice(&tdata_offset.to_le_bytes());
        buf[64..68].copy_from_slice(&tdata_length.to_le_bytes());

        // Helper to write one sprite sub-header at byte offset `s`.
        let write_sprite = |buf: &mut [u8], s: usize, group: u16, data_off: u32| {
            buf[s..s + 2].copy_from_slice(&group.to_le_bytes()); // group
            buf[s + 2..s + 4].copy_from_slice(&0u16.to_le_bytes()); // image
            buf[s + 4..s + 6].copy_from_slice(&2u16.to_le_bytes()); // width = 2
            buf[s + 6..s + 8].copy_from_slice(&2u16.to_le_bytes()); // height = 2
            buf[s + 8..s + 10].copy_from_slice(&0i16.to_le_bytes()); // axis_x
            buf[s + 10..s + 12].copy_from_slice(&0i16.to_le_bytes()); // axis_y
            buf[s + 12..s + 14].copy_from_slice(&(((s - 512) / 28) as u16).to_le_bytes()); // linked = self
            buf[s + 14] = 2; // format = RLE8
            buf[s + 15] = 8; // color_depth
            buf[s + 16..s + 20].copy_from_slice(&data_off.to_le_bytes()); // data_offset in TData
            buf[s + 20..s + 24].copy_from_slice(&6u32.to_le_bytes()); // data_length
            buf[s + 24..s + 26].copy_from_slice(&0u16.to_le_bytes()); // palette_index
            buf[s + 26..s + 28].copy_from_slice(&1u16.to_le_bytes()); // flags: bit0 -> TData
        };

        // Sprite 0: valid, TData offset 0.
        write_sprite(&mut buf, sprite_offset as usize, 0, 0);
        // Sprite 1: data_offset 0xFF00 is way past the 6-byte TData block.
        write_sprite(&mut buf, sprite_offset as usize + 28, 1, 0xFF00);

        // --- Palette sub-header ---
        let p = palette_offset as usize;
        buf[p..p + 2].copy_from_slice(&1u16.to_le_bytes()); // group
        buf[p + 2..p + 4].copy_from_slice(&1u16.to_le_bytes()); // item
        buf[p + 4..p + 6].copy_from_slice(&256u16.to_le_bytes()); // num_colors
        buf[p + 6..p + 8].copy_from_slice(&0u16.to_le_bytes()); // linked = self
        buf[p + 8..p + 12].copy_from_slice(&0u32.to_le_bytes()); // data_offset in LData
        buf[p + 12..p + 16].copy_from_slice(&1024u32.to_le_bytes()); // data_length

        // --- LData: palette (color 1 = red, opaque after decode) ---
        let l = ldata_offset as usize;
        buf[l + 4] = 255; // color 1 R

        // --- TData: RLE8 run of 4 x color 1 (decompresses to a 2x2 sprite) ---
        let t = tdata_offset as usize;
        buf[t..t + 4].copy_from_slice(&4u32.to_le_bytes()); // decompressed size = 4
        buf[t + 4] = 0x44; // bit6 set, lower 6 bits = 4 -> run of 4
        buf[t + 5] = 0x01; // color = 1

        buf
    }

    /// T037 acceptance #1 & #2: an SFF v2 file whose one sprite has an
    /// out-of-range data offset must still load, decode its *valid* sprites, and
    /// only fail (recoverably, never panicking) on the single bad sprite.
    #[test]
    fn v2_out_of_range_sprite_offset_skips_only_bad_one() {
        let data = make_sff_with_bad_offset_sprite();
        let sff = SffFile::from_bytes(&data).expect("file must still load with one bad sprite");

        // Both sub-headers parsed (positions preserved), so the count is intact.
        assert_eq!(sff.sprites.len(), 2);

        // The valid sprite (index 0) decodes to its 2x2 red pixels.
        let pixels = sff.decode_sprite(0).expect("valid sprite must decode");
        assert_eq!(pixels.len(), 4);
        assert!(pixels.iter().all(|&p| p == 1));

        let rgba = sff
            .decode_sprite_rgba(0)
            .expect("valid sprite must resolve to RGBA");
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);

        // The bad sprite (index 1) returns a recoverable error — NOT a panic, and
        // NOT corrupted garbage that aliases another sprite's pixels.
        let bad = sff.decode_sprite(1);
        assert!(
            bad.is_err(),
            "out-of-range data offset must be a clean error"
        );
        assert!(sff.decode_sprite_rgba(1).is_err());
    }

    /// T037: a declared sprite count larger than the sub-header block the file
    /// can physically hold (the sprite table runs past EOF) loads the in-range
    /// sprites and fills the rest with placeholders rather than failing the whole
    /// file. This is the path that previously returned a hard "sprite sub-header
    /// block extends past end of file" error and degraded the entire character.
    #[test]
    fn v2_truncated_sprite_table_loads_valid_sprites() {
        // Build a minimal file whose body ends right after sprite 0's 28-byte
        // sub-header, then claim THREE sprites in the header. Sprites 1 and 2's
        // bytes do not exist in the file at all.
        let sprite_offset: u32 = 512;
        // File ends immediately after one sub-header: no palette/LData/TData.
        let total = sprite_offset as usize + sprite::SPRITE_SUBHEADER_SIZE;
        let mut buf = vec![0u8; total];

        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[14] = 1;
        buf[15] = 2; // version 2.1.0.0
        buf[36..40].copy_from_slice(&sprite_offset.to_le_bytes());
        buf[40..44].copy_from_slice(&3u32.to_le_bytes()); // claim 3 sprites
        buf[44..48].copy_from_slice(&0u32.to_le_bytes()); // palette_offset
        buf[48..52].copy_from_slice(&0u32.to_le_bytes()); // num_palettes = 0
        buf[52..56].copy_from_slice(&(total as u32).to_le_bytes()); // ldata_offset (empty)
        buf[56..60].copy_from_slice(&0u32.to_le_bytes()); // ldata_length = 0
        buf[60..64].copy_from_slice(&(total as u32).to_le_bytes()); // tdata_offset (empty)
        buf[64..68].copy_from_slice(&0u32.to_le_bytes()); // tdata_length = 0

        // Sprite 0 sub-header: a self-linked, zero-data sprite (decodes to empty
        // without needing any data block — the file has none).
        let s = sprite_offset as usize;
        buf[s + 4..s + 6].copy_from_slice(&8u16.to_le_bytes()); // width = 8
        buf[s + 6..s + 8].copy_from_slice(&8u16.to_le_bytes()); // height = 8
        buf[s + 12..s + 14].copy_from_slice(&0u16.to_le_bytes()); // linked = self
        buf[s + 14] = 0; // format = Raw
        buf[s + 15] = 8; // color_depth
                         // data_offset = 0, data_length = 0 (empty payload)

        let sff = SffFile::from_bytes(&buf).expect("truncated table must not abort the load");
        assert_eq!(sff.sprites.len(), 3);

        // Sprite 0 is the genuine one and decodes (to an empty Raw payload).
        assert_eq!(sff.sprites[0].width, 8);
        assert_eq!(
            sff.decode_sprite(0).expect("real sprite must decode").len(),
            0
        );

        // Sprites 1 and 2 are safe placeholders (zero-sized, self-linked, no data).
        assert_eq!(sff.sprites[1].width, 0);
        assert_eq!(sff.sprites[1].linked_index, 1);
        assert_eq!(sff.sprites[2].width, 0);
        assert_eq!(sff.sprites[2].linked_index, 2);
        // A placeholder decodes to an empty buffer — never panics.
        assert_eq!(sff.decode_sprite(1).unwrap().len(), 0);
        assert_eq!(sff.decode_sprite(2).unwrap().len(), 0);
    }

    /// A short external palette must not panic: out-of-range indices fall back
    /// to transparent black rather than indexing past the buffer.
    #[test]
    fn external_palette_too_short_is_safe() {
        let data = make_test_sff();
        let sff = SffFile::from_bytes(&data).unwrap();

        // Only index 0's worth of colour; index 1 (the sprite's pixel) is missing.
        let short = vec![0u8; 4];
        let rgba = sff.decode_sprite_rgba_with_palette(0, &short).unwrap();
        assert_eq!(rgba.len(), 2 * 2 * 4);
        for px in rgba.chunks_exact(4) {
            assert_eq!(px, &[0, 0, 0, 0], "missing color falls back to transparent");
        }
    }
}
