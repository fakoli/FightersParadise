# Fighters Paradise

A modern reimplementation of the MUGEN 2D fighting game engine in Rust, with full backward compatibility for existing MUGEN content (.sff, .air, .cns, .cmd, .def, .snd files).

## Build & Run

```bash
cargo build --workspace          # Build everything
cargo run -p fp-app              # Run with test pattern
cargo run -p fp-app -- path.sff  # Run with SFF file
cargo test --workspace           # Run all tests
cargo clippy --workspace         # Lint (must pass with zero warnings)
```

**macOS prerequisite**: SDL2 must be installed via Homebrew (`brew install sdl2`). The `.cargo/config.toml` adds `/opt/homebrew/lib` to the linker path automatically.

## Project Structure

Cargo workspace with 14 crates under `crates/`:

| Crate | Status | Purpose |
|-------|--------|---------|
| `fp-core` | Implemented | Shared types (`Vec2`, `Rect`, `SpriteId`, `FpError`) |
| `fp-formats` | Implemented | MUGEN file parsers (SFF v2 done; AIR, CNS, CMD, DEF, SND, FNT planned) |
| `fp-render` | Implemented | wgpu sprite renderer with palette lookup shader |
| `fp-app` | Implemented | SDL2 window, 60Hz game loop, entry point |
| `fp-vm` | Stub | Bytecode compiler + stack VM for expressions |
| `fp-input` | Stub | Input buffering + command recognition |
| `fp-physics` | Stub | Physics bodies + AABB collision |
| `fp-combat` | Stub | HitDef, damage, juggle, guard |
| `fp-character` | Stub | Character struct + state machine |
| `fp-stage` | Stub | Stage loading, backgrounds, camera |
| `fp-audio` | Stub | Sound mixer, BGM, SFX |
| `fp-ui` | Stub | Lifebars, menus, select screen |
| `fp-storyboard` | Stub | Cutscene system |
| `fp-engine` | Stub | Game coordinator + round flow |

## Code Conventions

### Rust Style
- **Edition 2021**, resolver v2
- `#![warn(missing_docs)]` on every crate — all public items need `///` doc comments
- Module-level `//!` docs in every `lib.rs` explaining the crate's role
- Use `thiserror` for error types; all errors are `FpError` variants, never `panic!`
- Use `tracing` for logging (`tracing::info!`, `tracing::warn!`), not `println!`
- Dependencies are declared at workspace level in root `Cargo.toml` and inherited via `.workspace = true`

### Error Philosophy: Never Crash on Bad Content
MUGEN community content is messy. Parsers must:
- Return `FpResult<T>` (never panic)
- Log warnings with `tracing::warn!` for recoverable issues
- Substitute safe defaults (missing sprite -> invisible, bad expression -> 0)
- Only return `Err` when loading truly cannot continue

### File Format Parsers (fp-formats)
- Use `nom` for binary parsing (little-endian: `le_u16`, `le_u32`, `le_i16`)
- Each format gets its own submodule under `src/` (e.g., `src/sff/`, `src/air.rs`)
- Parser functions return `FpResult<T>`, converting nom errors to `FpError::Parse`
- Include unit tests with synthetic binary data inline (no external test fixtures required)

### Rendering (fp-render)
- Sprites are 256-color indexed (`R8Unorm` textures)
- Palette lookup happens in WGSL shader (`shaders/palette.wgsl`)
- Palette index 0 = transparent (discarded in fragment shader)
- Orthographic projection: origin top-left, Y increases downward

### Game Loop
- Fixed 60Hz timestep (16.667ms per tick) — MUGEN runs at exactly 60 ticks/second
- `accumulator` pattern: accumulate elapsed time, drain in fixed-size ticks
- Rendering happens after update, outside the tick loop

### Testing
- Unit tests in each module via `#[cfg(test)] mod tests`
- Test binary parsers with synthetic byte arrays constructed inline
- `cargo test --workspace` and `cargo clippy --workspace` must both pass clean
- Doc tests on key public types with usage examples

## Architecture Notes

- **Struct-based entities** (not ECS) — MUGEN entities have fixed properties, VM needs direct field access
- **Bytecode VM** for expressions — compiled at load time, executed at runtime via stack interpreter
- **GPU palette lookup** — enables cheap palette swaps without texture re-upload
- Dependency graph: `fp-app` -> `fp-engine` -> `fp-character` -> `fp-vm`/`fp-combat`/`fp-physics`/`fp-input`; all depend on `fp-core`

## MUGEN Format Reference

Detailed format specs live in `docs/format-specs/`. Key formats:
- **SFF v2** (sprites): 512-byte header + 28-byte sprite sub-headers + 16-byte palette sub-headers + LData/TData blocks
- **AIR** (animations): Text-based, `[Begin Action N]` sections with frame entries `group, image, x, y, ticks, [flags]` and `Clsn` collision boxes
- **CNS** (states): Text-based, `[Statedef N]` + `[State N, label]` blocks with trigger expressions and state controllers
- **CMD** (commands): Text-based, `[Command]` blocks defining input sequences with timing windows
