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
| 28 | 4 | u32 | Number of groups |
| 32 | 4 | u32 | Number of sprites |
| 36 | 4 | u32 | Sprite sub-header offset |
| 40 | 4 | u32 | Sprite sub-header block length |
| 44 | 4 | u32 | Palette sub-header offset |
| 48 | 4 | u32 | Palette sub-header block length |
| 52 | 4 | u32 | LData offset |
| 56 | 4 | u32 | LData length |
| 60 | 4 | u32 | TData offset |
| 64 | 4 | u32 | TData length |

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

Palette data is stored as 256 RGB triplets (768 bytes) in the LData block.

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
