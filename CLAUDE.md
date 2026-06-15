# Fighters Paradise

A clean-room reimplementation of the [MUGEN](https://en.wikipedia.org/wiki/Mugen_(game_engine)) 2D
fighting engine in Rust, aiming for a **completely customizable fighting-game engine**: bring your own
characters in MUGEN format (`.sff`, `.air`, `.cns`, `.cmd`, `.def`, `.snd`).

> **Current state (not v0.1.0 stubs):** this is a **playable two-character fighter**. `fp-app` renders a
> two-character `fp_engine::Match` (P1 = keyboard), a life HUD, KO/winner readout, and best-of-3 rounds,
> driven by **real Kung Fu Man data**. KFM's signature throw, supers (meter), hitpause, i-frames, hit
> reactions, jump/airjump/land, and damage multipliers all work end to end. Only `fp-stage` and `fp-ui`
> are still true stubs. See [docs/known-issues.md](docs/known-issues.md) and [docs/roadmap.md](docs/roadmap.md)
> for what's missing.

## Build & Run

```bash
cargo build --workspace                              # Build everything
cargo run -p fp-app                                  # Default: two-KFM match (test-assets/kfm/kfm.def)
cargo run -p fp-app -- p1.def [p2.def]               # Two-character match (one .def = same char both sides)
cargo run -p fp-app -- file.sff [file.air]           # Legacy sprite/animation viewer
cargo test --workspace                               # Run all tests (~1769 pass incl. doc tests)
cargo clippy --workspace --all-targets -- -D warnings  # Lint — must be clean
```

- **macOS prerequisite:** `brew install sdl2`. SDL2 is a hard native dep (`sdl2 0.37`). `.cargo/config.toml`
  injects `rustflags = ["-L", "/opt/homebrew/lib"]` for `aarch64-apple-darwin`, so no manual flags are
  needed locally. If you must set it by hand: `RUSTFLAGS="-L /opt/homebrew/lib"`.
- **Linux prerequisite:** `apt install libsdl2-dev`.
- No args runs the default two-KFM match from `DEFAULT_DEF = "test-assets/kfm/kfm.def"` (main.rs:71),
  degrading to a checkerboard test pattern if KFM is absent. Bad/missing assets never panic.
- A convenience Makefile (thin `cargo` wrappers: `make build`/`run`/`run-kfm`/`run-sprite`/`test`/`test-fast`/
  `check`/`clippy`/`fmt`/`fmt-check`/`doc`/`clean`/`ci`) and `scripts/fp.sh` (windowed-game start/stop/
  restart/status supervisor) exist; there is **no** justfile or xtask. Every `make` target is a plain `cargo`
  command, so anything in the Makefile can also be run by hand.
  Note: CI does **not** gate on `cargo fmt --check` (the `ci` workflow runs clippy + tests; the codebase
  isn't rustfmt-clean — backlog CB3). The Makefile's `fmt-check` target exists but is not wired into CI.

## Project Structure

Cargo workspace, 14 crates under `crates/` (edition 2021, resolver v2, MIT). Deps are declared once in the
root `[workspace.dependencies]` and inherited via `.workspace = true`. **Only `fp-stage` and `fp-ui` are
stubs** (7-line module-doc files); everything else is implemented and tested.

| Crate | Tests | Status | Purpose |
|-------|------:|--------|---------|
| `fp-character` | 620 | Implemented | Loader (`.def`→`LoadedCharacter`) + live `Character` entity + per-tick MUGEN-order executor (~30 state controllers) + cross-entity `EvalContext`. **Largest crate.** |
| `fp-vm` | 463 | Implemented | CNS expression engine: lexer → Pratt parser → **tree-walk evaluator** (NOT a bytecode/stack VM despite the name). Triggers, redirects, `Value` model. |
| `fp-formats` | 142 | Implemented | Parsers: SFF v1 (PCX) + SFF v2 (RLE8/RLE5/LZ5/raw), AIR, CMD, DEF, CNS, SND. **PNG decode, SFF v1 palette, FNT/ACT pending.** |
| `fp-input` | 102 | Implemented | 60-frame ring buffer + command recognition (incl. `~` `/` `$` `>` `+` symbols). |
| `fp-engine` | 96 | Implemented | Two-player `Match` coordinator: 6-step tick, round/best-of-N flow, push/bounds, deferred-op application. |
| `fp-physics` | 79 | Implemented | Euler integration, gravity (0.44), Y=0 ground plane, AABB `Clsn` overlap, player push/bounds. No friction (that's `fp-character`). |
| `fp-combat` | 60 | Implemented | `HitDef` data model, `Clsn1`×`Clsn2` hit primitive, pure `resolve_hit` → `HitOutcome`, `GetHitVars`. Pure data/geometry/decision leaf. |
| `fp-app` | 49 | Implemented | SDL2 window, 60Hz accumulator loop, CLI args, hand-rolled HUD, two-player match wiring, audio routing. |
| `fp-storyboard` | 44 | Parser only | Storyboard `.def` parser + typed scene model. **No tick/render, no consumer.** |
| `fp-audio` | 32 | Implemented | rodio WAV decode + channel-managed playback (`PlaySnd` + HitDef impact sounds); `NullBackend` headless fallback. |
| `fp-render` | 22 | Implemented | wgpu sprite renderer, WGSL palette-lookup shader, 256-color indexed (palette idx 0 = transparent). |
| `fp-core` | 15 | Implemented | Shared types: `Vec2`, `Rect`, `SpriteId`, `FpError`/`FpResult`. |
| `fp-stage` | 0 | **Stub** | Empty — no `[BGDef]`/`[BG]`/`[Camera]` parser; matches render over a flat clear color. |
| `fp-ui` | 0 | **Stub** | Empty — HUD is hand-rolled solid quads in `fp-app`, not a `fight.def`/`fight.sff` screenpack. |

~59,000 LOC across 14 crates. Dependency graph: `fp-app` → `fp-engine` → `fp-character` → `fp-vm` /
`fp-combat` / `fp-physics` / `fp-input`; `fp-combat` depends only on `fp-core` + `fp-physics`; everything
depends on `fp-core`.

## Architecture Notes

These are the keystones a new session (or maintainer) must understand first.

- **Fixed 60Hz tick** (`TICK_DURATION = 16_666_667 ns`, main.rs:68). Accumulator pattern: accumulate
  elapsed time, drain in fixed steps, render **once after** the catch-up loop (outside it).
- **Struct-based entities, not ECS.** MUGEN entities have fixed fields and the evaluator needs direct
  field access, so `Character` is a plain struct.
- **CNS → VM compile + execute.** Every trigger and every controller parameter is compiled to an
  `fp_vm::Expr` AST **at load time** (`CompiledExpr::compile`, loader.rs:92); a parse failure becomes a
  const-0 fallback with a warning, never a panic. Per tick the executor evaluates those expressions.
  Note: the crate is named "VM" and its docs say "bytecode + stack VM," but the implementation is a
  **tree-walk evaluator** over the AST — there are no opcodes or a stack machine.
- **Cross-entity eval context (the keystone that unblocked redirects / P2Dist / edges).**
  `EvalEnv { opponent: Option<&EvalCtx>, stage: StageView, anim: AnimSet }` is `Copy` and threaded through
  the whole executor dispatch. At each eval site the executor reborrows `&*self` into a short-lived
  `EvalCtx { me, opponent, stage, anim }` (executor.rs:810). A bare `Character` is a **self-only**
  `EvalContext` (`redirect()` always `None`); the `EvalCtx` wrapper resolves `p2`/`enemy`/`enemynear`→opponent
  and `root`→self, and computes `P2Dist`/`P2BodyDist`/edge distances/`ScreenPos`/`SelfAnimExist`/`P2Life`.
  `parent`/`helper`/`target`/`partner`/`playerid` resolve to `None` (no helper graph / teams yet).
- **Deferred-effects pattern (`TickReport`).** A tick cannot `&mut` another entity, so a character emits
  requests — `sound_requests` (`PlaySnd`) and `target_ops` (`Target*` throw controllers) — into a
  `TickReport`, and `fp-engine`'s `Match` applies them after both characters tick. Same idea for
  cross-entity *reads* via `Option<&dyn EvalContext>`.
- **Executor dispatch** is one `eq_ignore_ascii_case` if/else chain (executor.rs:900-981). ~30 controllers
  are handled; **any unhandled controller falls to a debug-logged safe no-op** (this is how missing
  features like `Width`, `AssertSpecial`, `SprPriority`, `Pause`/`SuperPause`, `AfterImage`/`PalFX`,
  get-hit vel controllers currently behave).
- **Hit application lives in `fp-character`, not `fp-combat`/`fp-engine`.** `fp-combat` is the pure
  data + geometry + `resolve_hit` decision; the detect→resolve→**apply** bridge (`GetHitVars`, hitpause,
  damage multipliers, knockback, invuln gating) is `fp_character::combat::resolve_attack` (combat.rs).
  `fp-engine` coordinates: it calls `resolve_attack`, applies `p1stateno`/`Target*` ops, and runs round flow.
- **GPU palette lookup.** Sprites are `R8Unorm` index textures + a 256×1 RGBA palette texture; the WGSL
  shader discards index 0 and looks up the rest — cheap palette swaps without re-upload.

## Where to Look (navigation map)

| Subsystem | Key file |
|-----------|----------|
| Character loader + CNS merge + const reads | `crates/fp-character/src/loader.rs` |
| Per-tick executor, dispatch, `EvalEnv`, `TickReport` | `crates/fp-character/src/executor.rs` |
| `Character` entity, `EvalContext`/`EvalCtx`, triggers, `resolve_const` | `crates/fp-character/src/lib.rs` |
| Hit application (`resolve_attack`, GetHitVars, multipliers) | `crates/fp-character/src/combat.rs` |
| NotHitBy/HitBy i-frame windows | `crates/fp-character/src/invuln.rs` |
| Two-player `Match`, tick order, round flow, `apply_target_ops` | `crates/fp-engine/src/lib.rs` |
| `HitDef`, `detect_hit`/`detect_hit_contact`, pure `resolve_hit` | `crates/fp-combat/src/lib.rs` |
| Expression lexer / parser / evaluator | `crates/fp-vm/src/lexer.rs` · `parser.rs` · `evaluator.rs` |
| `Value` model + `EvalContext` trait (the seam) | `crates/fp-vm/src/eval.rs` |
| SFF orchestration + version detect + decode dispatch | `crates/fp-formats/src/sff/mod.rs` |
| SFF codecs (RLE8/RLE5/LZ5 + PNG stub) | `crates/fp-formats/src/sff/compression.rs` |
| SFF v1 PCX container (note: no palette extraction) | `crates/fp-formats/src/sff/v1.rs` |
| CNS parser (largest text parser) | `crates/fp-formats/src/cns.rs` |
| AIR / CMD / DEF / SND parsers | `crates/fp-formats/src/{air,cmd,def,snd}.rs` |
| Command compile + backward-scan matcher | `crates/fp-input/src/command.rs` |
| Input ring buffer | `crates/fp-input/src/buffer.rs` |
| Physics integration + ground plane | `crates/fp-physics/src/lib.rs` |
| AABB collision + `place_clsn` facing mirror | `crates/fp-physics/src/collision.rs` |
| wgpu renderer + 3 blend pipelines | `crates/fp-render/src/renderer.rs` |
| Palette-lookup fragment shader | `crates/fp-render/src/shaders/palette.wgsl` |
| Audio backend + cut-off policy | `crates/fp-audio/src/system.rs` |
| App: CLI modes, 60Hz loop, HUD, audio routing | `crates/fp-app/src/main.rs` |

## Code Conventions

### Rust Style
- **Edition 2021**, resolver v2.
- `#![warn(missing_docs)]` on every library crate (all crates except the `fp-app` binary) — all public
  items need `///` docs; every `lib.rs` carries a module-level `//!` doc explaining the crate's role.
- Errors are `FpError` variants (`thiserror`), never `panic!`. Public APIs return `FpResult<T>`.
- Use `tracing` (`tracing::info!`, `tracing::warn!`), not `println!`.
- Workspace-level dependencies inherited via `.workspace = true`.

### Error Philosophy: Never Crash on Bad Content
MUGEN community content is messy. Parsers and the evaluator must:
- Return `FpResult<T>` / a recoverable error, never panic.
- `tracing::warn!` and skip recoverable issues; bound linked-list walks against cycles; reject oversized
  allocations before allocating.
- Substitute safe defaults: missing sprite → invisible, bad/unknown expression → `0`, unresolved
  redirect → `0`, div/mod-by-zero → `0`. Only return `Err` when loading truly cannot continue.

### File Format Parsers (fp-formats)
- Binary (SFF): little-endian; `mod.rs` orchestrates, `compression.rs`/`v1.rs` do the heavy lifting.
- Text (AIR/CMD/DEF/CNS/SND): line-oriented, case-insensitive, BOM/CRLF-tolerant, `;`/`//`/`#` comments
  stripped. CNS splits only on the **first `=`** so trigger expressions survive verbatim.
- Each format gets its own submodule; functions return `FpResult<T>`.
- Real-content tests run against the genuine KFM fixture and **skip cleanly when `test-assets/` is absent**.

### Rendering (fp-render)
- Sprites are 256-color indexed (`R8Unorm`); palette lookup happens in WGSL (`shaders/palette.wgsl`).
- Palette index 0 = transparent (discarded in the fragment shader).
- Orthographic projection: origin top-left, Y increases downward.

### Game Loop
- Fixed 60Hz timestep (16.667ms/tick); `accumulator` pattern; render after update, outside the tick loop.
- Known gap (#27): input is currently sampled *inside* the catch-up loop, so a multi-tick frame re-reads
  the same keyboard state.

### Testing
- Unit tests per module via `#[cfg(test)] mod tests`; binary parsers tested with synthetic byte arrays.
- `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must both pass clean.
- **CI caveat (#36):** real-content tests are asset-gated and `test-assets/` is gitignored with no CI fetch
  step, so the real-KFM regression net runs as **green no-ops on CI** — only synthetic tests truly execute
  there. Run the suite locally with KFM linked to actually exercise real content.

## Dev Loop, Worktrees & Clean-Room

- **Worktree discipline:** operate **only** in this worktree (`fp-work`). Other Claude instances and the
  shared checkout live at `.../FightersParadise`; never edit that directly. (`.git` here is
  `gitdir: .../FightersParadise/.git/worktrees/fp-work`.)
- **Clean-room contract (must stay true):** no Elecbyte/MUGEN engine source or copyrighted assets are
  shipped or tracked. The **only** tracked content/binaries are the project's own **original** assets:
  `assets/banner.png` and the original conformance character under `assets/trainingdummy/`
  (`*.def/.cns/.cmd/.air/.sff/.snd` — MIT, authored from scratch as the shippable default + CI fixture;
  the only ASCII strings in its `.sff`/`.snd` are the required `ElecbyteSpr\0`/`ElecbyteSnd\0` format
  magic, not copyrighted data). `*.sff`/`*.snd` stay gitignored globally except those `assets/trainingdummy`
  paths. Beyond these originals, `git ls-files` shows zero `.sff/.air/.cmd/.cns/.def/.snd/.fnt/.pcx/.act`
  files. Real KFM content (CC BY-NC 3.0, Elecbyte) is **local-only** behind the gitignored `test-assets`
  symlink. Never commit or ship it.
- Fighters Paradise is an independent project; MUGEN is a trademark of Elecbyte. Code is MIT
  (© 2025 Sekou Doumbouya); see [LICENSE](LICENSE).
- Commit/push only when asked; if on the default branch, branch first.

## Documentation

Authoritative project docs live in `docs/`; research/planning docs in `docs/knowledge-base/`.

- [docs/architecture.md](docs/architecture.md) — design overview, dependency graph, decisions.
- [docs/mugen-compatibility.md](docs/mugen-compatibility.md) — what MUGEN features/formats are supported.
- [docs/content-guide.md](docs/content-guide.md) — how to structure bring-your-own-character content.
- [docs/known-issues.md](docs/known-issues.md) — ranked fidelity gaps (mirrors the faithfulness audit).
- [docs/roadmap.md](docs/roadmap.md) — what's planned next.
- [docs/development.md](docs/development.md) — build/test/lint, worktree + clean-room rules.
- [docs/handoffs/](docs/handoffs/) — dated session-handoff notes; the **newest is the current pickup
  point** (see "Session handoffs" below).
- [docs/format-specs/sff-v2.md](docs/format-specs/sff-v2.md) — SFF v2 binary layout.
- [docs/knowledge-base/](docs/knowledge-base/) — MUGEN research (01-03), roadmap (05), execution plan
  (06, live ledger), evaluator semantics (07), faithfulness audit (08). Note: doc 04 (codebase review)
  and the top-level CHANGELOG are stale; trust the source over them.
- Root [README.md](README.md) for the public-facing overview.

> macOS-filesystem caveat: the filesystem here is **case-insensitive**, so do **not** create
> `docs/ARCHITECTURE.md` alongside the existing `docs/architecture.md` (they are the same file). New docs
> belong under `docs/`, not at the repo root.

## Session handoffs

Cross-session / cross-workstation continuity lives in [`docs/handoffs/`](docs/handoffs/) — dated,
**version-controlled** notes named `YYYY-MM-DD-<slug>.md`. They are committed on purpose so any
workstation (or a fresh session with no memory of the conversation) can resume from where the last one
stopped.

- **Starting a session:** read the **most recent** file in `docs/handoffs/` first. It carries the
  current `main`/PR state, what the last session changed, where the audit "P" fixes stand, and the
  recommended next actions — orient from it before doing anything else.
- **Ending a substantial session, or whenever asked for a handoff:** write a **new** dated file
  capturing (1) repo state — `main` tip, active branch, open PRs (verify with `git`/`gh`, don't guess);
  (2) what this session changed; (3) where work stopped + the next concrete steps; (4) invariants/gotchas
  to not relearn. Make it **self-contained** — a cold session must be able to act on it alone. Commit it
  (typically onto the active branch/PR).
- Newest = source of truth. Keep them factual and skimmable; don't delete older ones — they are the
  project's running history.
