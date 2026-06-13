//! SFF v2 sprite decompression routines.
//!
//! Implements the RLE8, RLE5, and LZ5 decompression algorithms used by SFF v2
//! sprite data. PNG-based formats return an unsupported-format error.

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
            output.extend(std::iter::repeat_n(color, actual_len));
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
/// RLE5 is MUGEN's two-level run encoder. The first 4 bytes are a little-endian
/// `u32` giving the decompressed size; the codec stream follows.
///
/// The stream is a sequence of *packets*. Each packet begins with a header:
/// - a **run-length** byte `rl` (full 8 bits),
/// - a **data-length** byte whose low 7 bits (`dl`) count how many *additional*
///   colour segments follow, and whose high bit flags whether an explicit colour
///   byte is present,
/// - if that flag is set, an explicit **colour** byte (a full 8 bits).
///
/// The packet then emits the header colour (0 when the flag was clear) `rl + 1`
/// times, followed by `dl` further segments. Each segment is a single byte that
/// packs the colour in its low 5 bits and `run_length - 1` in its high 3 bits,
/// so it emits `(byte >> 5) + 1` copies of `byte & 0x1f`.
///
/// This mirrors the reference Elecbyte / Ikemen GO `Rle5Decode`. The reference
/// drives the inner loop with counters that fall *below zero* as sentinels
/// (`rl < 0` ⇒ pull the next segment; `dl < 0` ⇒ end the packet), so the port
/// uses signed `i32` counters. All indexing is bounds-checked and the read head
/// saturates at the final byte, so malformed input yields a best-effort,
/// zero-padded buffer rather than panicking (never-crash).
pub fn decompress_rle5(data: &[u8]) -> FpResult<Vec<u8>> {
    if data.len() < 4 {
        return Err(FpError::parse(
            "SFF",
            "RLE5 data too short for size header",
        ));
    }

    let decompressed_size =
        u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;

    // A single SFF sprite is at most a few megapixels; reject absurd sizes from a
    // corrupt prefix rather than attempting a multi-gigabyte allocation (never-crash).
    const MAX_RLE5_OUTPUT: usize = 64 * 1024 * 1024;
    if decompressed_size > MAX_RLE5_OUTPUT {
        return Err(FpError::parse(
            "SFF",
            format!("RLE5 declared size {decompressed_size} exceeds sane limit ({MAX_RLE5_OUTPUT})"),
        ));
    }

    // The compressed stream begins after the 4-byte size prefix.
    let rle = &data[4..];
    let mut p = vec![0u8; decompressed_size];

    if rle.is_empty() || decompressed_size == 0 {
        return Ok(p);
    }

    // Saturating advance: like the reference decoder, `i` never moves past the
    // final byte, so a truncated stream re-reads the last byte rather than
    // overrunning — the output-size bound (`j < decompressed_size`) still
    // terminates the loop because every outer packet writes at least one byte.
    let next = |i: &mut usize| {
        if *i < rle.len() - 1 {
            *i += 1;
        }
    };

    let mut i = 0usize; // read head into `rle`
    let mut j = 0usize; // write head into `p`

    while j < decompressed_size {
        // Packet header: 8-bit run length, then a data byte whose low 7 bits are
        // the count of *additional* colour segments and whose high bit flags an
        // explicit colour byte.
        let mut rl = rle[i] as i32;
        next(&mut i);

        let mut dl = (rle[i] & 0x7f) as i32;
        let mut c = 0u8;
        if rle[i] >> 7 != 0 {
            next(&mut i);
            c = rle[i];
        }
        next(&mut i);

        // Emit the header colour `rl + 1` times, then `dl` further segments. The
        // negative-going `rl`/`dl` counters mirror the reference exactly: when
        // `rl` falls below zero we pull the next segment's colour (low 5 bits)
        // and run length (high 3 bits); when `dl` falls below zero the packet is
        // done. Because the first write always lands (the outer guard ensures
        // `j < decompressed_size` on entry), the write head always advances.
        loop {
            if j < decompressed_size {
                p[j] = c;
                j += 1;
            }
            rl -= 1;
            if rl < 0 {
                dl -= 1;
                if dl < 0 {
                    break;
                }
                c = rle[i] & 0x1f;
                rl = (rle[i] >> 5) as i32;
                next(&mut i);
            }
        }
    }

    if j < decompressed_size {
        tracing::warn!(
            expected = decompressed_size,
            actual = j,
            "RLE5 decompressed data shorter than expected, padding with zeros"
        );
        // `p` is already zero-initialized to `decompressed_size`.
    }

    Ok(p)
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
    //
    // These exercise the *real* MUGEN RLE5 codec (Elecbyte / Ikemen GO
    // `Rle5Decode`), not the synthetic "5-bit colour + 3-bit flag" scheme the
    // previous tests asserted. A packet is: an 8-bit run-length byte `rl`, a
    // data byte (low 7 bits = count of additional segments `dl`, high bit =
    // "explicit colour follows"), an optional full-byte colour, then `dl`
    // segment bytes (`(b >> 5) + 1` copies of `b & 0x1f`). The header colour is
    // emitted `rl + 1` times. Each sequence below is hand-traced in its comments.

    #[test]
    fn rle5_single_run_explicit_color() {
        // size = 5. One packet, no extra segments, explicit colour 7.
        //   rl byte   = 0x04            -> rl = 4  (emit colour rl+1 = 5 times)
        //   data byte = 0x80            -> dl = 0, high bit set -> colour follows
        //   colour    = 0x07            -> c = 7
        //   inner: write 7 five times (rl: 4,3,2,1,0 then -1); dl-- = -1 -> stop.
        let data = [
            5, 0, 0, 0, //
            0x04,       // rl = 4
            0x80,       // dl = 0, explicit-colour flag set
            0x07,       // colour = 7
        ];
        let result = decompress_rle5(&data).unwrap();
        assert_eq!(result, vec![7, 7, 7, 7, 7]);
    }

    #[test]
    fn rle5_implicit_zero_color_run() {
        // size = 5. No explicit-colour flag -> header colour defaults to 0.
        //   rl byte   = 0x04 -> rl = 4 (emit 0 five times)
        //   data byte = 0x00 -> dl = 0, flag clear -> c stays 0
        let data = [
            5, 0, 0, 0, //
            0x04,       // rl = 4
            0x00,       // dl = 0, no explicit colour -> colour 0
        ];
        let result = decompress_rle5(&data).unwrap();
        assert_eq!(result, vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn rle5_multi_segment_packet() {
        // size = 6. One packet: header colour once, then two extra segments.
        //   rl byte   = 0x00 -> rl = 0  (emit header colour rl+1 = 1 time)
        //   data byte = 0x82 -> dl = 2, explicit-colour flag set
        //   colour    = 0x05 -> c = 5   -> emit [5]
        //   segment 1 = 0x23 -> colour 0x23&0x1f = 3, run (0x23>>5)+1 = 2 -> [3,3]
        //   segment 2 = 0x47 -> colour 0x47&0x1f = 7, run (0x47>>5)+1 = 3 -> [7,7,7]
        let data = [
            6, 0, 0, 0, //
            0x00,       // rl = 0
            0x82,       // dl = 2, explicit-colour flag set
            0x05,       // header colour = 5
            0x23,       // segment: colour 3, run 2
            0x47,       // segment: colour 7, run 3
        ];
        let result = decompress_rle5(&data).unwrap();
        assert_eq!(result, vec![5, 3, 3, 7, 7, 7]);
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
    fn rle5_flag_clear_with_segments() {
        // size = 4. Header data byte has the high bit CLEAR, so the header colour
        // defaults to 0 and NO explicit colour byte is read; dl = 1 still pulls one
        // segment.
        //   rl byte = 0x00 -> rl = 0 (emit header colour 0 once) -> [0]
        //   data    = 0x01 -> dl = 1, high bit clear -> colour 0, no colour byte
        //   segment = 0x47 -> colour 7, run (0x47>>5)+1 = 3 -> [7,7,7]
        let data = [4, 0, 0, 0, 0x00, 0x01, 0x47];
        let result = decompress_rle5(&data).unwrap();
        assert_eq!(result, vec![0, 7, 7, 7]);
    }

    #[test]
    fn rle5_multiple_packets() {
        // size = 3. Packet 1: rl=1 -> emit colour 5 twice ([5,5]); Packet 2: rl=0 ->
        // emit colour 9 once ([9]). Both set the explicit-colour flag (0x80).
        let data = [
            3, 0, 0, 0, //
            0x01, 0x80, 0x05, // packet 1 -> [5, 5]
            0x00, 0x80, 0x09, // packet 2 -> [9]
        ];
        let result = decompress_rle5(&data).unwrap();
        assert_eq!(result, vec![5, 5, 9]);
    }

    #[test]
    fn rle5_rejects_absurd_size() {
        // A corrupt 4-byte prefix declaring 0xFFFFFFFF (~4 GB) must be rejected
        // before any allocation rather than aborting the process on OOM
        // (never-crash). At least one codec byte follows so the size check, not the
        // too-short guard, is what fires.
        let data = [0xFF, 0xFF, 0xFF, 0xFF, 0x00];
        let err = decompress_rle5(&data).unwrap_err();
        assert!(err.to_string().contains("exceeds sane limit"));
    }

    #[test]
    fn rle5_truncated_stream_pads_without_panicking() {
        // Declares 8 output bytes but supplies only one packet's header. The
        // decoder must terminate (never-crash) and zero-pad to the declared size.
        //   rl byte = 0x02 -> rl = 2 (emit colour 3 times)
        //   data    = 0x80 -> dl = 0, explicit-colour flag set
        //   colour  = 0x09 -> c = 9  -> emit [9,9,9]
        // The read head then saturates on the last byte; remaining packets write
        // zero-derived padding until the 8-byte buffer is full.
        let data = [
            8, 0, 0, 0, //
            0x02,       // rl = 2
            0x80,       // dl = 0, explicit-colour flag set
            0x09,       // colour = 9
        ];
        let result = decompress_rle5(&data).unwrap();
        assert_eq!(result.len(), 8, "output must match declared size");
        assert_eq!(&result[0..3], &[9, 9, 9], "real pixels preserved");
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
