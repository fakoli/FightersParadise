//! Real-content coverage for the SFF v2 **RLE5** sprite codec.
//!
//! The shipping Kung Fu Man fixture (`kfm.sff`) contains only RLE8 + LZ5
//! sprites — zero RLE5 — so it gives the RLE5 decoder no end-to-end coverage.
//! Rather than leave that path exercised only by in-module unit tests, this
//! integration test *builds* a genuine SFF v2 container holding a single RLE5
//! sprite and decodes it through the public on-disk API
//! (`read` -> parse header -> parse sub-headers -> `decode_sprite` ->
//! `decompress_rle5`).
//!
//! Because the fixture is synthesized at runtime, the test always runs —
//! including on CI where no real RLE5 asset exists.

use fp_formats::sff::{SffFile, SpriteFormat};
use std::sync::atomic::{AtomicU64, Ordering};

/// Builds a complete, valid SFF v2 file holding a single RLE5-compressed sprite.
///
/// The layout matches the SFF v2 header the current parser expects: a 512-byte
/// header (12 reserved bytes after the version, then `num_groups`, `num_sprites`,
/// and the sprite/palette/LData/TData offset+length pairs), one 28-byte sprite
/// sub-header, one 16-byte palette sub-header, a 768-byte LData palette block,
/// and a TData block carrying the RLE5 codec stream.
///
/// The codec stream `[0x00, 0x82, 0x05, 0x23, 0x47]` (after its 4-byte LE
/// decompressed-size prefix `6,0,0,0`) decodes to the 6 palette indices
/// `[5, 3, 3, 7, 7, 7]`:
///   - header: `rl = 0` (emit the colour once), data byte `0x82` -> `dl = 2`
///     (two further segments) with the high bit set, so an explicit colour byte
///     `0x05` follows -> emit `[5]`
///   - segment `0x23`: colour `0x23 & 0x1f = 3`, run `(0x23 >> 5) + 1 = 2` -> `[3, 3]`
///   - segment `0x47`: colour `0x47 & 0x1f = 7`, run `(0x47 >> 5) + 1 = 3` -> `[7, 7, 7]`
fn synthesize_rle5_sff() -> Vec<u8> {
    // RLE5 codec stream: 4-byte LE decompressed size (6) followed by the packet.
    let rle5: [u8; 9] = [6, 0, 0, 0, 0x00, 0x82, 0x05, 0x23, 0x47];

    let sprite_offset: u32 = 512;
    let sprite_length: u32 = 28; // 1 sprite sub-header
    let palette_offset: u32 = 540;
    let palette_length: u32 = 16; // 1 palette sub-header
    let ldata_offset: u32 = 556;
    let ldata_length: u32 = 768; // 256 RGB triples
    let tdata_offset: u32 = ldata_offset + ldata_length; // 1324
    let tdata_length: u32 = rle5.len() as u32;

    let total = tdata_offset as usize + tdata_length as usize;
    let mut buf = vec![0u8; total];

    // --- Header ---
    buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
    buf[12] = 0; // version minor3
    buf[13] = 0; // version minor2
    buf[14] = 1; // version minor1
    buf[15] = 2; // version major -> SFF v2
    // buf[16..28] = 12 reserved bytes (left zero)
    buf[28..32].copy_from_slice(&0u32.to_le_bytes()); // num_groups
    buf[32..36].copy_from_slice(&1u32.to_le_bytes()); // num_sprites
    buf[36..40].copy_from_slice(&sprite_offset.to_le_bytes());
    buf[40..44].copy_from_slice(&sprite_length.to_le_bytes());
    buf[44..48].copy_from_slice(&palette_offset.to_le_bytes());
    buf[48..52].copy_from_slice(&palette_length.to_le_bytes());
    buf[52..56].copy_from_slice(&ldata_offset.to_le_bytes());
    buf[56..60].copy_from_slice(&ldata_length.to_le_bytes());
    buf[60..64].copy_from_slice(&tdata_offset.to_le_bytes());
    buf[64..68].copy_from_slice(&tdata_length.to_le_bytes());

    // --- Sprite sub-header (28 bytes) at 512: a 3x2 RLE5 sprite living in TData ---
    let s = sprite_offset as usize;
    buf[s..s + 2].copy_from_slice(&0u16.to_le_bytes()); // group
    buf[s + 2..s + 4].copy_from_slice(&0u16.to_le_bytes()); // image
    buf[s + 4..s + 6].copy_from_slice(&3u16.to_le_bytes()); // width = 3
    buf[s + 6..s + 8].copy_from_slice(&2u16.to_le_bytes()); // height = 2 (3*2 = 6 px)
    buf[s + 8..s + 10].copy_from_slice(&0i16.to_le_bytes()); // axis_x
    buf[s + 10..s + 12].copy_from_slice(&0i16.to_le_bytes()); // axis_y
    buf[s + 12..s + 14].copy_from_slice(&0u16.to_le_bytes()); // linked_index = self
    buf[s + 14] = 3; // format = RLE5
    buf[s + 15] = 8; // color_depth
    buf[s + 16..s + 20].copy_from_slice(&0u32.to_le_bytes()); // data_offset within TData
    buf[s + 20..s + 24].copy_from_slice(&tdata_length.to_le_bytes()); // data_length
    buf[s + 24..s + 26].copy_from_slice(&0u16.to_le_bytes()); // palette_index
    buf[s + 26..s + 28].copy_from_slice(&1u16.to_le_bytes()); // flags: bit0 = use TData

    // --- Palette sub-header (16 bytes) at 540 ---
    let p = palette_offset as usize;
    buf[p..p + 2].copy_from_slice(&1u16.to_le_bytes()); // group
    buf[p + 2..p + 4].copy_from_slice(&1u16.to_le_bytes()); // item
    buf[p + 4..p + 6].copy_from_slice(&256u16.to_le_bytes()); // num_colors
    buf[p + 6..p + 8].copy_from_slice(&0u16.to_le_bytes()); // linked_index = self
    buf[p + 8..p + 12].copy_from_slice(&0u32.to_le_bytes()); // data_offset in LData
    buf[p + 12..p + 16].copy_from_slice(&ldata_length.to_le_bytes()); // data_length

    // --- LData palette (RGB triples) at 556: give colours 3/5/7 distinct reds ---
    let l = ldata_offset as usize;
    buf[l + 3 * 3] = 0x30; // colour index 3, R channel
    buf[l + 5 * 3] = 0x50; // colour index 5, R channel
    buf[l + 7 * 3] = 0x70; // colour index 7, R channel

    // --- TData: RLE5 codec stream at 1324 ---
    let t = tdata_offset as usize;
    buf[t..t + rle5.len()].copy_from_slice(&rle5);

    buf
}

/// Decodes a synthesized RLE5 sprite end-to-end through the public file API.
///
/// Never skips: the fixture is generated at runtime, so the RLE5 codec always
/// has a full-pipeline regression guard regardless of which local assets exist.
#[test]
fn synthetic_rle5_sff_decodes_end_to_end() {
    let bytes = synthesize_rle5_sff();

    // Exercise the real file-loading path, not just in-memory parsing. Combining
    // the PID with a process-lifetime atomic counter keeps the name unique across
    // concurrent tests in this binary and across repeated runs.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "fp_formats_rle5_{}_{}.sff",
        std::process::id(),
        unique
    ));
    std::fs::write(&path, &bytes).expect("write synthetic RLE5 SFF fixture");

    let loaded = SffFile::load(&path);
    // Remove the temp file before asserting so a failure never leaks it.
    let _ = std::fs::remove_file(&path);
    let sff = loaded.expect("synthetic RLE5 SFF should load");

    assert_eq!(sff.sprites.len(), 1, "fixture declares exactly one sprite");

    let sprite = &sff.sprites[0];
    assert_eq!(
        sprite.format,
        SpriteFormat::Rle5,
        "the fixture's sprite must be RLE5-encoded"
    );
    let expected_px = sprite.width as usize * sprite.height as usize;
    assert_eq!(expected_px, 6, "fixture sprite is 3x2");

    let pixels = sff.decode_sprite(0).expect("RLE5 sprite should decode");
    assert_eq!(
        pixels.len(),
        expected_px,
        "decoded pixel count must equal width*height"
    );
    assert_eq!(
        pixels,
        vec![5, 3, 3, 7, 7, 7],
        "RLE5 stream must decode to the hand-traced palette indices"
    );
}
