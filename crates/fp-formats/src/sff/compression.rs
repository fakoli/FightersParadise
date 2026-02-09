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

/// Stub for RLE5 decompression (not yet implemented).
pub fn decompress_rle5(_data: &[u8]) -> FpResult<Vec<u8>> {
    tracing::warn!("RLE5 decompression not yet implemented");
    Err(FpError::Unsupported(
        "RLE5 sprite decompression".to_string(),
    ))
}

/// Stub for LZ5 decompression (not yet implemented).
pub fn decompress_lz5(_data: &[u8]) -> FpResult<Vec<u8>> {
    tracing::warn!("LZ5 decompression not yet implemented");
    Err(FpError::Unsupported(
        "LZ5 sprite decompression".to_string(),
    ))
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

    #[test]
    fn rle5_stub_returns_error() {
        let err = decompress_rle5(&[]).unwrap_err();
        assert!(err.to_string().contains("RLE5"));
    }

    #[test]
    fn lz5_stub_returns_error() {
        let err = decompress_lz5(&[]).unwrap_err();
        assert!(err.to_string().contains("LZ5"));
    }

    #[test]
    fn png_stub_returns_error() {
        let err = decompress_png(&[]).unwrap_err();
        assert!(err.to_string().contains("PNG"));
    }
}
