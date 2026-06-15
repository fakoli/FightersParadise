<p align="center">
  <img src="assets/banner.png" alt="Fighters Paradise Banner" width="100%">
</p>

# Fighters Paradise

<p align="center">
  A clean-room reimplementation of the <a href="https://en.wikipedia.org/wiki/Mugen_(game_engine)">MUGEN</a> 2D fighting game engine in Rust — bring your own characters, in the original MUGEN content formats (.def, .sff, .air, .cmd, .cns, .snd).
</p>

<p align="center">
  <strong>Status: Playable.</strong> A two-character fighter driven by real Kung Fu Man data — CNS state machine, combat, throws, supers, best-of-3 rounds, and audio all run end to end. ~2,045 tests pass; <code>clippy -D warnings</code> is clean; CI is green.
</p>

The engine is real and the match is playable. The presentation layers now exist too — parallax stage backgrounds, a `fight.def` lifebar screenpack, on-screen text, and intro/ending storyboards are all wired in — but several are partial or asset-blocked (e.g. the screenpack falls back to a quad HUD without a `fight.def`, and there's no bundled Elecbyte stage/screenpack/font fixture yet). SFF v1 art now renders with its extracted palette, and modern PNG sprites decode. See [Known Issues](docs/known-issues.md) for the honest gap list.

## Quickstart

### Prerequisites

- **Rust** (edition 2021) — [install via rustup](https://rustup.rs/)
- **SDL2** — required for the window and keyboard input
  - macOS: `brew install sdl2`
  - Ubuntu/Debian: `apt install libsdl2-dev`
  - Windows: download from [libsdl.org](https://www.libsdl.org/download-2.0.php)

> **macOS note:** the linker needs Homebrew's libdir. This is handled automatically — [`.cargo/config.toml`](.cargo/config.toml) injects `rustflags = ["-L", "/opt/homebrew/lib"]` for `aarch64-apple-darwin`, so after `brew install sdl2` you do not need to set `RUSTFLAGS` by hand.

### Build, run, test, lint

Every operation is a plain `cargo` command. The [`Makefile`](Makefile) provides thin wrappers (run `make help` for the full self-documented list) — both columns below do the same thing:

| Task | make | cargo |
|------|------|-------|
| Build everything | `make build` | `cargo build --workspace` |
| Run (default match or test pattern) | `make run` | `cargo run -p fp-app` |
| Run all tests | `make test` | `cargo test --workspace` |
| Lint (deny warnings) | `make clippy` | `cargo clippy --workspace --all-targets -- -D warnings` |
| Local CI gate | `make ci` | `clippy` + `fmt --check` + `test` |

> The `make` targets are thin wrappers with no hidden build magic, so the `cargo` column is always the source of truth. For the long-running windowed game (start/stop/restart/status), use [`scripts/fp.sh`](scripts/fp.sh) — a Makefile cannot cleanly supervise a detached GUI process.

### Run a real KFM match

With Kung Fu Man content present at `test-assets/kfm/kfm.def` (see [Clean-room & content](#clean-room--content)), no arguments launches a two-KFM match:

```bash
make run-kfm                                   # explicit two-KFM match (errors if KFM absent)
cargo run -p fp-app                            # default two-KFM match (P1 = keyboard)
cargo run -p fp-app -- p1.def p2.def           # two characters, your choice
cargo run -p fp-app -- char.def                # same character on both sides
```

If no character is found, the app degrades to an on-screen test pattern rather than crashing.

### Run the sprite viewer / test pattern

```bash
make run-sprite SFF=char.sff AIR=char.air      # animation viewer (SFF + AIR)
cargo run -p fp-app -- char.sff char.air       # same, via cargo
cargo run -p fp-app -- char.sff                # static sprite viewer
cargo run -p fp-app                            # no KFM present → checkerboard test pattern
```

## Controls

Player 1 is keyboard-driven; Player 2 is an idle dummy in this milestone (no second-player input map or AI yet).

| Input | Keys |
|-------|------|
| Move (up / down / left / right) | `W` `S` `A` `D` or the arrow keys |
| Punches (a / b / c) | `U` / `I` / `O` |
| Kicks (x / y / z) | `J` / `K` / `L` |
| Quit | `Escape` |

## What works today

Driven by genuine Kung Fu Man content end to end:

- **CNS state machine** — every trigger and controller parameter is compiled to an expression at load time and executed by a per-tick, MUGEN-order executor (special states −3/−2/−1, then the current state). ~40 state controllers are dispatched (including `AssertSpecial`, `Width`, `SprPriority`, `Pause`/`SuperPause`, `PalFX`/`AfterImage`, `HitOverride`, and the get-hit-vel set); unimplemented ones fall to a logged no-op rather than crashing.
- **Combat** — `Clsn1`×`Clsn2` hit detection, a faithful Guard / Hit / Miss resolution ladder, mirrored knockback, and per-side damage application.
- **Throws** — `TargetState` / `TargetBind` / `TargetLifeAdd` / `TargetFacing` / `TargetVelSet` plus the attacker's `p1stateno`, applied via the engine's deferred target-op pass (KFM's signature throw works).
- **Supers & meter** — a power bar fed by `PowerAdd` / `TargetPowerAdd`, carried across rounds within a match.
- **Hitpause** — impact freeze on both fighters; while frozen, only `ignorehitpause` controllers run and anim/time/physics are held.
- **I-frames** — `NotHitBy` / `HitBy` invulnerability windows consulted before a hit is applied.
- **Hit reactions** — get-hit common states and `GetHitVar` (including `animtype`) populated from the connecting `HitDef`.
- **Jump / air-jump / land** — directional jump, a built-in double-jump, and a data-driven auto-land via the ground-plane clamp.
- **Best-of-3 rounds** — Intro → Fight → KO → Win flow, KO and time-over resolution, draws, and first-to-N round tracking.
- **Audio** — `PlaySnd` and `HitDef` impact sounds played through a channel-managed rodio mixer (with a headless null fallback).

Cross-entity triggers (`P2Dist`, `P2BodyDist`, edge distances, `p2`/`enemy`/`root` redirects) work via the engine's cross-entity eval context. See [MUGEN Compatibility](docs/mugen-compatibility.md) for the per-controller and per-trigger support matrix, and [Known Issues](docs/known-issues.md) for what is still missing.

## Architecture

Cargo workspace, edition 2021, 14 crates under `crates/`. No crate is a true stub anymore — `fp-stage`, `fp-ui`, and `fp-storyboard` have all graduated (some of their presentation features are still partial — see [Known Issues](docs/known-issues.md)).

```
fp-app (binary: SDL2 window, 60Hz loop, CLI + validate, HUD, audio routing, debug overlay)
  ├── fp-engine        two-player Match coordinator, round/best-of-N flow, freeze, hit-spark effects
  │     ├── fp-character  loader + Character entity + per-tick executor + cross-entity eval  ← largest crate
  │     │     ├── fp-vm      CNS expression parser + tree-walk evaluator (triggers, redirects)
  │     │     ├── fp-combat  HitDef model, Clsn hit primitive, resolve_hit/resolve_clash (depends on fp-physics)
  │     │     └── fp-input   60-frame ring buffer + MUGEN command recognition
  │     └── fp-physics    Euler integration, gravity, ground plane, push/bounds (also used by fp-combat)
  ├── fp-render        wgpu palette-lookup sprite renderer (256-color indexed) + PalFX tint + text/glyphs
  ├── fp-audio         rodio WAV decode + channel-managed playback
  ├── fp-formats       parsers: SFF (v1 PCX+palette, v2 RLE/LZ5/PNG), AIR, CMD, DEF, CNS, SND, FNT, ACT
  ├── fp-storyboard    storyboard .def parser + scene model + StoryboardPlayer (intro/ending overlay)
  ├── fp-stage         stage .def parser ([BGDef]/[BG]/[Camera]/[StageInfo]) + parallax render
  ├── fp-ui            fight.def screenpack model/parser + ScreenpackHud renderer (quad-HUD fallback)
  └── fp-core          shared types: Vec2, Rect, SpriteId, FpError/FpResult
```

| Crate | Status | Tests | Role |
|-------|--------|------:|------|
| `fp-character` | Implemented | 692 | Loader, `Character` entity, per-tick executor, cross-entity eval (the biggest crate) |
| `fp-vm` | Implemented | 491 | CNS expression lexer + Pratt parser + tree-walk evaluator (+ proptest fuzz) |
| `fp-formats` | Implemented | 182 | SFF v1 (+palette)/v2 (+PNG), AIR, CMD, DEF, CNS, SND, FNT, ACT parsers |
| `fp-engine` | Implemented | 121 | Two-player `Match`, round + best-of-N flow, freeze, hit-spark effects |
| `fp-input` | Implemented | 103 | Ring buffer + command recognition (`~ / $ > +`) |
| `fp-physics` | Implemented | 90 | Euler integration, gravity, ground plane, push/bounds |
| `fp-app` | Implemented | 89 | SDL2 window, 60Hz loop, CLI + `validate`, HUD, debug overlay, match wiring |
| `fp-combat` | Implemented | 84 | `HitDef` data model + `resolve_hit`/`resolve_clash` decision |
| `fp-storyboard` | Implemented | 63 | Storyboard `.def` parser + scene model + `StoryboardPlayer` (intro/ending overlay) |
| `fp-render` | Implemented | 46 | wgpu renderer, WGSL palette-lookup + PalFX-tint shader, text/glyphs |
| `fp-audio` | Implemented | 34 | rodio playback, channel cut-off, hardened WAV decode |
| `fp-core` | Implemented | 20 | Shared types (`Vec2`, `Rect`, `SpriteId`, `FpError`) |
| `fp-ui` | Implemented | 17 | `fight.def` screenpack model/parser + `ScreenpackHud` renderer (quad-HUD fallback) |
| `fp-stage` | Implemented | 13 | Stage `.def` parser + parallax background rendering |

> **Naming note:** `fp-vm` is named for a "bytecode VM," but the current implementation is a tree-walk evaluator over an AST — there is no bytecode or stack machine. The behavior (compile-at-load, evaluate-per-tick, never panic) matches the design intent. See [Architecture](docs/architecture.md).

### Design keystones

- **Fixed 60Hz tick** (16.667ms) with an accumulator loop; rendering happens once after the catch-up loop, outside the tick.
- **Struct-based entities** (not ECS), so the expression evaluator has direct field access.
- **CNS expressions compiled at load** and evaluated per tick; every error path resolves to `0` and never panics.
- **Cross-entity eval context** — a `Copy` `EvalEnv` threads the opponent/stage/anim through the executor so redirects (`p2`/`enemy`/`root`), `P2Dist`/`P2BodyDist`, and edge triggers work.
- **Deferred effects** — a tick cannot `&mut` another entity or the match, so it emits requests (sound requests, target ops, `Pause`/`SuperPause` freeze requests) into a `TickReport` that the `Match` applies; the `Match` also spawns hit-spark effect entities on a connecting hit.
- **Never crash on bad content** — parsers warn-and-skip malformed input and substitute safe defaults.

## Documentation

| Doc | What it covers |
|-----|----------------|
| [Architecture](docs/architecture.md) | Design overview, crate dependency graph, the keystone decisions |
| [MUGEN Compatibility](docs/mugen-compatibility.md) | Supported formats, controllers, and triggers — the compatibility matrix |
| [Content Guide](docs/content-guide.md) | How to structure characters and content for the engine |
| [Known Issues](docs/known-issues.md) | The honest gap list (stage backgrounds, screenpack, text, SFF v1 palettes, …) |
| [Roadmap](docs/roadmap.md) | What's planned next and why |
| [Development](docs/development.md) | Build/test/lint workflow, conventions, contributing details |
| [Knowledge Base](docs/knowledge-base/) | Research + planning: MUGEN overview, ecosystem, evaluator semantics, faithfulness audit |
| [Format Specs](docs/format-specs/) | Binary/text format references (e.g. [SFF v2](docs/format-specs/sff-v2.md)) |
| [CONTRIBUTING](CONTRIBUTING.md) · [CHANGELOG](CHANGELOG.md) | Contributor guide and change log |

## MUGEN compatibility

The goal is a **completely customizable** fighting-game engine: bring your own characters in the original MUGEN formats. All six core formats parse real content today — SFF (v1 inline-PCX with trailing-palette extraction, and v2 with RLE8/RLE5/LZ5 **plus PNG8/24/32 decode**), AIR (incl. scale/angle/Interpolate), CMD, DEF, CNS, and SND. The bitmap-font (**FNT v1**) and palette (**ACT**) formats now parse as well.

Remaining gaps are mostly about *consuming* what parses rather than parsing it: there's no bundled Elecbyte stage/screenpack/font fixture, ACT palettes aren't yet applied at runtime, and team/turns/tag modes are unimplemented. For the full picture of what loads and runs, see [MUGEN Compatibility](docs/mugen-compatibility.md); for guidance on authoring or porting characters, see the [Content Guide](docs/content-guide.md).

## Contributing

Contributions are welcome. Before opening a PR, both of these must pass clean:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

See [CONTRIBUTING](CONTRIBUTING.md) and [Development](docs/development.md) for conventions. Note that the real-*KFM* tests are **asset-gated**: without local KFM content under `test-assets/`, they skip cleanly (which is why CI's KFM-specific tests run as no-ops — see [Known Issues](docs/known-issues.md)). CI is no longer blind to real content, though: it loads, matches, and runs the `validate` CLI against the shipped original `assets/trainingdummy` character on every push.

## Clean-room & content

Fighters Paradise is a **clean-room** project: **no Elecbyte/MUGEN engine source or copyrighted assets are shipped or tracked.** The only tracked content is the project's own **original** assets — `assets/banner.png` and the from-scratch MIT conformance character under `assets/trainingdummy/` (the shippable default and CI fixture). Kung Fu Man (CC BY-NC 3.0, by Elecbyte) is used **locally only** for testing — `test-assets/` is a gitignored symlink, and no third-party asset files are committed. Please do not commit any third-party content.

## License

This project's code is licensed under the MIT License. See [LICENSE](LICENSE) for details.

Fighters Paradise is an independent project. MUGEN is a trademark of Elecbyte. This project does not include any Elecbyte code or assets.
