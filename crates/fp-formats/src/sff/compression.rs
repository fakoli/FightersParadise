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
/// LZ5 is MUGEN's bespoke LZ77 variant for SFF v2. The first 4 bytes are a
/// little-endian `u32` giving the decompressed size; the bit-stream follows.
///
/// Decoding processes tokens grouped under a *control byte*: its 8 bits (LSB
/// first) flag each token as a **back-reference** (bit set) or a **literal run**
/// (bit clear). A fresh control byte is fetched after every 8 tokens.
///
/// - **Literal run** (control bit clear): the token byte `d` is either a short
///   run (`d & 0xE0 != 0`) whose length is `d >> 5` and color is `d & 0x1F`, or a
///   long run (`d & 0xE0 == 0`) whose length is `next_byte + 8` of color 0.
/// - **Back-reference** (control bit set): copies already-emitted bytes at a
///   relative distance `d` behind the write head. Short references pack the high
///   two bits of `d` across consecutive tokens via a "recycled bits" accumulator
///   (`rb`/`rbc`); long references (`d & 0x3F == 0`) spell out the distance and
///   length in following bytes.
///
/// This mirrors the reference Elecbyte / Ikemen GO algorithm. All indexing is
/// bounds-checked so malformed input yields a best-effort, zero-padded buffer
/// rather than panicking (never-crash).
pub fn decompress_lz5(data: &[u8]) -> FpResult<Vec<u8>> {
    if data.len() < 4 {
        return Err(FpError::parse(
            "SFF",
            "LZ5 data too short for size header",
        ));
    }

    let decompressed_size =
        u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;

    // The compressed bit-stream begins after the 4-byte size prefix.
    let rle = &data[4..];
    let mut p = vec![0u8; decompressed_size];

    if rle.is_empty() || decompressed_size == 0 {
        return Ok(p);
    }

    // `i` advances through `rle`; like the reference decoder it saturates at the
    // last byte (`next` below) so a truncated stream re-reads the final byte
    // rather than overrunning — the output-size bound still terminates the loop.
    let mut i = 0usize;
    let mut j = 0usize; // write head into `p`
    let mut n: usize; // current run / copy length

    // Recycled-bits accumulator for short back-reference distances.
    let mut rb: u8 = 0;
    let mut rbc: u32 = 0;

    let next = |i: &mut usize| {
        if *i < rle.len() - 1 {
            *i += 1;
        }
    };

    let mut ct = rle[i];
    let mut cts: u32 = 0;
    next(&mut i);

    while j < decompressed_size {
        let d_byte = rle[i];
        next(&mut i);

        if ct & (1u8 << cts) != 0 {
            // Back-reference token.
            let dist: usize;
            if d_byte & 0x3f == 0 {
                // Long reference: distance and length spelled out in next bytes.
                let dd = ((d_byte as usize) << 2 | rle[i] as usize) + 1;
                next(&mut i);
                n = rle[i] as usize + 2;
                next(&mut i);
                dist = dd;
            } else {
                // Short reference: accumulate the high two bits across tokens.
                rb |= (d_byte & 0xc0) >> rbc;
                rbc += 2;
                n = (d_byte & 0x3f) as usize;
                if rbc < 8 {
                    dist = rle[i] as usize + 1;
                    next(&mut i);
                } else {
                    dist = rb as usize + 1;
                    rb = 0;
                    rbc = 0;
                }
            }
            // Copy `n + 1` bytes from `dist` behind the write head.
            loop {
                if j < decompressed_size && dist <= j {
                    p[j] = p[j - dist];
                    j += 1;
                } else if j < decompressed_size {
                    // Distance points before the start of output: emit 0 to keep
                    // the geometry intact instead of panicking on `j - dist`.
                    p[j] = 0;
                    j += 1;
                }
                if n == 0 {
                    break;
                }
                n -= 1;
            }
        } else {
            // Literal-run token.
            if d_byte & 0xe0 == 0 {
                // Long run of zeros: length spelled out in the next byte.
                n = rle[i] as usize + 8;
                next(&mut i);
                for _ in 0..n {
                    if j >= decompressed_size {
                        break;
                    }
                    p[j] = 0;
                    j += 1;
                }
            } else {
                // Short run: count in the high 3 bits, color in the low 5.
                n = (d_byte >> 5) as usize;
                let color = d_byte & 0x1f;
                while n > 0 {
                    if j >= decompressed_size {
                        break;
                    }
                    p[j] = color;
                    j += 1;
                    n -= 1;
                }
            }
        }

        cts += 1;
        if cts >= 8 {
            ct = rle[i];
            cts = 0;
            next(&mut i);
        }
    }

    if j < decompressed_size {
        tracing::warn!(
            expected = decompressed_size,
            actual = j,
            "LZ5 decompressed data shorter than expected, padding with zeros"
        );
        // `p` is already zero-initialized to `decompressed_size`.
    }

    Ok(p)
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

    // ---- LZ5 tests ----
    //
    // These exercise the *real* MUGEN LZ5 codec (Elecbyte / Ikemen GO layout),
    // not a generic LZ77. The control byte's bits run LSB-first: a clear bit is a
    // literal run, a set bit is a back-reference. Each test byte sequence below is
    // hand-traced in its comments. The decoder is validated end-to-end against the
    // real `kfm.sff` fixture in `tests/real_content.rs`.

    #[test]
    fn lz5_short_literal_runs() {
        // size = 3. Control = 0x00 (both tokens are literals).
        //   token 0xC3? No — short-literal run: length = d>>5, color = d&0x1f.
        //   token 0x43 -> n=2, color=3 -> emit 3,3
        //   token 0x27 -> n=1, color=7 -> emit 7
        let data = [
            3, 0, 0, 0, //
            0x00,       // control: bits clear -> literal tokens
            0x43,       // n=2 (0x43>>5), color=3 (0x43&0x1f)
            0x27,       // n=1, color=7
        ];
        let result = decompress_lz5(&data).unwrap();
        assert_eq!(result, vec![3, 3, 7]);
    }

    #[test]
    fn lz5_long_zero_run() {
        // A literal token with the top 3 bits clear (d & 0xe0 == 0) is a long run
        // of zeros: length = next_byte + 8.
        let data = [
            8, 0, 0, 0, //
            0x00,       // control: bit0 clear -> literal
            0x00,       // d & 0xe0 == 0 -> long zero run
            0x00,       // length = 0 + 8 = 8 zeros
        ];
        let result = decompress_lz5(&data).unwrap();
        assert_eq!(result, vec![0; 8]);
    }

    #[test]
    fn lz5_short_back_reference() {
        // Emit one literal, then a short back-reference that copies it forward.
        //   control = 0x02: bit0 clear (literal), bit1 set (back-ref)
        //   literal token 0x29 -> n=1, color=9 -> emit 9
        //   back-ref token 0x01: d&0x3f=1 (n=1 -> copy n+1 = 2 bytes),
        //     rbc<8 so distance = next_byte + 1 = 0 + 1 = 1
        //   copy 2 bytes at distance 1: p[1]=p[0]=9, p[2]=p[1]=9
        let data = [
            3, 0, 0, 0, //
            0x02,       // control: bit0=lit, bit1=back-ref
            0x29,       // literal: n=1, color=9
            0x01,       // short back-ref: n=1, distance from next byte
            0x00,       // distance - 1 = 0 -> distance = 1
        ];
        let result = decompress_lz5(&data).unwrap();
        assert_eq!(result, vec![9, 9, 9]);
    }

    #[test]
    fn lz5_long_back_reference() {
        // Emit 1,2,3 as literals, then a long back-reference copying them.
        //   control = 0x08: bits 0..2 clear (literals), bit3 set (back-ref)
        //   literals 0x21,0x22,0x23 -> emit 1,2,3
        //   long back-ref: token 0x00 (d & 0x3f == 0) ->
        //     dist = (0<<2 | next=0x02) + 1 = 3
        //     n    = next2=0x00 + 2 = 2 -> copy n+1 = 3 bytes
        //   copy 3 bytes at distance 3: p[3..6] = p[0..3] = 1,2,3
        let data = [
            6, 0, 0, 0, //
            0x08,       // control: 3 literals then a back-ref
            0x21, 0x22, 0x23, // literals 1, 2, 3
            0x00,       // long back-ref marker (d & 0x3f == 0)
            0x02,       // distance bytes -> dist = 3
            0x00,       // length byte -> n = 2 -> copy 3 bytes
        ];
        let result = decompress_lz5(&data).unwrap();
        assert_eq!(result, vec![1, 2, 3, 1, 2, 3]);
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
    fn lz5_truncated_stream_pads_without_panicking() {
        // Declares 64 output bytes but supplies almost no stream. The decoder
        // must terminate (never-crash) and zero-pad to the declared size.
        let data = [
            64, 0, 0, 0, //
            0x00,        // control: literal
            0x21,        // single literal token (n=1, color=1)
        ];
        let result = decompress_lz5(&data).unwrap();
        assert_eq!(result.len(), 64, "output must match declared size");
        // First byte is the one real pixel; the remainder is zero padding.
        assert_eq!(result[0], 1);
    }

    #[test]
    fn lz5_output_only_prefix_yields_zeroes() {
        // Just the 4-byte size prefix with an empty bit-stream: a valid, fully
        // zero-padded buffer, never an error or panic.
        let data = [5, 0, 0, 0];
        let result = decompress_lz5(&data).unwrap();
        assert_eq!(result, vec![0; 5]);
    }

    #[test]
    fn png_stub_returns_error() {
        let err = decompress_png(&[]).unwrap_err();
        assert!(err.to_string().contains("PNG"));
    }
}
