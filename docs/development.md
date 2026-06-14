# Fighters Paradise — Development Guide

Everyday build, run, and test workflow for working on the engine on your own
workstation, plus the onboarding map a new contributor (or a fresh agent
session) needs to get productive fast. For design background see
[Architecture](architecture.md); for what's missing see
[Known Issues](known-issues.md) and the [Roadmap](roadmap.md); for contribution
conventions see the root [CONTRIBUTING](../CONTRIBUTING.md).

> **Project state (not early stubs):** this is a **playable two-character
> fighter** driven by real Kung Fu Man data — KFM's throw, supers (meter),
> hitpause, i-frames, hit reactions, jump/airjump/land, and damage multipliers
> all work end to end. Only `fp-stage` and `fp-ui` are true stubs. The root
> [CHANGELOG.md](../CHANGELOG.md) and `docs/knowledge-base/04-codebase-review.md`
> are **stale** — trust the source, [CLAUDE.md](../CLAUDE.md), and this doc.

## Prerequisites

Fighters Paradise is a Rust (edition 2021, resolver v2) Cargo workspace with
**14 crates** under `crates/`. SDL2 and wgpu are native dependencies.

| Tool | Install | Notes |
| --- | --- | --- |
| Rust | [rustup.rs](https://rustup.rs) → `rustup toolchain install stable` | Any recent `stable`; CI pins stable + the `clippy` component. No toolchain file in-repo. |
| SDL2 (macOS) | `brew install sdl2` | Hard native dep (`sdl2 0.37`). The only manual step on macOS. |
| SDL2 (Debian/Ubuntu) | `sudo apt install libsdl2-dev` | Same package CI installs. |

### The SDL2 / `RUSTFLAGS` / `.cargo/config.toml` note

The Rust linker has to find the native SDL2 library:

- **Apple Silicon macOS:** handled automatically.
  [`.cargo/config.toml`](../.cargo/config.toml) injects
  `rustflags = ["-L", "/opt/homebrew/lib"]` for the `aarch64-apple-darwin`
  target. After `brew install sdl2`, plain `cargo build` works — **no manual
  `RUSTFLAGS` needed.**
- **Intel macOS:** the `config.toml` target doesn't match, so the
  [`Makefile`](../Makefile) and [`scripts/fp.sh`](../scripts/fp.sh) append
  `-L $(brew --prefix)/lib` to `RUSTFLAGS` at runtime — covering `/usr/local`.
  This is a no-op on Linux/CI and a harmless duplicate `-L` on Apple Silicon.
- **By hand, if ever needed:** `RUSTFLAGS="-L /opt/homebrew/lib" cargo build --workspace`.
- **Linux / CI:** system `libsdl2-dev` is on the default search path; no flags.

## Raw cargo commands

There is no `xtask` or build script — every operation is plain `cargo`:

```sh
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p fp-app                        # default two-KFM match (test pattern if assets absent)
cargo run -p fp-app -- p1.def [p2.def]     # explicit character(s)
cargo run -p fp-app -- file.sff [file.air] # static SFF sprite / AIR viewer
```

The two wrappers below (Makefile + `scripts/fp.sh`) are thin conveniences over
exactly these commands — there's no build magic, so anything via `make` or
`fp.sh` you can also run by hand.

## The Makefile — canonical dev interface

The repo root [`Makefile`](../Makefile) wraps the cargo commands above with a
self-documenting `help` default target. Run `make` or `make help` to list
targets:

| Target | Does |
| --- | --- |
| `make help` | List targets (default) |
| `make build` | Build the workspace (debug) |
| `make run` | Run fp-app — real KFM match if `test-assets` present, else the no-arg test pattern |
| `make run-kfm` | Explicit two-KFM match (errors if `test-assets/kfm/kfm.def` is missing) |
| `make run-sprite SFF=… [AIR=…]` | Static SFF sprite / AIR viewer |
| `make test` | Full workspace test suite |
| `make test-fast [CRATE=fp-vm]` | One crate's tests, or `--lib --bins` only for fast feedback |
| `make check` | `cargo check --all-targets` (type-check, no binaries) — fastest feedback |
| `make clippy` | Clippy with `-D warnings` (matches CI) |
| `make fmt` / `make fmt-check` | Format in place / check formatting |
| `make doc` | Build & open API docs (`cargo doc --no-deps --open`) |
| `make clean` | `cargo clean` (removes `target/`) |
| `make ci` | Local gate: `clippy -D warnings` + `fmt-check` + `test` |

> `make ci` runs `fmt-check`, but **GitHub CI does not** yet — adopting
> `rustfmt` is tracked as backlog CB3, so do not be surprised if `make ci`
> flags formatting that CI is green on. `make ci` is therefore *stricter* than
> GitHub CI. See [`.github/workflows/ci.yml`](../.github/workflows/ci.yml).

## scripts/fp.sh — windowed-game process control

A `Makefile` target runs in the foreground and cannot cleanly supervise a
long-running GUI window. [`scripts/fp.sh`](../scripts/fp.sh) is the
process-control wrapper for the SDL2/wgpu game: it can launch fp-app
**detached**, record its PID, and later **stop/restart/status** it. This is the
tool to reach for in a build-play-iterate loop where you want to relaunch the
window without hunting for the process.

```sh
scripts/fp.sh <command> [args...]
```

| Command | Does |
| --- | --- |
| `build` | Build the workspace (debug) |
| `run [args]` | Run fp-app in the **foreground** (Ctrl-C to quit) |
| `start [args]` | Build, then launch fp-app **detached**; record PID + log |
| `stop` | Stop the detached fp-app (SIGTERM, then SIGKILL after ~5s) |
| `restart [args]` | `stop` then `start` (args passed through) |
| `status` | Report whether the detached fp-app is running (+ log path) |
| `clean` | `cargo clean` |
| `test` | Workspace test suite |
| `lint` | Clippy with `-D warnings` |

Detached runs write `fp-app.pid` and `fp-app.log` to a `.run/` directory at the
repo root, falling back to `$TMPDIR/fp-work-run/` if `.run/` cannot be created.
Watch a detached session with `tail -f .run/fp-app.log`. The script is robust to
a missing or stale PID file: `stop` and `status` clean up a recorded PID whose
process has already exited, and `start` refuses to launch a second instance
while one is already running.

> **Note:** `.run/` is runtime scratch and is already **gitignored** (the entry
> `/.run` is in `.gitignore`), so the detached-run PID/log files are correctly
> ignored and won't show up as untracked. `scripts/fp.sh` is built around this —
> it comments "Prefer an in-repo, gitignored `.run/` dir."

Typical loop:

```sh
scripts/fp.sh start           # build + launch detached
scripts/fp.sh status          # confirm it came up
# … edit code …
scripts/fp.sh restart         # stop the old window, rebuild, relaunch
scripts/fp.sh stop            # done
```

## Running the game with real content

The app supports several CLI shapes; all degrade to a checkerboard **test
pattern** on missing/bad assets and **never panic** (`select_mode`,
`crates/fp-app/src/main.rs:972`).

```sh
make run                                    # two-KFM match if test-assets present, else test pattern
cargo run -p fp-app                         # same (no-args default)
cargo run -p fp-app -- p1.def [p2.def]      # two-character match (one .def = same char both sides)
cargo run -p fp-app -- file.sff [file.air]  # legacy static sprite / animation viewer
scripts/fp.sh start                         # launch detached, then `status` / `stop`
```

- **No args** loads a two-character Kung Fu Man match from
  `DEFAULT_DEF = "test-assets/kfm/kfm.def"`
  ([`crates/fp-app/src/main.rs:71`](../crates/fp-app/src/main.rs)); if that path
  is absent it falls back to a checkerboard test pattern.
- **Controls this milestone:** P1 is the keyboard. **P2 is a hardcoded idle
  dummy** (`MatchInput::none()`, `main.rs:1133`) — no second-player input map or
  AI yet.
- **What you'll see:** two fighters drawn from their current AIR frame, two life
  bars + a KO/winner marker (a hand-rolled solid-quad HUD, **not** a `fight.def`
  screenpack — `fp-ui` is a stub), over a flat clear color (no stage background
  — `fp-stage` is a stub).

`test-assets` is a **local-only, gitignored** symlink into the shared checkout —
KFM content (Elecbyte, CC BY-NC 3.0) is never tracked or shipped. If `make run`
shows a checkerboard, the symlink is missing; re-link it to your local KFM
fixtures. See the [clean-room constraints](#worktree-discipline--clean-room).

## Testing

```sh
make test                       # full workspace suite (~1769 pass incl. doc-tests)
cargo test --workspace          # the same, by hand
make test-fast CRATE=fp-vm      # one crate, fast iteration
make test-fast                  # all crates' lib/bin unit tests only (skips integration + doc tests)
```

The suite is large and green: ~1769 tests pass (~1724 `#[test]` attributes plus
doc-tests). Rough per-crate share:

| Crate | Tests | Crate | Tests |
| --- | ---: | --- | ---: |
| `fp-character` | 620 | `fp-input` | 102 |
| `fp-vm` | 463 | `fp-engine` | 96 |
| `fp-formats` | 142 | `fp-physics` | 79 |
| `fp-combat` | 60 | `fp-app` | 49 |
| `fp-storyboard` | 44 | `fp-audio` | 32 |
| `fp-render` | 22 | `fp-core` | 15 |

(`fp-stage` and `fp-ui` are stubs: 0 tests each.)

### Real-content tests are gated

Real-content tests (e.g. `crates/fp-formats/tests/real_content.rs`,
`crates/fp-vm/tests/cns_integration.rs`) decode/evaluate the genuine KFM
fixture. They **skip cleanly when `test-assets/` is absent** — they check the
path and early-return rather than fail. So locally:

- **With KFM linked:** the real-content net actually runs and guards against
  regressions in parsing, evaluation, and the playable loop.
- **Without it:** those tests no-op green; only synthetic/fixture tests (e.g.
  the always-on RLE5 fixture) truly execute.

### The CI asset-gate caveat (known issue #36)

`test-assets/` is gitignored **and CI has no fetch/restore step**, so on GitHub
**every real-content test runs as a green no-op.** A passing CI run does **not**
prove the real-KFM path still works — only that the synthetic tests pass. **Run
the full suite locally with KFM linked** before trusting a change that touches
parsing, evaluation, or match flow. Full write-up in
[Known Issues](known-issues.md).

## CI pipeline

[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) — one Ubuntu job, on
push/PR to `main`:

1. `actions/checkout`
2. Install `libsdl2-dev`
3. Install the **stable** toolchain + `clippy` component
4. Cargo cache (`Swatinem/rust-cache`)
5. **Clippy** — `cargo clippy --workspace --all-targets -- -D warnings`
6. **Build** — `cargo build --workspace`
7. **Test** — `cargo test --workspace`

Notes:

- Warnings are denied via the **`RUSTFLAGS: "-D warnings"` env var** set globally
  in the workflow, not by the clippy flag alone.
- There is **no `cargo fmt --check` gate** (codebase isn't rustfmt-clean yet;
  backlog **CB3**). `make ci` runs `fmt-check` locally, so it's stricter than CI.
- See above for the **#36 asset-gate**: real-content tests are no-ops on CI.

### Before you push

Run the same gate CI enforces (clippy + build + test), plus formatting if you
intend to keep the tree fmt-clean:

```sh
make ci          # clippy -D warnings + fmt-check + test (stricter than CI)
# or the CI-exact subset:
make clippy && make build && make test
```

## How this codebase was built — the reviewed agent build-loop

Much of Fighters Paradise was produced by an **audit-driven, reviewed agent
build-loop**. Knowing how it works helps you read the commit history and the
[faithfulness audit](knowledge-base/08-faithfulness-audit.md), which doubles as
the prioritized work ledger. Per task, the loop is:

```
worktree  →  task  →  review  →  fix  →  verify   (repeat if verify fails)
```

- **worktree** — work happens in an isolated git worktree (this one, `fp-work`),
  never the shared checkout.
- **task** — a single ranked item, usually from the faithfulness audit
  (`docs/knowledge-base/08-faithfulness-audit.md`, 39 ranked MUGEN-fidelity
  items) or the execution plan (`docs/knowledge-base/06-execution-plan.md`, the
  live DONE/TODO ledger with commit hashes).
- **review** — an Opus review agent inspects the diff.
- **fix** — a fix agent addresses review findings.
- **verify** — a pass/fail agent confirms the change against the gate (build +
  test + clippy, ideally the real-content suite); on failure the loop repeats.

> **The loop harness lives OUTSIDE the repo and is NOT tracked.** It sits at
> `/Users/sdoumbouya/code/claude-env/.fp-loop/` (e.g. `fp-build-task.mjs`,
> `fp-loop-batch.mjs`), a sibling of this worktree. `git ls-files` shows nothing
> under it — it's deliberately external tooling, not part of the shipped
> project. Don't look for it in-repo and don't add it.

The audit's inline ✅ markers can **lag the actual code** — verify done-status
against source, not the markers (e.g. keystone items #1/#2 are implemented but
unmarked; #9 over-claims `HitOverride`, which is still missing).

## Worktree discipline & clean-room

These two rules are non-negotiable.

### Worktree discipline

- **Operate only in this worktree (`fp-work`).** Its `.git` is a pointer:
  `gitdir: .../FightersParadise/.git/worktrees/fp-work`.
- The **shared checkout** at `.../FightersParadise` is used by other instances —
  **never edit it directly.** Make all changes here.
- Commit/push **only when asked**. If you're on the default branch, branch first.

### Clean-room contract (must stay true)

- **No Elecbyte/MUGEN engine source or copyrighted assets are shipped or
  tracked.** This is a clean-room reimplementation.
- `git ls-files` shows **zero** `.sff/.air/.cmd/.cns/.def/.snd/.fnt/.pcx/.act`
  files. The only tracked binary asset is the project's own `assets/banner.png`.
  `.gitignore` blanket-ignores `*.sff`, `*.snd`, `*.fnt`, and `/test-assets`.
- Real **KFM** content (CC BY-NC 3.0, Elecbyte) is **local-only**, reached
  through the gitignored `test-assets` symlink. **Never commit or ship it.**
  Provenance/licensing is in `test-assets/SOURCES.md` (behind the symlink).
- Fighters Paradise is an independent project; **MUGEN is a trademark of
  Elecbyte.** Project code is MIT (© 2025 Sekou Doumbouya); see
  [LICENSE](../LICENSE).

## Navigating the codebase

The workspace is 14 crates under `crates/`. Dependency spine: `fp-app` →
`fp-engine` → `fp-character` → `fp-vm` / `fp-combat` / `fp-physics` /
`fp-input`; `fp-combat` depends only on `fp-core` + `fp-physics`; everything
depends on `fp-core`. Full design overview: [Architecture](architecture.md).

| Crate | Status | Start here | One-line role |
| --- | --- | --- | --- |
| `fp-character` | Implemented (largest) | `crates/fp-character/src/loader.rs` · `executor.rs` · `lib.rs` · `combat.rs` | `.def`→`LoadedCharacter` loader, live `Character` entity, per-tick MUGEN-order executor (~30 controllers), cross-entity `EvalCtx` keystone, and the `resolve_attack` hit-application bridge. |
| `fp-vm` | Implemented | `crates/fp-vm/src/lexer.rs` · `parser.rs` · `evaluator.rs` · `eval.rs` | CNS trigger-expression engine: lexer → Pratt parser → **tree-walk evaluator** (NOT a bytecode/stack VM despite the name). `Value` model + `EvalContext` trait seam. |
| `fp-formats` | Implemented | `crates/fp-formats/src/sff/mod.rs` · `cns.rs` | Parsers: SFF v1 (PCX) + v2 (RLE8/RLE5/LZ5/raw), AIR, CMD, DEF, CNS, SND. PNG decode, SFF v1 palette, FNT/ACT are gaps. |
| `fp-input` | Implemented | `crates/fp-input/src/command.rs` · `buffer.rs` | 60-frame ring buffer + backward-scan command matcher (`~` `/` `$` `>` `+`). |
| `fp-engine` | Implemented | `crates/fp-engine/src/lib.rs` | Two-player `Match` coordinator: 6-step tick, round/best-of-N flow, push/bounds, deferred-op (`Target*`) application. |
| `fp-physics` | Implemented | `crates/fp-physics/src/lib.rs` · `collision.rs` · `push.rs` | Euler integration, gravity (0.44), Y=0 ground plane, AABB `Clsn` overlap, player push/bounds. (No friction — that's `fp-character`.) |
| `fp-combat` | Implemented | `crates/fp-combat/src/lib.rs` | Pure leaf: `HitDef` data model, `Clsn1`×`Clsn2` `detect_hit`, pure `resolve_hit` → `HitOutcome`. No mutation. |
| `fp-app` | Implemented | `crates/fp-app/src/main.rs` | SDL2 window, 60Hz accumulator loop, CLI modes, hand-rolled HUD, two-player wiring, audio routing. |
| `fp-storyboard` | Parser only | `crates/fp-storyboard/src/storyboard.rs` | Storyboard `.def` parser + typed scene model. No tick/render; no in-engine consumer. |
| `fp-audio` | Implemented | `crates/fp-audio/src/system.rs` · `sound.rs` | rodio WAV decode + channel cut-off mixer; `NullBackend` headless fallback (never panics). |
| `fp-render` | Implemented | `crates/fp-render/src/renderer.rs` · `shaders/palette.wgsl` | wgpu sprite renderer; WGSL palette-lookup shader, 256-color indexed (palette idx 0 = transparent); 3 blend pipelines. |
| `fp-core` | Implemented | `crates/fp-core/src/lib.rs` · `error.rs` | Shared types: `Vec2`, `Rect`, `SpriteId`, `FpError`/`FpResult`. |
| `fp-stage` | **Stub** | `crates/fp-stage/src/lib.rs` | Empty (7-line doc) — no `[BGDef]`/`[BG]`/`[Camera]` parser; matches render over a flat color. |
| `fp-ui` | **Stub** | `crates/fp-ui/src/lib.rs` | Empty (7-line doc) — HUD is hand-rolled quads in `fp-app`, not a `fight.def`/`fight.sff` screenpack. |

### Two keystones to internalize first

1. **Cross-entity eval** — `EvalEnv { opponent, stage, anim }` (`Copy`) is
   threaded through the executor; each eval reborrows `&*self` into a short-lived
   `EvalCtx { me, opponent, stage, anim }`
   (`crates/fp-character/src/executor.rs:810`). This is what makes
   `p2`/`enemy`/`root` redirects, `P2Dist`/`P2BodyDist`, and edge distances work.
   A bare `Character` is self-only (`redirect()` → `None`).
2. **Deferred-effects (`TickReport`)** — a tick can't `&mut` another entity, so a
   character emits `sound_requests` (`PlaySnd`) and `target_ops` (`Target*`
   throws) into a `TickReport`; `fp-engine`'s `Match` applies them after both
   characters tick. Hit *application* itself lives in
   `fp_character::combat::resolve_attack`, not in `fp-combat`/`fp-engine`.

## See also

- [README](../README.md) — public-facing overview & quickstart.
- [CLAUDE.md](../CLAUDE.md) — agent-oriented project brief.
- [Architecture](architecture.md) — design overview, dependency graph, decisions.
- [MUGEN compatibility](mugen-compatibility.md) — supported features/formats.
- [Content guide](content-guide.md) — structuring bring-your-own-character content.
- [Known issues](known-issues.md) — ranked fidelity gaps (mirrors the audit).
- [Roadmap](roadmap.md) — what's planned next.
- [knowledge-base/](knowledge-base/) — MUGEN research, execution-plan ledger,
  evaluator semantics, faithfulness audit.
