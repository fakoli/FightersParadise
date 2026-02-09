//! SFF v2 sprite decompression routines.
//!
//! Implements the RLE8 decompression algorithm used by most SFF v2 sprite data.
//! RLE5, LZ5, and PNG-based formats are stubbed with error returns.

use fp_core::{FpError, FpResult};

/// Decompresses RLE8-encoded sprite data.
///
/// The RLE8 encoding works as follows:
/// - If a byte has bit 6 clear (`byte & 0x40 == 0`): it is a literal pixel value; output it.
/// - If a byte has bit 6 set: the lower 6 bits give the run length, and the next byte
///   is the color to repeat. A run length of 0 means 256.
///
/// The first 4 bytes of the compressed data are a little-endian u32 giving the
/// decompressed size. The actual compressed data follows.
pub fn decompress_rle8(data: &[u8]) -> FpResult<Vec<u8>> {
    if data.len() < 4 {
        return Err(FpError::parse(
            "SFF",
            "RLE8 data too short for size header",
        ));
    }

    let decompressed_size =
        u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;

    let mut output = Vec::with_capacity(decompressed_size);
    let mut i = 4; // skip the 4-byte size prefix

    while i < data.len() && output.len() < decompressed_size {
        let byte = data[i];
        i += 1;

        if byte & 0x40 == 0 {
            // Literal pixel
            output.push(byte);
        } else {
            // RLE run: lower 6 bits = length, next byte = color
            let run_len = (byte & 0x3F) as usize;
            let run_len = if run_len == 0 { 256 } else { run_len };

            if i >= data.len() {
                return Err(FpError::parse(
                    "SFF",
                    "RLE8 unexpected end of data during run",
                ));
            }

            let color = data[i];
            i += 1;

            let actual_len = run_len.min(decompressed_size - output.len());
            output.extend(std::iter::repeat(color).take(actual_len));
        }
    }

    // Pad with zeros if we didn't reach the expected size
    if output.len() < decompressed_size {
        tracing::warn!(
            expected = decompressed_size,
            actual = output.len(),
            "RLE8 decompressed data shorter than expected, padding with zeros"
        );
        output.resize(decompressed_size, 0);
    }

    Ok(output)
}

/// Decompresses RLE5-encoded sprite data.
///
/// RLE5 encodes 5-bit palette indices (0–31). Each byte is split:
/// - Lower 5 bits: color index
/// - Upper 3 bits: flags (0 = run-length mode, 1–7 = literal count)
///
/// In run-length mode (flags == 0), the next byte gives the repeat count.
/// In literal mode (flags == N where N in 1..=7), the color is output once,
/// then N-1 additional bytes are read and their lower 5 bits output.
///
/// The first 4 bytes are a little-endian u32 giving the decompressed size.
pub fn decompress_rle5(data: &[u8]) -> FpResult<Vec<u8>> {
    if data.len() < 4 {
        return Err(FpError::parse(
            "SFF",
            "RLE5 data too short for size header",
        ));
    }

    let decompressed_size =
        u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;

    let mut output = Vec::with_capacity(decompressed_size);
    let mut i = 4;

    while i < data.len() && output.len() < decompressed_size {
        let d = data[i];
        i += 1;

        let color_index = d & 0x1F;
        let flags = (d >> 5) & 0x07;

        if flags == 0 {
            // Run-length mode: next byte is the repeat count
            if i >= data.len() {
                return Err(FpError::parse(
                    "SFF",
                    "RLE5 unexpected end of data during run-length read",
                ));
            }
            let rle_count = data[i] as usize;
            i += 1;

            let actual_len = rle_count.min(decompressed_size - output.len());
            output.extend(std::iter::repeat(color_index).take(actual_len));
        } else {
            // Literal mode: output the color, then flags-1 more literals
            if output.len() < decompressed_size {
                output.push(color_index);
            }
            for _ in 1..flags {
                if i >= data.len() || output.len() >= decompressed_size {
                    break;
                }
                output.push(data[i] & 0x1F);
                i += 1;
            }
        }
    }

    // Pad with zeros if we didn't reach the expected size
    if output.len() < decompressed_size {
        tracing::warn!(
            expected = decompressed_size,
            actual = output.len(),
            "RLE5 decompressed data shorter than expected, padding with zeros"
        );
        output.resize(decompressed_size, 0);
    }

    Ok(output)
}

/// Decompresses LZ5-encoded sprite data.
///
/// LZ5 is an LZ77-style compression using an 8192-byte sliding window.
/// A control byte encodes 8 operations (LSB first):
/// - Bit = 1: literal byte — read one byte and output it.
/// - Bit = 0: back reference — read a 16-bit LE value; lower 13 bits are the
///   offset into the recycling buffer, upper 3 bits + 2 give the copy length.
///
/// The first 4 bytes are a little-endian u32 giving the decompressed size.
pub fn decompress_lz5(data: &[u8]) -> FpResult<Vec<u8>> {
    if data.len() < 4 {
        return Err(FpError::parse(
            "SFF",
            "LZ5 data too short for size header",
        ));
    }

    let decompressed_size =
        u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;

    let mut output = Vec::with_capacity(decompressed_size);
    let mut recycling_buf = vec![0u8; 8192];
    let mut buf_pos: usize = 0;
    let mut i = 4;

    while i < data.len() && output.len() < decompressed_size {
        if i >= data.len() {
            break;
        }
        let control = data[i];
        i += 1;

        for bit in 0..8 {
            if output.len() >= decompressed_size {
                break;
            }

            if (control >> bit) & 1 == 1 {
                // Literal byte
                if i >= data.len() {
                    return Err(FpError::parse(
                        "SFF",
                        "LZ5 unexpected end of data during literal read",
                    ));
                }
                let byte = data[i];
                i += 1;

                output.push(byte);
                recycling_buf[buf_pos] = byte;
                buf_pos = (buf_pos + 1) % 8192;
            } else {
                // Back reference
                if i + 1 >= data.len() {
                    return Err(FpError::parse(
                        "SFF",
                        "LZ5 unexpected end of data during back-reference read",
                    ));
                }
                let value = u16::from_le_bytes([data[i], data[i + 1]]);
                i += 2;

                let ref_offset = (value & 0x1FFF) as usize;
                let copy_length = (((value >> 13) & 0x07) + 2) as usize;

                for j in 0..copy_length {
                    if output.len() >= decompressed_size {
                        break;
                    }
                    let byte = recycling_buf[(ref_offset + j) % 8192];
                    output.push(byte);
                    recycling_buf[buf_pos] = byte;
                    buf_pos = (buf_pos + 1) % 8192;
                }
            }
        }
    }

    // Pad with zeros if we didn't reach the expected size
    if output.len() < decompressed_size {
        tracing::warn!(
            expected = decompressed_size,
            actual = output.len(),
            "LZ5 decompressed data shorter than expected, padding with zeros"
        );
        output.resize(decompressed_size, 0);
    }

    Ok(output)
}

/// Stub for PNG sprite decoding (not yet implemented).
pub fn decompress_png(_data: &[u8]) -> FpResult<Vec<u8>> {
    tracing::warn!("PNG sprite decoding not yet implemented");
    Err(FpError::Unsupported(
        "PNG sprite decoding".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle8_literals_only() {
        // Size prefix (4 bytes LE) = 3, then 3 literal bytes (all < 0x40)
        let data = [
            3, 0, 0, 0, // decompressed size = 3
            0x01, 0x02, 0x03, // three literal pixels
        ];
        let result = decompress_rle8(&data).unwrap();
        assert_eq!(result, vec![1, 2, 3]);
    }

    #[test]
    fn rle8_run() {
        // Size prefix = 5, then a run of 5 x color 0x0A
        let data = [
            5, 0, 0, 0, // decompressed size = 5
            0x45, 0x0A, // bit6 set, lower 6 bits = 5, color = 0x0A
        ];
        let result = decompress_rle8(&data).unwrap();
        assert_eq!(result, vec![0x0A, 0x0A, 0x0A, 0x0A, 0x0A]);
    }

    #[test]
    fn rle8_mixed() {
        // Size = 7: 2 literals, then run of 5
        let data = [
            7, 0, 0, 0, // decompressed size = 7
            0x01, 0x02, // two literals
            0x45, 0x0A, // run of 5 x 0x0A
        ];
        let result = decompress_rle8(&data).unwrap();
        assert_eq!(result, vec![0x01, 0x02, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A]);
    }

    #[test]
    fn rle8_run_length_zero_means_256() {
        // Size = 256, run with length 0 (meaning 256) x color 0x05
        let data = [
            0, 1, 0, 0, // decompressed size = 256
            0x40, 0x05, // bit6 set, lower 6 bits = 0 -> run of 256, color = 0x05
        ];
        let result = decompress_rle8(&data).unwrap();
        assert_eq!(result.len(), 256);
        assert!(result.iter().all(|&b| b == 0x05));
    }

    #[test]
    fn rle8_too_short() {
        let data = [1, 0, 0]; // less than 4 bytes
        let err = decompress_rle8(&data).unwrap_err();
        assert!(err.to_string().contains("too short"));
    }

    // ---- RLE5 tests ----

    #[test]
    fn rle5_run_length_mode() {
        // Decompressed size = 5
        // Byte: color=3, flags=0 -> run mode, next byte=5 -> output 3 five times
        let data = [
            5, 0, 0, 0, // decompressed size = 5
            0x03,       // lower 5 bits = 3, upper 3 bits = 0 (run mode)
            5,          // repeat count = 5
        ];
        let result = decompress_rle5(&data).unwrap();
        assert_eq!(result, vec![3, 3, 3, 3, 3]);
    }

    #[test]
    fn rle5_literal_mode() {
        // Decompressed size = 3
        // Byte: color=1, flags=3 -> output color 1, then read 2 more literal bytes
        let data = [
            3, 0, 0, 0, // decompressed size = 3
            0x61,       // flags=3 (0b011 << 5 = 0x60), color=1 (0x01) -> 0x61
            0x42,       // lower 5 bits = 2 (flags ignored for subsequent)
            0x63,       // lower 5 bits = 3
        ];
        let result = decompress_rle5(&data).unwrap();
        assert_eq!(result, vec![1, 2, 3]);
    }

    #[test]
    fn rle5_mixed() {
        // Decompressed size = 7
        // First: literal mode flags=2, color=5 -> output 5, then read 1 more (color=10)
        // Second: run mode, color=7, count=5 -> output 7 five times
        let data = [
            7, 0, 0, 0,
            0x45,       // flags=2 (0b010 << 5 = 0x40), color=5 -> 0x45
            0x0A,       // literal: lower 5 bits = 10
            0x07,       // flags=0, color=7 -> run mode
            5,          // repeat count = 5
        ];
        let result = decompress_rle5(&data).unwrap();
        assert_eq!(result, vec![5, 10, 7, 7, 7, 7, 7]);
    }

    #[test]
    fn rle5_empty_output() {
        let data = [0, 0, 0, 0]; // decompressed size = 0
        let result = decompress_rle5(&data).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn rle5_too_short() {
        let data = [1, 0, 0]; // less than 4 bytes
        let err = decompress_rle5(&data).unwrap_err();
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn rle5_truncated_run() {
        // Run mode but no repeat-count byte follows
        let data = [
            5, 0, 0, 0,
            0x03, // flags=0 (run mode), color=3, but no count byte
        ];
        let err = decompress_rle5(&data).unwrap_err();
        assert!(err.to_string().contains("unexpected end"));
    }

    // ---- LZ5 tests ----

    #[test]
    fn lz5_literals_only() {
        // Decompressed size = 3, control byte = 0b00000111 (3 literal bits)
        let data = [
            3, 0, 0, 0, // decompressed size = 3
            0x07,       // control: bits 0,1,2 = 1 (literal), rest 0
            0xAA,       // literal byte 1
            0xBB,       // literal byte 2
            0xCC,       // literal byte 3
        ];
        let result = decompress_lz5(&data).unwrap();
        assert_eq!(result, vec![0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn lz5_back_reference() {
        // Write 2 literal bytes, then back-reference them
        // Decompressed size = 4
        // Control byte: bits 0,1 = 1 (literal), bit 2 = 0 (back-ref)
        // Control = 0b00000011 = 0x03
        // Back-ref: offset = 0, length_bits = 0 -> copy_length = 0 + 2 = 2
        // value = (0 << 13) | 0 = 0x0000
        let data = [
            4, 0, 0, 0,
            0x03,       // control: bit0=1(lit), bit1=1(lit), bit2=0(ref)
            0x11,       // literal: 0x11 -> recycling_buf[0]
            0x22,       // literal: 0x22 -> recycling_buf[1]
            0x00, 0x00, // back-ref: offset=0, length=0+2=2 -> copies buf[0],buf[1]
        ];
        let result = decompress_lz5(&data).unwrap();
        assert_eq!(result, vec![0x11, 0x22, 0x11, 0x22]);
    }

    #[test]
    fn lz5_empty_output() {
        let data = [0, 0, 0, 0]; // decompressed size = 0
        let result = decompress_lz5(&data).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn lz5_too_short() {
        let data = [1, 0]; // less than 4 bytes
        let err = decompress_lz5(&data).unwrap_err();
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn lz5_longer_back_reference() {
        // Write 1 literal byte, then back-reference it with length 5
        // Decompressed size = 6
        // Control = 0b00000001 = 0x01 (bit0=1 literal, bit1=0 ref)
        // Back-ref: offset=0, length_bits=3 -> copy_length=3+2=5
        // value = (3 << 13) | 0 = 0x6000
        let data = [
            6, 0, 0, 0,
            0x01,       // control: bit0=1(lit), bit1=0(ref)
            0xFF,       // literal: 0xFF -> recycling_buf[0]
            0x00, 0x60, // back-ref: 0x6000 LE -> offset=0, length=3+2=5
        ];
        let result = decompress_lz5(&data).unwrap();
        assert_eq!(result, vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn png_stub_returns_error() {
        let err = decompress_png(&[]).unwrap_err();
        assert!(err.to_string().contains("PNG"));
    }
}
