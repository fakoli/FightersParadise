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

use super::sprite::{SffSprite, SpriteFormat};

/// Size of an SFF v1 sprite sub-header in bytes (excludes the inline PCX image).
const V1_SUBHEADER_SIZE: usize = 32;

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

/// Parses all SFF v1 sprite sub-headers by walking the linked list.
///
/// Returns the parsed [`SffSprite`] list (with `data_offset`/`data_length`
/// pointing at the inline PCX image within `data`) and never loops forever even
/// if the file declares a cyclic/garbage next-offset.
pub fn parse_v1_sprites(data: &[u8], header: &SffV1Header) -> FpResult<Vec<SffSprite>> {
    let mut sprites = Vec::new();
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
        // sub[18] = palette-shared flag, sub[19..32] reserved (ignored).

        // The inline PCX image immediately follows the sub-header.
        let pcx_offset = offset + V1_SUBHEADER_SIZE;
        let (width, height) = if data_length == 0 {
            // Linked sprite: no own pixel data, dimensions come from the link.
            (0, 0)
        } else {
            pcx_dimensions(data.get(pcx_offset..).unwrap_or(&[]))
        };

        sprites.push(SffSprite {
            group,
            image,
            width,
            height,
            axis_x,
            axis_y,
            linked_index,
            // SFF v1 always stores 8-bit indexed PCX images.
            format: SpriteFormat::Png8,
            color_depth: 8,
            data_offset: pcx_offset as u32,
            data_length,
            palette_index: 0,
            // Bit 0 = 0 -> data lives in the (single) backing buffer, not TData.
            flags: 0,
        });

        offset = next_offset as usize;
    }

    if sprites.is_empty() {
        return Err(FpError::parse("SFF", "SFF v1 file contains no sprites"));
    }

    Ok(sprites)
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
        let sprites = parse_v1_sprites(&data, &header).unwrap();
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
        let sprites = parse_v1_sprites(&data, &header).unwrap();
        let start = sprites[0].data_offset as usize;
        let end = start + sprites[0].data_length as usize;
        let pixels = decode_pcx_8bit(&data[start..end]).unwrap();
        assert_eq!(pixels.len(), 4); // 2x2
        assert!(pixels.iter().all(|&p| p == 0x05));
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
        let sprites = parse_v1_sprites(&buf, &header).unwrap();
        // Bounded iteration: a handful of entries, not an infinite loop.
        assert!(!sprites.is_empty());
        assert!(sprites.len() < 64);
    }
}
