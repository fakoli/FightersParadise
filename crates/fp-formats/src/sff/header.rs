//! SFF v2 file header parser.
//!
//! The SFF v2 header occupies the first 512 bytes of the file and contains
//! version information, sprite/palette counts, and offsets to the various
//! data blocks within the file.

use fp_core::{FpError, FpResult};
use nom::bytes::complete::{tag, take};
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
    /// Number of sprite groups in the file.
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

/// Parses an SFF v2 file header from raw bytes.
///
/// Expects at least 512 bytes of input. Validates the signature and
/// version major byte.
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
    let (rest, version_major) = le_u8(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read version major")
    })?;

    if version_major != 2 {
        return Err(FpError::parse(
            "SFF",
            format!("unsupported SFF version {version_major} (expected 2)"),
        ));
    }

    // Reserved: 3 x u32 (12 bytes)
    let (rest, _) = take(12u8)(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read reserved fields")
    })?;

    // num_groups, num_sprites
    let (rest, num_groups) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read num_groups")
    })?;
    let (rest, num_sprites) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read num_sprites")
    })?;

    // Sprite sub-header offset and length
    let (rest, sprite_offset) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite_offset")
    })?;
    let (rest, sprite_length) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read sprite_length")
    })?;

    // Palette sub-header offset and length
    let (rest, palette_offset) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read palette_offset")
    })?;
    let (rest, palette_length) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read palette_length")
    })?;

    // LData offset and length
    let (rest, ldata_offset) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read ldata_offset")
    })?;
    let (rest, ldata_length) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read ldata_length")
    })?;

    // TData offset and length
    let (rest, tdata_offset) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read tdata_offset")
    })?;
    let (_rest, tdata_length) = le_u32(rest).map_err(|_: nom::Err<nom::error::Error<&[u8]>>| {
        FpError::parse("SFF", "failed to read tdata_length")
    })?;

    Ok(SffHeader {
        version_major,
        version_minor1,
        version_minor2,
        version_minor3,
        num_groups,
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
    fn make_test_header(num_sprites: u32, num_groups: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 512];

        // Signature
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");

        // Version: minor3=0, minor2=0, minor1=1, major=2
        buf[12] = 0; // minor3
        buf[13] = 0; // minor2
        buf[14] = 1; // minor1
        buf[15] = 2; // major

        // Reserved (16..28): zeros

        // num_groups at offset 28
        buf[28..32].copy_from_slice(&num_groups.to_le_bytes());
        // num_sprites at offset 32
        buf[32..36].copy_from_slice(&num_sprites.to_le_bytes());
        // sprite_offset at offset 36
        buf[36..40].copy_from_slice(&512u32.to_le_bytes());
        // sprite_length at offset 40
        buf[40..44].copy_from_slice(&(num_sprites * 28).to_le_bytes());
        // palette_offset at offset 44
        let pal_off = 512 + num_sprites * 28;
        buf[44..48].copy_from_slice(&pal_off.to_le_bytes());
        // palette_length at offset 48
        buf[48..52].copy_from_slice(&16u32.to_le_bytes());
        // ldata_offset at offset 52
        buf[52..56].copy_from_slice(&1024u32.to_le_bytes());
        // ldata_length at offset 56
        buf[56..60].copy_from_slice(&768u32.to_le_bytes());
        // tdata_offset at offset 60
        buf[60..64].copy_from_slice(&2048u32.to_le_bytes());
        // tdata_length at offset 64
        buf[64..68].copy_from_slice(&256u32.to_le_bytes());

        buf
    }

    #[test]
    fn parse_valid_header() {
        let data = make_test_header(10, 3);
        let header = parse_header(&data).unwrap();

        assert_eq!(header.version_major, 2);
        assert_eq!(header.version_minor1, 1);
        assert_eq!(header.num_sprites, 10);
        assert_eq!(header.num_groups, 3);
        assert_eq!(header.sprite_offset, 512);
        assert_eq!(header.sprite_length, 280); // 10 * 28
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
