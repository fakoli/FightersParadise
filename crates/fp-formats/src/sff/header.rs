//! SFF v2 file header parser.
//!
//! The SFF v2 header occupies the first 512 bytes of the file and contains
//! version information, sprite/palette counts, and offsets to the various
//! data blocks within the file.

use fp_core::{FpError, FpResult};
use nom::bytes::complete::tag;
use nom::number::complete::{le_u32, le_u8};

/// The expected file signature at the start of every SFF file.
const SFF_SIGNATURE: &[u8; 12] = b"ElecbyteSpr\0";

/// Total size of the SFF v2 header in bytes.
const HEADER_SIZE: usize = 512;

/// Parsed SFF v2 file header.
#[derive(Debug, Clone)]
pub struct SffHeader {
    /// Major version number (should be 2 for SFF v2).
    pub version_major: u8,
    /// Minor version number (minor1 component).
    pub version_minor1: u8,
    /// Minor version number (minor2 component).
    pub version_minor2: u8,
    /// Minor version number (minor3 / lo component).
    pub version_minor3: u8,
    /// Always `0`. SFF v2 has no dedicated group-count field in its header
    /// (groups are implied by the individual sprite entries), so this is never
    /// populated from the file. The field is retained only for API/back-compat
    /// with callers that still reference it.
    pub num_groups: u32,
    /// Total number of sprites in the file.
    pub num_sprites: u32,
    /// Byte offset to the start of sprite sub-headers.
    pub sprite_offset: u32,
    /// Total byte length of the sprite sub-headers block.
    pub sprite_length: u32,
    /// Byte offset to the start of palette sub-headers.
    pub palette_offset: u32,
    /// Total byte length of the palette sub-headers block.
    pub palette_length: u32,
    /// Byte offset to the literal data block (LData).
    pub ldata_offset: u32,
    /// Total byte length of the literal data block.
    pub ldata_length: u32,
    /// Byte offset to the translate data block (TData).
    pub tdata_offset: u32,
    /// Total byte length of the translate data block.
    pub tdata_length: u32,
}

/// Byte size of a single sprite sub-header (used to derive the block length).
const SPRITE_SUBHEADER_SIZE: u32 = 28;

/// Byte size of a single palette sub-header (used to derive the block length).
const PALETTE_SUBHEADER_SIZE: u32 = 16;

/// Parses an SFF v2 file header from raw bytes.
///
/// Expects at least 512 bytes of input. Validates the signature and version
/// major byte.
///
/// ## Real-world layout note
///
/// The header layout follows the *actual* MUGEN 1.0 SFF v2 format produced by
/// the official tools (and Ikemen), which differs from some older third-party
/// write-ups. After the 12-byte signature and 4 version bytes there are five
/// reserved `u32`s (offsets 16..36), then the directory fields begin at offset
/// 36 and store **counts**, not byte-lengths, for the sprite/palette tables:
///
/// | Offset | Field |
/// |--------|-------|
/// | 36 | first sprite sub-header offset |
/// | 40 | number of sprites |
/// | 44 | first palette sub-header offset |
/// | 48 | number of palettes |
/// | 52 | LData offset |
/// | 56 | LData length |
/// | 60 | TData offset |
/// | 64 | TData length |
///
/// The block *lengths* (`sprite_length`, `palette_length`) are derived from the
/// counts so the rest of the crate can keep working in byte terms.
pub fn parse_header(input: &[u8]) -> FpResult<SffHeader> {
    if input.len() < HEADER_SIZE {
        return Err(FpError::parse(
            "SFF",
            format!(
                "file too small for header: {} bytes (need {})",
                input.len(),
                HEADER_SIZE
            ),
        ));
    }

    let (rest, _) = tag(SFF_SIGNATURE.as_slice())(input).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "invalid file signature (expected 'ElecbyteSpr\\0')")
    })?;

    // Version bytes: minor3, minor2, minor1, major
    let (rest, version_minor3) = le_u8(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read version minor3")
    })?;
    let (rest, version_minor2) = le_u8(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read version minor2")
    })?;
    let (rest, version_minor1) = le_u8(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read version minor1")
    })?;
    let (_rest, version_major) = le_u8(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read version major")
    })?;

    if version_major != 2 {
        return Err(FpError::parse(
            "SFF",
            format!("unsupported SFF version {version_major} (expected 2)"),
        ));
    }

    // Read directory fields by absolute offset (see the layout table above).
    // `read_u32` is bounds-safe because we already checked `input.len() >= 512`.
    let read_u32 = |off: usize| -> FpResult<u32> {
        let (_, v) = le_u32::<_, nom::error::Error<&[u8]>>(&input[off..])
            .map_err(|_| FpError::parse("SFF", format!("failed to read u32 at offset {off}")))?;
        Ok(v)
    };

    let sprite_offset = read_u32(36)?;
    let num_sprites = read_u32(40)?;
    let palette_offset = read_u32(44)?;
    let num_palettes = read_u32(48)?;
    let ldata_offset = read_u32(52)?;
    let ldata_length = read_u32(56)?;
    let tdata_offset = read_u32(60)?;
    let tdata_length = read_u32(64)?;

    // Derive block byte-lengths from the counts.
    let sprite_length = num_sprites.saturating_mul(SPRITE_SUBHEADER_SIZE);
    let palette_length = num_palettes.saturating_mul(PALETTE_SUBHEADER_SIZE);

    Ok(SffHeader {
        version_major,
        version_minor1,
        version_minor2,
        version_minor3,
        // The real SFF v2 header has no dedicated group-count field at this
        // position; groups are implied by the sprite entries themselves.
        num_groups: 0,
        num_sprites,
        sprite_offset,
        sprite_length,
        palette_offset,
        palette_length,
        ldata_offset,
        ldata_length,
        tdata_offset,
        tdata_length,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal 512-byte synthetic SFF v2 header.
    fn make_test_header(num_sprites: u32, num_palettes: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 512];

        // Signature
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");

        // Version: minor3=0, minor2=0, minor1=1, major=2
        buf[12] = 0; // minor3
        buf[13] = 0; // minor2
        buf[14] = 1; // minor1
        buf[15] = 2; // major

        // Reserved (16..36): zeros — real SFF v2 has five reserved u32s here.

        // Directory fields, real MUGEN 1.0 layout (counts, not byte lengths):
        // sprite_offset @36
        buf[36..40].copy_from_slice(&512u32.to_le_bytes());
        // num_sprites @40
        buf[40..44].copy_from_slice(&num_sprites.to_le_bytes());
        // palette_offset @44
        buf[44..48].copy_from_slice(&(512 + num_sprites * 28).to_le_bytes());
        // num_palettes @48
        buf[48..52].copy_from_slice(&num_palettes.to_le_bytes());
        // ldata_offset @52
        buf[52..56].copy_from_slice(&1024u32.to_le_bytes());
        // ldata_length @56
        buf[56..60].copy_from_slice(&768u32.to_le_bytes());
        // tdata_offset @60
        buf[60..64].copy_from_slice(&2048u32.to_le_bytes());
        // tdata_length @64
        buf[64..68].copy_from_slice(&256u32.to_le_bytes());

        buf
    }

    #[test]
    fn parse_valid_header() {
        // num_sprites = 10, num_palettes = 3
        let data = make_test_header(10, 3);
        let header = parse_header(&data).unwrap();

        assert_eq!(header.version_major, 2);
        assert_eq!(header.version_minor1, 1);
        assert_eq!(header.num_sprites, 10);
        assert_eq!(header.sprite_offset, 512);
        assert_eq!(header.sprite_length, 280); // 10 * 28, derived from count
        assert_eq!(header.palette_length, 48); // 3 * 16, derived from count
        assert_eq!(header.ldata_offset, 1024);
        assert_eq!(header.ldata_length, 768);
        assert_eq!(header.tdata_offset, 2048);
        assert_eq!(header.tdata_length, 256);
    }

    #[test]
    fn reject_bad_signature() {
        let mut data = make_test_header(1, 1);
        data[0] = b'X';
        let err = parse_header(&data).unwrap_err();
        assert!(err.to_string().contains("signature"));
    }

    #[test]
    fn reject_wrong_version() {
        let mut data = make_test_header(1, 1);
        data[15] = 1; // set major version to 1
        let err = parse_header(&data).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    #[test]
    fn reject_too_small_input() {
        let data = vec![0u8; 100];
        let err = parse_header(&data).unwrap_err();
        assert!(err.to_string().contains("too small"));
    }
}
