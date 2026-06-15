# SFF v2 — Sprite File Format

## Overview

SFF (Sprite File Format) v2 is a binary container for 256-color indexed sprites. Introduced in MUGEN 1.0, it replaces the older SFF v1 format with better compression and palette sharing.

## File Structure

```
┌─────────────────────────────┐
│ Header (512 bytes)          │
├─────────────────────────────┤
│ Sprite Sub-Headers          │
│ (28 bytes each)             │
├─────────────────────────────┤
│ Palette Sub-Headers         │
│ (16 bytes each)             │
├─────────────────────────────┤
│ LData Block                 │
│ (literal/large data)        │
├─────────────────────────────┤
│ TData Block                 │
│ (translate data)            │
└─────────────────────────────┘
```

## Header Layout (512 bytes)

| Offset | Size | Type | Description |
|--------|------|------|-------------|
| 0 | 12 | string | Signature: `ElecbyteSpr\0` |
| 12 | 1 | u8 | Version minor3 (lo) |
| 13 | 1 | u8 | Version minor2 |
| 14 | 1 | u8 | Version minor1 |
| 15 | 1 | u8 | Version major (hi) = 2 |
| 16 | 4 | u32 | Reserved (low compat version) |
| 20 | 16 | bytes | Reserved |
| 36 | 4 | u32 | Sprite node-list offset |
| 40 | 4 | u32 | Number of sprites |
| 44 | 4 | u32 | Palette node-list offset |
| 48 | 4 | u32 | Number of palettes |
| 52 | 4 | u32 | LData offset |
| 56 | 4 | u32 | LData length |
| 60 | 4 | u32 | TData offset |
| 64 | 4 | u32 | TData length |

> **Correction (2026-06-13, task 0.3):** An earlier version of this table placed the sprite/palette
> **counts** at offsets 28/32 and labeled 40/48 as "block length". That was wrong — real MUGEN 1.0
> files (e.g. `kfm.sff`) store the **counts** at offsets **40** (sprites) and **48** (palettes), with
> the node-list **offsets** at 36/44. The old layout made the parser read 0 sprites from genuine
> files while all synthetic tests (which encoded the same wrong layout) passed. The implementation in
> `crates/fp-formats/src/sff/header.rs` reads these by absolute offset and is the source of truth.

## Sprite Sub-Header (28 bytes)

| Offset | Size | Type | Description |
|--------|------|------|-------------|
| 0 | 2 | u16 | Group number |
| 2 | 2 | u16 | Image number |
| 4 | 2 | u16 | Width in pixels |
| 6 | 2 | u16 | Height in pixels |
| 8 | 2 | i16 | X axis offset |
| 10 | 2 | i16 | Y axis offset |
| 12 | 2 | u16 | Linked sprite index |
| 14 | 1 | u8 | Compression format |
| 15 | 1 | u8 | Color depth (8/24/32) |
| 16 | 4 | u32 | Data offset in block |
| 20 | 4 | u32 | Data length |
| 24 | 2 | u16 | Palette index |
| 26 | 2 | u16 | Flags (bit 0: data block) |

### Compression Formats
- 0: Raw (uncompressed)
- 2: RLE8 (run-length encoded, 8-bit)
- 3: RLE5 (run-length encoded, 5-bit)
- 4: LZ5 (LZ77 variant)
- 10: PNG8 (8-bit PNG)
- 11: PNG24 (24-bit PNG)
- 12: PNG32 (32-bit PNG)

### Flags
- Bit 0: 0 = data is in LData block, 1 = data is in TData block

## Palette Sub-Header (16 bytes)

| Offset | Size | Type | Description |
|--------|------|------|-------------|
| 0 | 2 | u16 | Group number |
| 2 | 2 | u16 | Item number |
| 4 | 2 | u16 | Number of colors |
| 6 | 2 | u16 | Linked palette index |
| 8 | 4 | u32 | Data offset in LData |
| 12 | 4 | u32 | Data length |

Palette data is stored in the LData block as `num_colors` **RGBA** quadruplets — 4
bytes per color, so a full 256-color palette is 1024 bytes (and `data_length`
sizes it: e.g. KFM's 32-color per-sprite palettes are 128 bytes). The 4th byte
is reserved/padding (typically `0`), **not** a usable per-color alpha: the
decoder forces index 0 transparent and every other color opaque.

> **Correction (task T001):** the format is **RGBA**, not the RGB triplets (768
> bytes) an earlier note claimed — that is the SFF **v1** trailing-PCX layout.
> Reading v2 palettes through the v1 RGB path mis-strided the colors and rendered
> v2 characters (e.g. KFM) as black silhouettes. `SffFile::palette()` is now
> version-aware: v1 = RGB→RGBA, v2 = `num_colors` RGBA quadruplets.

## RLE8 Decompression

```
For each byte in input:
  If bit 6 is clear (byte & 0x40 == 0):
    Output byte as literal pixel
  If bit 6 is set:
    run_length = byte & 0x3F (lower 6 bits)
    if run_length == 0: run_length = 256
    color = next byte
    Output color repeated run_length times
```

## Linked Sprites and Palettes

Sprites and palettes can be "linked" — sharing data with another entry to save space. When a sprite's linked index differs from its own index, it uses the pixel data from the linked sprite. Same for palettes.
