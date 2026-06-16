# Fighters Paradise

A clean-room reimplementation of the [MUGEN](https://en.wikipedia.org/wiki/Mugen_(game_engine)) 2D
fighting engine in Rust, aiming for a **completely customizable fighting-game engine**: bring your own
characters in MUGEN format (`.sff`, `.air`, `.cns`, `.cmd`, `.def`, `.snd`).

> **Current state (2026-06-15 — v1.0):** a **complete, playable fighting game**, not just an engine. From
> the **Title screen** you flow through **character select → stage select → fight**, plus a **Setup/Options
> screen with live key remapping** and **HUD / life-power-bar customization** — all navigable by keyboard
> or game controller. Bring-your-own content is **discovered from directories the MUGEN way**: point
> `fp-app` at a game root (or a `chars/` folder directly) and the roster auto-populates from
> `chars/<name>/<name>.def`; stages come from `stages/`, and selectable motif/screenpack sets from
> `data/<motif>/` (the `--motif` flag). `fp-app` renders a two-character `fp_engine::Match`
> (P1 = keyboard/gamepad, P2 = baseline CPU AI or human) over a full-color stage, with a life/power HUD,
> combo counter + portraits, KO/winner readout, and best-of-3 rounds; team **Simul/Turns** are reachable
> via `--simul`/`--turns`.
>
> The engine — throws, supers (meter), hitpause, i-frames, hit reactions, jump/airjump/land, damage
> multipliers, helpers + projectiles + the **Explod** subsystem, `target`/`parent`/`root`/`helper`/
> `partner`/`playerid` redirects, full PalFX + true AfterImage, per-frame AIR scale/angle/Interpolate, the
> `:=` assignment operator, deterministic serialization + record/replay, and broad **community-content
> robustness** (Shift-JIS CNS/CMD + `.def`, SFF v2 sub-header resilience, FNT v2 detect, `FU`/`BU` command
> tokens, helper lifecycle/`DestroySelf`) — is implemented and unit-tested (**~2621 tests**; clippy
> `-D warnings` + `cargo fmt --check` are CI gates). **No crate is a stub.** SFF v1 (evilken) and SFF v2
> (KFM) both render in full color.
>
> The 1.0 build landed across **47 fakoli-state tasks** (PRs #44–#94) on 2026-06-15; the newest
> `docs/handoffs/` file is the per-run summary and the remaining non-blocking follow-ups (e.g. `stages/`
> auto-discovery under a game-root arg, bare no-id `Proj*` triggers, `.snd` ADX decode). See
> [docs/known-issues.md](docs/known-issues.md) and [docs/roadmap.md](docs/roadmap.md).

## Build & Run

```bash
cargo build --workspace                              # Build everything
cargo run -p fp-app                                  # No args: boots the Title menu over the shipped clean-room motif
cargo run -p fp-app -- p1.def [p2.def]               # Direct match (one .def = same char both sides)
cargo run -p fp-app -- <dir>                          # Discover a roster from a game root or chars/ dir (F020) -> menu
cargo run -p fp-app -- --motif <name|path> [args]    # Pick a discovered/explicit motif (T045; falls back to default)
cargo run -p fp-app -- --simul p1.def [p2.def]       # Team Simul (or --turns) direct match (T027/T028)
cargo run -p fp-app -- file.sff [file.air]           # Legacy sprite/animation viewer
cargo test --workspace                               # Run all tests (~2621 pass)
cargo run -p fp-app -- validate char.def             # Character validator (lints a .def's assets/states)
cargo clippy --workspace --all-targets -- -D warnings  # Lint — must be clean
```

- **macOS prerequisite:** `brew install sdl2`. SDL2 is a hard native dep (`sdl2 0.37`). `.cargo/config.toml`
  injects `rustflags = ["-L", "/opt/homebrew/lib"]` for `aarch64-apple-darwin`, so no manual flags are
  needed locally. If you must set it by hand: `RUSTFLAGS="-L /opt/homebrew/lib"`.
- **Linux prerequisite:** `apt install libsdl2-dev`.
- **No args** boots the in-app **Title menu** (`cli_route` → `CliRoute::Menu`; main.rs ~2338) over the
  shipped clean-room `assets/data/{system,select}.def` motif. Passing a **`.def` boots a direct match**
  (`cargo run -p fp-app -- <char.def>` — same char both sides; `CliRoute::Direct`), bypassing the menu —
  this is the fast path for rendering/visual work. Bad/missing assets never panic (test-pattern fallback).
- A convenience Makefile (thin `cargo` wrappers: `make build`/`run`/`run-kfm`/`run-sprite`/`test`/`test-fast`/
  `check`/`clippy`/`fmt`/`fmt-check`/`doc`/`clean`/`ci`) and `scripts/fp.sh` (windowed-game start/stop/
  restart/status supervisor) exist; there is **no** justfile or xtask. Every `make` target is a plain `cargo`
  command, so anything in the Makefile can also be run by hand.
  Note: CI **now gates on `cargo fmt --all --check`** (backlog CB3, done) — the workspace was run through
  `cargo fmt --all` once and is rustfmt-clean, so keep it that way (run `make fmt` before committing). The
  `ci` workflow runs fmt-check + clippy + tests; the Makefile's `fmt-check` target and `make ci` mirror it.

### Live-debugging the windowed app (how to actually SEE what it renders)

Rendering bugs are invisible to `cargo test` — `fp-render` has no headless GPU readback, so the suite can
pass while the screen is blank (that is exactly how the fighters-don't-render bug survived). To verify
visual behaviour you must look at the real window. The proven workflow (macOS, used for #40/#41):

1. **Launch a direct match** so you skip the menu and it renders immediately:
   `RUST_LOG=error ./target/debug/fp-app test-assets/evilken/evilken.def &` (build first; `evilken` is
   SFF v1 and renders in full color — use it, not KFM, until task #32 lands).
2. **You cannot use computer-use screenshots on this window.** `fp-app` is a bare `cargo` binary, so
   `request_access` returns `not_installed` and computer-use's compositor filter hides it. Instead:
   - Window geometry: `osascript -e 'tell application "System Events" to tell process "fp-app" to get
     {position, size} of window 1'` → `x, y, w, h`.
   - Capture it: **`screencapture -x -R x,y,w,h /tmp/s.png`** (macOS's own tool ignores the filter), then
     `Read` the PNG. Needs **Screen Recording** granted to Claude (System Settings → Privacy & Security;
     takes effect after a relaunch).
3. **Instrument the draw path** with an env-gated `eprintln!` (e.g. `FP_DBG`) in `draw_player` /
   `get_or_create_sprite` / `SffFile::palette` when a sprite is missing/black — that is how the
   vertex-overwrite and the KFM v2 palette bugs were pinned. Revert the instrumentation before committing.
4. **Generate image assets with nanobanana** (backgrounds, etc.):
   `~/.claude/plugins/marketplaces/fakoli-plugins/plugins/nano-banana-pro/.venv/bin/python
   .../skills/generate/scripts/nanobanana.py gen --prompt "..." --out x.png --aspect 4:3 --size 2K`
   (subcommand is **`gen`**, not `generate`; `GEMINI_API_KEY` is in env; output is JPEG-in-`.png`, so run
   `sips -s format png ...` to get a real PNG the `png` crate can decode).

## Project Structure

Cargo workspace, 14 crates under `crates/` (edition 2021, resolver v2, MIT). Deps are declared once in the
root `[workspace.dependencies]` and inherited via `.workspace = true`. **No crate is a stub anymore** —
`fp-stage`, `fp-ui`, and `fp-storyboard` have all graduated; everything is implemented and tested (a few
presentation features are partial/asset-blocked — see "Architecture Notes" and the doc table).

| Crate | Tests | Status | Purpose |
|-------|------:|--------|---------|
| `fp-character` | 692 | Implemented | Loader (`.def`→`LoadedCharacter`) + live `Character` entity + per-tick MUGEN-order executor (~40 state controllers) + cross-entity `EvalContext`. **Largest crate.** |
| `fp-vm` | 491 | Implemented | CNS expression engine: lexer → Pratt parser → **tree-walk evaluator** (NOT a bytecode/stack VM despite the name). Triggers, redirects, `Value` model, proptest fuzz, arith NaN→Bottom. |
| `fp-formats` | 182 | Implemented | Parsers: SFF v1 (PCX **+ trailing-palette extraction**) + SFF v2 (RLE8/RLE5/LZ5/raw **+ PNG8/24/32 decode**), AIR (incl. scale/angle/Interpolate), CMD, DEF, CNS, SND, **FNT v1, ACT palette**. |
| `fp-input` | 103 | Implemented | 60-frame ring buffer + command recognition (incl. `~` `/` `$` `>` `+` symbols). |
| `fp-engine` | 121 | Implemented | Two-player `Match` coordinator: 6-step tick, round/best-of-N flow, push/bounds, deferred-op application, clash/trade resolution, Pause/SuperPause freeze, hit-spark effect entities. |
| `fp-physics` | 90 | Implemented | Euler integration, gravity (0.44), Y=0 ground plane, AABB `Clsn` overlap, player push/bounds. No friction (that's `fp-character`). |
| `fp-combat` | 84 | Implemented | `HitDef` data model, `Clsn1`×`Clsn2` hit primitive, pure `resolve_hit` → `HitOutcome`, `resolve_clash`, `GetHitVars`. Pure data/geometry/decision leaf. |
| `fp-app` | 89 | Implemented | SDL2 window, 60Hz accumulator loop, CLI args (incl. `validate`), hand-rolled HUD, two-player match wiring, audio routing, Clsn debug overlay (F1). |
| `fp-storyboard` | 65 | Implemented | Storyboard `.def` parser + typed scene model + `StoryboardPlayer`; driven as an intro/ending overlay by `fp-app`. Per-scene `clearcolor` + `fadein`/`fadeout` (alpha ramp) are computed by the player and applied by `fp-app`; per-scene `bgm` transitions are computed/logged (no music-streaming backend yet). |
| `fp-audio` | 34 | Implemented | rodio WAV decode + channel-managed playback (`PlaySnd` + HitDef impact sounds); `NullBackend` headless fallback. |
| `fp-render` | 46 | Implemented | wgpu sprite renderer, WGSL palette-lookup shader, 256-color indexed (palette idx 0 = transparent), PalFX color-tint uniform, debug-box + `draw_text`/glyph primitives. |
| `fp-core` | 20 | Implemented | Shared types: `Vec2`, `Rect`, `SpriteId`, `FpError`/`FpResult`. |
| `fp-stage` | 13 | Implemented | Typed `[BGDef]`/`[BG]`/`[Camera]`/`[StageInfo]` parser + parallax camera render in `fp-app`. (Tile/velocity/mask/`type=anim` and vertical-follow parsed-not-rendered; no real stage fixture.) |
| `fp-ui` | 17 | Implemented | Typed `fight.def` model + parser + `ScreenpackHud` renderer; `fp-app` loads it (falls back to the quad HUD when absent). ([Combo] parsed-not-drawn; single bg layer; no real fixture.) |

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
- **Deferred-effects pattern (`TickReport`).** A tick cannot `&mut` another entity (or the match itself),
  so a character emits requests — `sound_requests` (`PlaySnd`), `target_ops` (`Target*` throw controllers),
  and `freeze_request` (`Pause`/`SuperPause`, a `FreezeRequest`) — into a `TickReport`, and `fp-engine`'s
  `Match` applies them after both characters tick (it also spawns hit-spark `Effect` entities on a
  connecting hit). Same idea for cross-entity *reads* via `Option<&dyn EvalContext>`.
- **Executor dispatch** is one `eq_ignore_ascii_case` if/else chain. ~40 controllers are now handled —
  including `AssertSpecial`, `Width`, `SprPriority`, `Pause`/`SuperPause`, `PalFX`/`AfterImage`/
  `AfterImageTime`, `HitOverride`, and the get-hit-vel set (`HitVelSet`/`HitFallSet`/`HitFallVel`/
  `HitFallDamage`), all of which used to fall through to the no-op. **Any still-unhandled controller falls
  to a debug-logged safe no-op.**
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
| Two-player `Match`, tick order, round flow, `apply_target_ops`, freeze, `Effect` spawns | `crates/fp-engine/src/lib.rs` |
| `HitDef`, `detect_hit`/`detect_hit_contact`, pure `resolve_hit`, `resolve_clash` | `crates/fp-combat/src/lib.rs` |
| Expression lexer / parser / evaluator | `crates/fp-vm/src/lexer.rs` · `parser.rs` · `evaluator.rs` |
| `Value` model + `EvalContext` trait (the seam) | `crates/fp-vm/src/eval.rs` |
| SFF orchestration + version detect + decode dispatch (`decode_sprite_rgba`) | `crates/fp-formats/src/sff/mod.rs` |
| SFF codecs (RLE8/RLE5/LZ5 + PNG8/24/32 decode) | `crates/fp-formats/src/sff/compression.rs` |
| SFF v1 PCX container + trailing-palette extraction | `crates/fp-formats/src/sff/v1.rs` |
| **SFF palette resolution (`palette()`) + v2 palette sub-headers** — the KFM v2 silhouette bug lives here (task #32: v2 palettes read RGBA, `palette()` only does v1 RGB→RGBA; small per-sprite palettes also point at a wrong LData offset) | `crates/fp-formats/src/sff/{mod.rs,palette.rs}` |
| CNS parser (largest text parser) | `crates/fp-formats/src/cns.rs` |
| AIR / CMD / DEF / SND parsers | `crates/fp-formats/src/{air,cmd,def,snd}.rs` |
| FNT v1 bitmap-font parser + ACT palette parser | `crates/fp-formats/src/{fnt,act}.rs` |
| Command compile + backward-scan matcher | `crates/fp-input/src/command.rs` |
| Input ring buffer | `crates/fp-input/src/buffer.rs` |
| Physics integration + ground plane | `crates/fp-physics/src/lib.rs` |
| AABB collision + `place_clsn` facing mirror | `crates/fp-physics/src/collision.rs` |
| wgpu renderer + blend pipelines + **per-quad bump-allocated vertex buffer** (`draw_textured_quad`, `MAX_SPRITE_QUADS`; the #40 fix) + **RGBA image path** (`ImageTexture`, `pipeline_image`, `draw_image` — the #41 stage-background path) | `crates/fp-render/src/renderer.rs` |
| Palette-lookup + PalFX-tint fragment shader; RGBA image shader (`image.wgsl`) | `crates/fp-render/src/shaders/{palette,image}.wgsl` |
| Text / glyph rendering (`draw_text`) | `crates/fp-render/src/text.rs` |
| Audio backend + cut-off policy | `crates/fp-audio/src/system.rs` |
| Stage `[BGDef]`/`[BG]`/`[Camera]` parser + parallax model | `crates/fp-stage/src/lib.rs` |
| `fight.def` screenpack model/parser + `ScreenpackHud` renderer | `crates/fp-ui/src/{screenpack,renderer}.rs` |
| Storyboard `StoryboardPlayer` (intro/ending playback) | `crates/fp-storyboard/src/player.rs` |
| App: CLI modes, 60Hz loop, HUD, audio routing | `crates/fp-app/src/main.rs` |
| Character validator CLI (`validate` subcommand) | `crates/fp-app/src/validate.rs` |

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
- Keyboard is sampled **once per frame** (before the catch-up loop), so a multi-tick frame reuses one
  consistent input snapshot rather than re-reading the keyboard per tick (audit #27, done).

### Testing
- Unit tests per module via `#[cfg(test)] mod tests`; binary parsers tested with synthetic byte arrays.
- `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must both pass clean.
- **CI note:** the real-*KFM* regression tests are still asset-gated (`test-assets/` is gitignored with no
  CI fetch step), so they run as **green no-ops on CI** — run the suite locally with KFM linked to exercise
  real KFM. But CI is no longer blind to real content (audit #36, done): it loads, matches, and runs the
  `validate` CLI against the shipped original `assets/trainingdummy` character and fails the build on
  regressions. Those trainingdummy tests are **not** asset-gated.

## Dev Loop, Worktrees & Clean-Room

- **Worktree discipline:** operate **only** in this worktree (`fp-work`). Other Claude instances and the
  shared checkout live at `.../FightersParadise`; never edit that directly. (`.git` here is
  `gitdir: .../FightersParadise/.git/worktrees/fp-work`.)
- **Clean-room contract (must stay true):** no Elecbyte/MUGEN engine source or copyrighted assets are
  shipped or tracked. The **only** tracked content/binaries are the project's own **original** assets:
  `assets/banner.png`, the original conformance character under `assets/trainingdummy/`
  (`*.def/.cns/.cmd/.air/.sff/.snd` — MIT, authored from scratch as the shippable default + CI fixture),
  the original common hit-spark effects under `assets/data/` (`fightfx.sff` + `fightfx.air` — MIT,
  audit #17, the shipped common-effects set so KFM/conventional characters render hit-sparks; sprites are
  authored procedurally from scratch), and the original HUD bitmap font `assets/data/font.fnt` (MIT, FL2b —
  a 5x7 block font covering `0-9`, `A-Z`, space, and colon for the real HUD text; an FNT v1 file whose only
  ASCII run is the required `ElecbyteFnt` magic plus its own original `[Def]`/`[Map]` config). The only
  ASCII strings in any tracked `.sff`/`.snd`/`.fnt` are the required `ElecbyteSpr\0`/`ElecbyteSnd\0`/
  `ElecbyteFnt` format magic (and, for the `.fnt`, original engine config), not copyrighted data.
  `*.sff`/`*.snd`/`*.fnt` stay gitignored globally except the `assets/trainingdummy/*.sff`/`*.snd`,
  `assets/data/*.sff`, and `assets/data/*.fnt` paths. Two **original text** motif files also ship under
  `assets/data/`: `assets/data/system.def` + `assets/data/select.def` (MIT — the default clean-room motif:
  an original `[Title Info]` text menu, `[Select Info]` grid geometry, and a `[Characters]` roster pointing
  at the shipped `assets/trainingdummy`; no Elecbyte motif art or text, parsed by `fp-ui`'s
  `system_def`/`select_def`). `.def` files are not globally gitignored, so these two are tracked directly.
  One more **original text** asset ships under `assets/data/`: `assets/data/common1.cns` (MIT — the
  engine-default common-state library, an independent reimplementation of the documented MUGEN engine-common
  states: the get-hit reaction family 5000-series plus the prefall/fall/downed/getup chain. NOT derived from
  Elecbyte's or any character's `common1.cns`). The loader falls back to it when a character's
  `stcommon` reference resolves to a missing file (e.g. evilken's `stcommon = common1.cns`); a character
  that bundles its own common1 (KFM) is unaffected. `.cns` is not globally gitignored, so it is tracked
  directly.
  One **original image** asset ships under `assets/stages/`: `assets/stages/dojo/bg.png` (MIT — an original,
  AI-generated dojo-stage backdrop authored from scratch, no Elecbyte/third-party art). `fp-app` draws it as
  a full-window RGBA background (via `fp_render::ImageTexture` / `RenderFrame::draw_image`) behind the
  fighters whenever no MUGEN `[BGdef]` stage is loaded, so the default match renders over a real backdrop
  instead of a flat clear color. Like `assets/banner.png`, `.png` is not globally gitignored, so it is
  tracked directly.
  Beyond these originals, `git ls-files` shows zero third-party `.sff/.air/.cmd/.cns/.def/.snd/.fnt/.pcx/.act`
  files — the only tracked such files are the originals named above (the `assets/trainingdummy/*` character
  set incl. `trainingdummy.def`, the `assets/data/` effects/font/common-states, and `assets/data/system.def` +
  `assets/data/select.def`).
  Real KFM content (CC BY-NC 3.0, Elecbyte) is **local-only** behind the gitignored `test-assets`
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
