//! SFF v1 sprite container parser.
//!
//! SFF v1 (used by MUGEN 2002/WinMUGEN-era content) predates the v2 LData/TData
//! layout. After the 12-byte `ElecbyteSpr\0` signature its header records the
//! sprite/group counts and the offset of the first sprite sub-header. Each
//! 32-byte sub-header is followed inline by a **PCX** image (8-bit, RLE).
//!
//! Sub-headers form a singly linked list via an explicit "next sub-header file
//! offset" field rather than being packed contiguously. A sub-header with a zero
//! data length is a *link* that reuses an earlier sprite's pixels.
//!
//! This module parses the header and every sprite sub-header (recording the PCX
//! image dimensions) and exposes a minimal 8-bit PCX RLE decoder so callers can
//! obtain palette-indexed pixels. Decoding never panics: malformed PCX data
//! yields a best-effort, length-clamped buffer.

use fp_core::{FpError, FpResult};
use nom::number::complete::{le_i16, le_u16, le_u32};

use super::palette::SffPalette;
use super::sprite::{SffSprite, SpriteFormat};

/// Size of an SFF v1 sprite sub-header in bytes (excludes the inline PCX image).
const V1_SUBHEADER_SIZE: usize = 32;

/// Bytes occupied by a trailing VGA palette in an 8-bit PCX image: a `0x0C`
/// marker byte followed by 256 RGB triplets (768 bytes).
const PCX_PALETTE_BLOCK_SIZE: usize = 1 + 768;

/// The `0x0C` marker byte that precedes the trailing 256-colour VGA palette in
/// an 8-bit PCX image.
const PCX_PALETTE_MARKER: u8 = 0x0C;

/// Total size of the SFF v1 fixed header region read by [`parse_v1_header`].
const V1_HEADER_SIZE: usize = 33;

/// PCX manufacturer byte (`0x0A`) found at the start of every PCX image.
const PCX_MANUFACTURER: u8 = 0x0A;

/// Parsed SFF v1 top-level header.
#[derive(Debug, Clone)]
pub struct SffV1Header {
    /// Number of sprite groups declared in the file.
    pub num_groups: u32,
    /// Total number of sprite images declared in the file.
    pub num_images: u32,
    /// File offset of the first sprite sub-header.
    pub first_subheader_offset: u32,
    /// Declared size of each sub-header (normally 32).
    pub subheader_size: u32,
}

/// Parses the SFF v1 fixed header.
///
/// Assumes the 12-byte signature has already been validated by the caller.
pub fn parse_v1_header(data: &[u8]) -> FpResult<SffV1Header> {
    if data.len() < V1_HEADER_SIZE {
        return Err(FpError::parse(
            "SFF",
            format!(
                "file too small for SFF v1 header: {} bytes (need {V1_HEADER_SIZE})",
                data.len()
            ),
        ));
    }

    // The signature (12 bytes) and 4 version bytes precede the counts.
    let after_version = &data[16..];

    let (rest, num_groups) =
        le_u32::<_, nom::error::Error<&[u8]>>(after_version).map_err(|_| {
            FpError::parse("SFF", "failed to read SFF v1 num_groups")
        })?;
    let (rest, num_images) = le_u32::<_, nom::error::Error<&[u8]>>(rest)
        .map_err(|_| FpError::parse("SFF", "failed to read SFF v1 num_images"))?;
    let (rest, first_subheader_offset) = le_u32::<_, nom::error::Error<&[u8]>>(rest)
        .map_err(|_| FpError::parse("SFF", "failed to read SFF v1 subheader offset"))?;
    let (_rest, subheader_size) = le_u32::<_, nom::error::Error<&[u8]>>(rest)
        .map_err(|_| FpError::parse("SFF", "failed to read SFF v1 subheader size"))?;

    Ok(SffV1Header {
        num_groups,
        num_images,
        first_subheader_offset,
        subheader_size,
    })
}

/// Parses all SFF v1 sprite sub-headers by walking the linked list, and extracts
/// the per-sprite trailing PCX palettes.
///
/// Returns the parsed [`SffSprite`] list (with `data_offset`/`data_length`
/// pointing at the inline PCX image within `data`) alongside the [`SffPalette`]
/// table built from each sprite's trailing 256-colour VGA palette. Each sprite's
/// `palette_index` is wired to the palette it should use.
///
/// SFF v1 stores a 256-colour VGA palette as the trailing 769 bytes of each 8-bit
/// PCX image (a `0x0C` marker byte followed by 256 RGB triplets). **Palette
/// ownership is driven by pixel-data ownership, not the sub-header's byte-18
/// "shared" flag.** Real WinMUGEN-era content sets that byte on nearly every
/// sprite even when each carries its own distinct palette, so honouring it would
/// collapse the majority of sprites onto one wrong palette. Instead: every sprite
/// that owns pixel data (`data_length > 0`) contributes the palette extracted from
/// its own trailing VGA block; only a data-less (linked) sprite — or a data-owning
/// sprite whose PCX is too short/corrupt to yield a real trailing palette — reuses
/// the most recent real palette (or, if none exists yet, a safe zeroed default).
///
/// The walk never loops forever even if the file declares a cyclic/garbage
/// next-offset, and never panics on malformed PCX/palette data (warn + safe
/// default).
pub fn parse_v1_sprites(
    data: &[u8],
    header: &SffV1Header,
) -> FpResult<(Vec<SffSprite>, Vec<SffPalette>)> {
    let mut sprites = Vec::new();
    let mut palettes: Vec<SffPalette> = Vec::new();
    // Index (into `palettes`) of the most recent real palette, for shared-palette
    // sprites to fall back to.
    let mut last_real_palette: Option<usize> = None;
    let declared = header.num_images as usize;
    // Cap iterations defensively: never trust the declared count or offsets.
    let max_iter = declared.max(1) * 2 + 16;

    let mut offset = header.first_subheader_offset as usize;
    let mut seen = 0usize;

    while offset != 0 && seen < max_iter {
        seen += 1;

        if offset + V1_SUBHEADER_SIZE > data.len() {
            tracing::warn!(offset, "SFF v1: sub-header offset out of range; stopping");
            break;
        }

        let sub = &data[offset..offset + V1_SUBHEADER_SIZE];
        let next_offset = le_u32::<_, nom::error::Error<&[u8]>>(sub)
            .map(|(_, v)| v)
            .unwrap_or(0);
        let data_length = le_u32::<_, nom::error::Error<&[u8]>>(&sub[4..])
            .map(|(_, v)| v)
            .unwrap_or(0);
        let axis_x = le_i16::<_, nom::error::Error<&[u8]>>(&sub[8..])
            .map(|(_, v)| v)
            .unwrap_or(0);
        let axis_y = le_i16::<_, nom::error::Error<&[u8]>>(&sub[10..])
            .map(|(_, v)| v)
            .unwrap_or(0);
        let group = le_u16::<_, nom::error::Error<&[u8]>>(&sub[12..])
            .map(|(_, v)| v)
            .unwrap_or(0);
        let image = le_u16::<_, nom::error::Error<&[u8]>>(&sub[14..])
            .map(|(_, v)| v)
            .unwrap_or(0);
        let linked_index = le_u16::<_, nom::error::Error<&[u8]>>(&sub[16..])
            .map(|(_, v)| v)
            .unwrap_or(0);
        // sub[19..32] reserved (ignored). Note: `sub[18]` is, in some readers,
        // a palette-shared flag; we deliberately do **not** consult it here. Real
        // WinMUGEN-era content (e.g. KFM's intro.sff/ending.sff) sets `sub[18] == 1`
        // on almost every sprite even though each one carries its *own* distinct
        // trailing VGA palette, so honouring the byte would force the majority of
        // sprites onto a single wrong palette. Palette ownership is therefore
        // driven solely by whether a sprite owns pixel data with an extractable
        // trailing palette (see below).

        // The inline PCX image immediately follows the sub-header.
        let pcx_offset = offset + V1_SUBHEADER_SIZE;
        let (width, height) = if data_length == 0 {
            // Linked sprite: no own pixel data, dimensions come from the link.
            (0, 0)
        } else {
            pcx_dimensions(data.get(pcx_offset..).unwrap_or(&[]))
        };

        // Resolve which palette this sprite uses. A sprite that owns its pixel data
        // (`data_length > 0`) carries its own trailing 256-colour VGA palette and
        // must use it — this is the common case for SFF v1 art. Only a data-less
        // (linked) sprite, or a data-owning sprite whose PCX is too short / corrupt
        // to yield a real trailing palette, reuses the most recent real palette
        // (falling back to a safe zeroed default if none exists yet).
        let palette_index = if data_length == 0 {
            // Linked sprite: no own pixels, so reuse the previous real palette.
            reuse_or_default(&mut palettes, &mut last_real_palette)
        } else {
            // `saturating_add` keeps the never-panic intent uniform with the rest
            // of the codec (on 64-bit targets this cannot overflow, but a
            // hypothetical 32-bit build stays safe); `extract_pcx_palette` then
            // clamps the end to the buffer.
            let pcx_end = pcx_offset.saturating_add(data_length as usize);
            match extract_pcx_palette(data, pcx_offset, pcx_end) {
                Some(mut pal) => {
                    // Own, valid trailing palette: contribute a fresh entry.
                    let idx = palettes.len();
                    pal.item = idx.min(u16::MAX as usize) as u16;
                    pal.linked_index = idx.min(u16::MAX as usize) as u16;
                    palettes.push(pal);
                    last_real_palette = Some(idx);
                    idx
                }
                // PCX too short / palette unextractable: reuse the previous palette
                // rather than synthesizing a wrong one.
                None => reuse_or_default(&mut palettes, &mut last_real_palette),
            }
        };

        sprites.push(SffSprite {
            group,
            image,
            width,
            height,
            axis_x,
            axis_y,
            linked_index,
            // SFF v1 sprites are inline 8-bit PCX images, not any of the SFF v2
            // codecs. `SpriteFormat` has no PCX variant, so we tag them `Raw`; the
            // tag is inert for v1 because both `decode_sprite` and
            // `decode_sprite_rgba` intercept `version == V1` before matching on
            // `format` and route to the dedicated PCX decoder.
            format: SpriteFormat::Raw,
            color_depth: 8,
            data_offset: pcx_offset as u32,
            data_length,
            palette_index: palette_index.min(u16::MAX as usize) as u16,
            // Bit 0 = 0 -> data lives in the (single) backing buffer, not TData.
            flags: 0,
        });

        offset = next_offset as usize;
    }

    if sprites.is_empty() {
        return Err(FpError::parse("SFF", "SFF v1 file contains no sprites"));
    }

    Ok((sprites, palettes))
}

/// Returns the palette index to use for a sprite that cannot contribute its own
/// trailing palette (a data-less link, or a data-owning sprite whose PCX is too
/// short / corrupt to yield a real palette).
///
/// Reuses the most recent real palette if one exists; otherwise synthesizes a
/// single safe zeroed default (and records it as the new "last real" so later
/// reuses share it), so a palette lookup never fails.
fn reuse_or_default(
    palettes: &mut Vec<SffPalette>,
    last_real_palette: &mut Option<usize>,
) -> usize {
    if let Some(idx) = *last_real_palette {
        return idx;
    }
    let idx = palettes.len();
    palettes.push(default_v1_palette(idx));
    *last_real_palette = Some(idx);
    idx
}

/// Builds an [`SffPalette`] describing the trailing VGA palette of an 8-bit PCX
/// image located at `[pcx_start..pcx_end)` within the backing buffer.
///
/// The palette is the last 769 bytes of the PCX image: a `0x0C` marker followed
/// by 256 RGB triplets. The returned palette's `data_offset`/`data_length` point
/// at the 768 RGB bytes within the backing buffer so the standard
/// `SffFile::palette` lookup (RGB -> RGBA) resolves it.
///
/// Returns `None` if the image is too short to hold a palette or the trailing
/// bytes fall out of range — the caller then reuses the previous real palette
/// rather than fabricating a wrong one (warn + safe fallback, never panic). The
/// returned palette's `item`/`linked_index` are left at `0`; the caller sets them
/// once it knows the entry's table index.
fn extract_pcx_palette(data: &[u8], pcx_start: usize, pcx_end: usize) -> Option<SffPalette> {
    // Clamp the declared end to the backing buffer to stay in bounds. Some SFF v1
    // writers (e.g. KFM's intro.sff/ending.sff) let the final sprite's declared
    // `data_length` run a few bytes past EOF; the real trailing palette still ends
    // exactly at the buffer end, so clamping recovers it correctly.
    let pcx_end = pcx_end.min(data.len());
    if pcx_end < pcx_start || pcx_end - pcx_start < PCX_PALETTE_BLOCK_SIZE {
        // Not an error: SFF v1 shares one palette across many sprites (the CvS /
        // `.act`-costume style, e.g. evilken), so most sprites carry no trailing
        // palette and correctly reuse the previous real one. Logging this per
        // sprite floods the console (hundreds of lines on a real character), so it
        // is `debug!`, not `warn!` — the reuse is the designed shared-palette path.
        tracing::debug!(
            pcx_start,
            pcx_end,
            "SFF v1: PCX image carries no trailing palette; reusing previous (shared palette)"
        );
        return None;
    }

    let marker_pos = pcx_end - PCX_PALETTE_BLOCK_SIZE;
    if data.get(marker_pos) != Some(&PCX_PALETTE_MARKER) {
        // Some writers omit the marker; tolerate it but note the anomaly.
        tracing::debug!(
            marker_pos,
            "SFF v1: missing 0x0C palette marker; reading trailing 768 bytes anyway"
        );
    }

    // The 768 RGB bytes follow the marker.
    let rgb_start = marker_pos + 1;
    let rgb_end = pcx_end; // rgb_start + 768 == pcx_end by construction
    if rgb_end > data.len() || rgb_end - rgb_start < 768 {
        // Same recoverable shared-palette fallback as above (a truncated trailing
        // block): reuse the previous real palette. `debug!`, not `warn!`, to keep
        // a real character's load quiet.
        tracing::debug!(
            rgb_start,
            rgb_end,
            "SFF v1: trailing palette out of range; reusing previous (shared palette)"
        );
        return None;
    }

    Some(SffPalette {
        group: 0,
        // `item`/`linked_index` are assigned by the caller from the table index.
        item: 0,
        num_colors: 256,
        linked_index: 0,
        data_offset: rgb_start as u32,
        data_length: 768,
    })
}

/// A safe zeroed 256-colour palette used when a real one cannot be extracted.
///
/// Its `data_length` is zero, so [`SffFile::palette`] reads no bytes for it and
/// the standard RGB->RGBA conversion yields an all-black, fully-transparent
/// palette rather than failing.
fn default_v1_palette(index: usize) -> SffPalette {
    SffPalette {
        group: 0,
        item: index.min(u16::MAX as usize) as u16,
        num_colors: 256,
        linked_index: index.min(u16::MAX as usize) as u16,
        data_offset: 0,
        data_length: 0,
    }
}

/// Reads the width/height from a PCX image header.
///
/// Returns `(0, 0)` if the data is too short or is not a recognizable PCX image
/// (never panics).
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

/// Decodes an 8-bit RLE-compressed PCX image into raw palette indices.
///
/// Only the common 8-bit, single-plane PCX variant used by SFF v1 is supported.
/// The decoder is bounds-checked and clamps the output to `width * height`
/// pixels; malformed input yields a best-effort buffer rather than an error.
pub fn decode_pcx_8bit(pcx: &[u8]) -> FpResult<Vec<u8>> {
    if pcx.len() < 128 || pcx[0] != PCX_MANUFACTURER {
        return Err(FpError::parse("SFF", "not a valid 8-bit PCX image"));
    }

    let PcxGeometry {
        width,
        height,
        bytes_per_line,
        planes,
    } = parse_pcx_geometry(pcx)?;

    let total = (bytes_per_line as usize)
        .saturating_mul(planes as usize)
        .saturating_mul(height as usize);
    let mut out = Vec::with_capacity(total.min(1 << 22));

    // PCX pixel data starts after the fixed 128-byte header.
    let mut i = 128usize;
    while i < pcx.len() && out.len() < total {
        let byte = pcx[i];
        i += 1;
        if byte & 0xC0 == 0xC0 {
            // Run-length packet: low 6 bits = count, next byte = value.
            let count = (byte & 0x3F) as usize;
            let value = if i < pcx.len() {
                let v = pcx[i];
                i += 1;
                v
            } else {
                0
            };
            for _ in 0..count {
                if out.len() >= total {
                    break;
                }
                out.push(value);
            }
        } else {
            out.push(byte);
        }
    }

    // Trim each scanline's padding bytes down to `width` and concatenate.
    let row_stride = (bytes_per_line as usize).saturating_mul(planes as usize);
    if row_stride == 0 || width == 0 || height == 0 {
        return Ok(out);
    }

    let mut pixels = Vec::with_capacity((width as usize).saturating_mul(height as usize));
    for row in 0..height as usize {
        let start = row.saturating_mul(row_stride);
        let end = (start + width as usize).min(out.len());
        if start >= out.len() {
            break;
        }
        pixels.extend_from_slice(&out[start..end]);
    }
    Ok(pixels)
}

/// Geometry extracted from a PCX header.
struct PcxGeometry {
    width: u16,
    height: u16,
    bytes_per_line: u16,
    planes: u8,
}

/// Parses the geometry fields from a 128-byte PCX header.
fn parse_pcx_geometry(pcx: &[u8]) -> FpResult<PcxGeometry> {
    if pcx.len() < 128 {
        return Err(FpError::parse("SFF", "PCX header truncated"));
    }
    let xmin = u16::from_le_bytes([pcx[4], pcx[5]]);
    let ymin = u16::from_le_bytes([pcx[6], pcx[7]]);
    let xmax = u16::from_le_bytes([pcx[8], pcx[9]]);
    let ymax = u16::from_le_bytes([pcx[10], pcx[11]]);
    let planes = pcx[65];
    let bytes_per_line = u16::from_le_bytes([pcx[66], pcx[67]]);
    let width = xmax.saturating_sub(xmin).saturating_add(1);
    let height = ymax.saturating_sub(ymin).saturating_add(1);
    Ok(PcxGeometry {
        width,
        height,
        bytes_per_line,
        planes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a tiny synthetic SFF v1 file: header + one sub-header + one PCX.
    fn make_v1_sff() -> Vec<u8> {
        // 512-byte header region plus a 32-byte sub-header at offset 512.
        let mut buf = vec![0u8; 512 + 32];
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        // Version 1.0.0.1 (major byte at offset 15 = 1).
        buf[12] = 0;
        buf[13] = 1;
        buf[14] = 0;
        buf[15] = 1;
        buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // num_groups
        buf[20..24].copy_from_slice(&1u32.to_le_bytes()); // num_images
        buf[24..28].copy_from_slice(&512u32.to_le_bytes()); // first subheader
        buf[28..32].copy_from_slice(&32u32.to_le_bytes()); // subheader size

        // Sub-header at offset 512.
        let s = 512usize;
        let mut sub = vec![0u8; 32];
        sub[0..4].copy_from_slice(&0u32.to_le_bytes()); // next offset = 0 (last)
        // PCX data length filled in after we build the PCX.
        sub[8..10].copy_from_slice(&3i16.to_le_bytes()); // axis_x
        sub[10..12].copy_from_slice(&4i16.to_le_bytes()); // axis_y
        sub[12..14].copy_from_slice(&7u16.to_le_bytes()); // group
        sub[14..16].copy_from_slice(&9u16.to_le_bytes()); // image

        // Minimal PCX: 128-byte header describing a 2x2 image, RLE body.
        let mut pcx = vec![0u8; 128];
        pcx[0] = 0x0A; // manufacturer
        pcx[1] = 5; // version
        pcx[2] = 1; // encoding = RLE
        pcx[3] = 8; // bits per pixel
                    // xmin=0, ymin=0, xmax=1, ymax=1 -> 2x2
        pcx[8..10].copy_from_slice(&1u16.to_le_bytes());
        pcx[10..12].copy_from_slice(&1u16.to_le_bytes());
        pcx[65] = 1; // planes
        pcx[66..68].copy_from_slice(&2u16.to_le_bytes()); // bytes per line
                                                          // Body: two scanlines, each a run of 2 of value 0x05.
        pcx.push(0xC2); // run count 2
        pcx.push(0x05); // value
        pcx.push(0xC2);
        pcx.push(0x05);

        let data_len = pcx.len() as u32;
        sub[4..8].copy_from_slice(&data_len.to_le_bytes());

        buf[s..s + 32].copy_from_slice(&sub);
        buf.extend_from_slice(&pcx);
        buf
    }

    #[test]
    fn parse_v1_header_fields() {
        let data = make_v1_sff();
        let header = parse_v1_header(&data).unwrap();
        assert_eq!(header.num_groups, 1);
        assert_eq!(header.num_images, 1);
        assert_eq!(header.first_subheader_offset, 512);
        assert_eq!(header.subheader_size, 32);
    }

    #[test]
    fn parse_v1_one_sprite() {
        let data = make_v1_sff();
        let header = parse_v1_header(&data).unwrap();
        let (sprites, _palettes) = parse_v1_sprites(&data, &header).unwrap();
        assert_eq!(sprites.len(), 1);
        assert_eq!(sprites[0].group, 7);
        assert_eq!(sprites[0].image, 9);
        assert_eq!(sprites[0].axis_x, 3);
        assert_eq!(sprites[0].axis_y, 4);
        assert_eq!(sprites[0].width, 2);
        assert_eq!(sprites[0].height, 2);
    }

    #[test]
    fn decode_synthetic_pcx() {
        let data = make_v1_sff();
        let header = parse_v1_header(&data).unwrap();
        let (sprites, _palettes) = parse_v1_sprites(&data, &header).unwrap();
        let start = sprites[0].data_offset as usize;
        let end = start + sprites[0].data_length as usize;
        let pixels = decode_pcx_8bit(&data[start..end]).unwrap();
        assert_eq!(pixels.len(), 4); // 2x2
        assert!(pixels.iter().all(|&p| p == 0x05));
    }

    /// Builds a synthetic SFF v1 file with two sprites that each carry their own
    /// distinct trailing PCX palette. The second sprite *also* sets the byte-18
    /// "shared" flag, mirroring real WinMUGEN-era content — the loader must ignore
    /// that flag and give the second sprite its OWN palette, since it owns its
    /// pixel data with an extractable trailing palette.
    ///
    /// Returns the assembled bytes. Sprite 0's palette colour 1 is red (`R=200`)
    /// and sprite 1's is `R=50`, so the two extracted palettes are distinguishable.
    fn make_v1_sff_with_palettes() -> Vec<u8> {
        // Build one 8-bit PCX (2x2) followed by a 0x0C marker + 768 RGB bytes.
        fn pcx_with_palette(pal_color1_r: u8) -> Vec<u8> {
            let mut pcx = vec![0u8; 128];
            pcx[0] = 0x0A; // manufacturer
            pcx[1] = 5;
            pcx[2] = 1; // RLE
            pcx[3] = 8; // bpp
            pcx[8..10].copy_from_slice(&1u16.to_le_bytes()); // xmax -> width 2
            pcx[10..12].copy_from_slice(&1u16.to_le_bytes()); // ymax -> height 2
            pcx[65] = 1; // planes
            pcx[66..68].copy_from_slice(&2u16.to_le_bytes()); // bytes per line
                                                              // body: two runs of 2 of value 1 -> pixels all index 1
            pcx.push(0xC2);
            pcx.push(0x01);
            pcx.push(0xC2);
            pcx.push(0x01);
            // Trailing VGA palette: 0x0C marker + 256 RGB triplets.
            pcx.push(0x0C);
            let mut pal = vec![0u8; 768];
            pal[3] = pal_color1_r; // colour index 1, R channel
            pcx.extend_from_slice(&pal);
            pcx
        }

        let pcx0 = pcx_with_palette(200);
        let pcx1 = pcx_with_palette(50); // its own DISTINCT palette, but flagged "shared"

        // Header region (512 bytes) + two sub-headers each followed by its PCX.
        let sub0_off = 512usize;
        let sub1_off = sub0_off + V1_SUBHEADER_SIZE + pcx0.len();

        let total = sub1_off + V1_SUBHEADER_SIZE + pcx1.len();
        let mut buf = vec![0u8; total];
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[15] = 1; // v1
        buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // num_groups
        buf[20..24].copy_from_slice(&2u32.to_le_bytes()); // num_images
        buf[24..28].copy_from_slice(&(sub0_off as u32).to_le_bytes());
        buf[28..32].copy_from_slice(&32u32.to_le_bytes());

        // Sub-header 0: own palette (shared flag clear).
        buf[sub0_off..sub0_off + 4].copy_from_slice(&(sub1_off as u32).to_le_bytes()); // next
        buf[sub0_off + 4..sub0_off + 8].copy_from_slice(&(pcx0.len() as u32).to_le_bytes());
        buf[sub0_off + 12..sub0_off + 14].copy_from_slice(&0u16.to_le_bytes()); // group
        buf[sub0_off + 14..sub0_off + 16].copy_from_slice(&0u16.to_le_bytes()); // image
        buf[sub0_off + 18] = 0; // shared-palette flag clear
        buf[sub0_off + V1_SUBHEADER_SIZE..sub0_off + V1_SUBHEADER_SIZE + pcx0.len()]
            .copy_from_slice(&pcx0);

        // Sub-header 1: byte-18 "shared" flag SET, but the sprite owns its own
        // pixel data + trailing palette, so the loader must use its OWN palette.
        buf[sub1_off..sub1_off + 4].copy_from_slice(&0u32.to_le_bytes()); // next = 0 (last)
        buf[sub1_off + 4..sub1_off + 8].copy_from_slice(&(pcx1.len() as u32).to_le_bytes());
        buf[sub1_off + 12..sub1_off + 14].copy_from_slice(&1u16.to_le_bytes()); // group
        buf[sub1_off + 14..sub1_off + 16].copy_from_slice(&0u16.to_le_bytes()); // image
        buf[sub1_off + 18] = 1; // byte-18 "shared" flag SET (must be ignored)
        buf[sub1_off + V1_SUBHEADER_SIZE..sub1_off + V1_SUBHEADER_SIZE + pcx1.len()]
            .copy_from_slice(&pcx1);

        buf
    }

    #[test]
    fn extracts_trailing_pcx_palette() {
        let data = make_v1_sff_with_palettes();
        let header = parse_v1_header(&data).unwrap();
        let (sprites, palettes) = parse_v1_sprites(&data, &header).unwrap();

        assert_eq!(sprites.len(), 2);
        // Each data-owning sprite contributes its OWN palette — the byte-18
        // "shared" flag on sprite 1 must NOT collapse it onto sprite 0's palette.
        assert_eq!(
            palettes.len(),
            2,
            "each sprite with its own pixel+palette data must extract its own palette"
        );
        assert_eq!(sprites[0].palette_index, 0);
        assert_eq!(
            sprites[1].palette_index, 1,
            "sprite 1 owns its data, so it must use its own palette (not sprite 0's)"
        );

        // The two extracted palettes must be distinct and carry the real PCX
        // colours we wrote (R=200 for sprite 0, R=50 for sprite 1).
        let pal0 = &palettes[0];
        let pal1 = &palettes[1];
        assert_eq!(pal0.data_length, 768);
        assert_eq!(pal1.data_length, 768);
        assert_ne!(
            pal0.data_offset, pal1.data_offset,
            "the two palettes must point at different trailing blocks"
        );
        let r0 = data[pal0.data_offset as usize + 3];
        let r1 = data[pal1.data_offset as usize + 3];
        assert_eq!(r0, 200, "sprite 0's palette colour 1 R channel");
        assert_eq!(r1, 50, "sprite 1's palette colour 1 R channel");
        // Self-referential links point each palette at its own table index.
        assert_eq!(pal0.linked_index, 0);
        assert_eq!(pal1.linked_index, 1);
    }

    #[test]
    fn data_owning_short_pcx_reuses_previous_palette() {
        // A data-owning sprite whose PCX is too short to hold a trailing palette
        // must reuse the previous real palette rather than fabricating a wrong one.
        let mut palettes = vec![SffPalette {
            group: 0,
            item: 0,
            num_colors: 256,
            linked_index: 0,
            data_offset: 4242,
            data_length: 768,
        }];
        let mut last_real: Option<usize> = Some(0);
        let idx = if let Some(p) = extract_pcx_palette(&[0u8; 200], 0, 200) {
            palettes.push(p);
            1
        } else {
            reuse_or_default(&mut palettes, &mut last_real)
        };
        assert_eq!(idx, 0, "short PCX must reuse the previous real palette");
        assert_eq!(palettes.len(), 1, "no fabricated palette was appended");
    }

    #[test]
    fn missing_palette_returns_none() {
        // A PCX too short to hold a trailing palette yields `None` so the caller
        // reuses the previous palette rather than fabricating one.
        assert!(
            extract_pcx_palette(&[0u8; 200], 0, 200).is_none(),
            "short PCX must not yield a palette"
        );
    }

    #[test]
    fn first_sprite_without_palette_falls_back_to_default() {
        // If the very first sprite cannot yield a palette, `reuse_or_default`
        // synthesizes a single safe zeroed default so lookups never fail.
        let mut palettes: Vec<SffPalette> = Vec::new();
        let mut last_real: Option<usize> = None;
        let idx = reuse_or_default(&mut palettes, &mut last_real);
        assert_eq!(idx, 0);
        assert_eq!(palettes.len(), 1);
        assert_eq!(palettes[0].data_length, 0, "default palette carries no data");
        assert_eq!(last_real, Some(0));
    }

    #[test]
    fn cyclic_offsets_do_not_loop_forever() {
        // A sub-header whose `next` points back to itself must terminate.
        let mut buf = vec![0u8; 512 + 32 + 128 + 4];
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[15] = 1;
        buf[16..20].copy_from_slice(&1u32.to_le_bytes());
        buf[20..24].copy_from_slice(&1u32.to_le_bytes());
        buf[24..28].copy_from_slice(&512u32.to_le_bytes());
        buf[28..32].copy_from_slice(&32u32.to_le_bytes());
        // next offset -> 512 (self), data_length 4
        buf[512..516].copy_from_slice(&512u32.to_le_bytes());
        buf[516..520].copy_from_slice(&4u32.to_le_bytes());
        let header = parse_v1_header(&buf).unwrap();
        let (sprites, _palettes) = parse_v1_sprites(&buf, &header).unwrap();
        // Bounded iteration: a handful of entries, not an infinite loop.
        assert!(!sprites.is_empty());
        assert!(sprites.len() < 64);
    }
}
