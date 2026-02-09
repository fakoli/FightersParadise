# Changelog

All notable changes to Fighters Paradise will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **fp-input**: Input state types (Button, Direction, InputState), 60-frame ring buffer, MUGEN command sequence matcher with `compile_command()` parser
- **fp-physics**: PhysicsBody with Euler integration, ground plane clamping (Y=0), gravity, `on_ground()`/`in_air()` predicates
- **fp-formats/cmd**: CMD file parser for MUGEN input command definitions (`[Command]` and `[Defaults]` sections)
- **fp-app**: Playable character mode with SDL2 key mapping, hardcoded common state machine (idle, walk, crouch, jump), physics-driven movement
- 45 new unit tests (131 total across workspace)

## [0.1.0] - 2025-01-01

### Phase 2: Animate a Character
- **fp-formats/air**: AIR animation file parser with frame sequences, collision boxes (Clsn1/Clsn2), blend modes, flip flags, loopstart
- **fp-render**: AnimController for animation playback with tick timing, looping, and frame advancement
- **fp-app**: Animation viewer mode with Left/Right arrow action cycling and Space to restart

### Phase 1: Draw a Sprite
- **fp-core**: Vec2, Rect, SpriteId, AnimId, SoundId, FpError/FpResult
- **fp-formats/sff**: SFF v2 parser with RLE5, RLE8, LZ5 decompression, linked sprites, shared palettes
- **fp-formats/def**: DEF file parser (INI-style key-value config)
- **fp-render**: wgpu rendering pipeline with R8Unorm indexed textures, WGSL palette lookup shader, orthographic projection
- **fp-app**: SDL2 window with 60Hz fixed timestep game loop, test pattern, static sprite display
- Cargo workspace with 14 crates
