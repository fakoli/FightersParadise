# Session Handoff — 2026-06-14

> **Purpose.** Hand this session's state to another workstation / a fresh session so it can resume
> without this conversation. This is the most recent handoff in [`docs/handoffs/`](./); read it first.
> Convention is documented in the root [CLAUDE.md](../../CLAUDE.md) → "Session handoffs".

## TL;DR — read this first

- **`main`** has the full engine: PR [#3](https://github.com/fakoli/FightersParadise/pull/3) **merged**
  (2026-06-14). It is a **playable two-character Kung Fu Man fighter** — not an early stub.
- **Docs PR [#4](https://github.com/fakoli/FightersParadise/pull/4) is OPEN and MERGEABLE**
  (branch `docs/comprehensive-guide`). It adds all the authoritative docs + dev tooling. **Next action:
  review & merge it.**
- **Where we left off on the audit "P" fixes:** ~15 of the 39 ranked items are done; ~24 remain. The
  authoritative done/remaining split + recommended order live in
  [docs/roadmap.md](../roadmap.md) and [docs/known-issues.md](../known-issues.md). Resume from the
  "Effort-honest sequencing" list in the roadmap.

## Repo & PR state (verified 2026-06-14)

| Thing | State |
|-------|-------|
| `main` tip | `a59e658` — "Merge pull request #3 from fakoli/build/fp-engine" |
| PR #3 (engine) | **MERGED** 2026-06-14T20:09:08Z |
| PR #4 (docs + tooling) | **OPEN**, `MERGEABLE`, branch `docs/comprehensive-guide`, 1 commit `f173d28` ahead of main |
| Test suite | `cargo test --workspace` → **1769 pass**; `clippy --workspace --all-targets -- -D warnings` clean; CI green |
| Stub crates | only `fp-stage` and `fp-ui` (7-line module-doc files) |

> The local worktree used this session was `/Users/sdoumbouya/code/claude-env/fp-work` (branch
> `docs/comprehensive-guide`). That path is **machine-specific** — a different workstation should clone
> the repo fresh and branch from `main`.

## Get set up on a fresh workstation

```sh
git clone https://github.com/fakoli/FightersParadise.git
cd FightersParadise
# macOS: brew install sdl2     |     Linux: sudo apt install libsdl2-dev
make build      # or: cargo build --workspace
make test       # full suite
make help       # all dev targets
./scripts/fp.sh status   # windowed-game process control (start/stop/restart/status)
```

- SDL2 is a hard native dep. On `aarch64-apple-darwin`, `.cargo/config.toml` injects the homebrew
  linker path automatically; elsewhere `make` adds `-L $(brew --prefix)/lib` when brew exists.
- **Real-content (KFM) tests are clean-room-gated.** `test-assets/` is a **local-only, gitignored
  symlink** to a legally-obtained KFM bundle. Without it, the real-content tests **skip** (and on CI
  they are green no-ops — see gap #36). To exercise them locally, point `./test-assets` at your own
  KFM. **Never commit KFM/Elecbyte content.**

## What this session did

1. **Merged the engine into `main` (PR #3).** The blocker was an SFF-parser conflict from `origin/main`
   advancing (PRs #1/#2 reworked the SFF parser after this branch forked). Resolution: kept **main's
   real-MUGEN-1.0 RLE5 codec** (a superset of ours + an OOM guard) but **preserved our faithful
   Elecbyte/Ikemen-GO LZ5 decoder** (main's generic LZ77 regressed on real KFM sprite 1), and **fixed
   main's broken `rle5_real_content.rs` test fixture** (it wrote the SFF header in a layout that
   contradicted main's own parser). Verified 1769 tests + CI green before merging.
2. **Authored the documentation + tooling (PR #4).** README/CLAUDE.md corrected (they claimed
   "v0.1.0, mostly stubs"); new `docs/architecture.md`, `mugen-compatibility.md`, `content-guide.md`,
   `known-issues.md`, `roadmap.md`, `development.md`; `Makefile` + `scripts/fp.sh`; updated CHANGELOG +
   KB index. Every claim fact-checked against source by a multi-agent verification pass.

## Where we left off — the audit "P" fixes (the build loop)

The engine is driven by the **39-item faithfulness audit**:
[docs/knowledge-base/08-faithfulness-audit.md](../knowledge-base/08-faithfulness-audit.md) (the gap map)
and [06-execution-plan.md](../knowledge-base/06-execution-plan.md) (the live ledger). The honest current
status is mirrored in [docs/known-issues.md](../known-issues.md) and [docs/roadmap.md](../roadmap.md).

- **DONE this session (do not re-do):** cross-entity eval keystone (#1/#2), `SelfState` (#3), velocity
  constants (#4), `Const720p` (#5), `AnimElemTime` (#6), `GetHitVar(animtype)` (#7), throws (#8),
  `NotHitBy`/`HitBy` i-frames (the implemented half of #9), `VelMul` (#11), friction idle-stop (#12),
  airjump + ground-clamp/auto-land (#14/#15), damage multipliers (#19), `SelfAnimExist` (#22), plus
  super meter, hitpause, best-of-3 rounds, and audio. **`HitOverride` (the other half of #9) is still
  missing** (no dispatch arm).
- **REMAINING (resume here):** see roadmap "Effort-honest sequencing". Shortest path to "runs *my*
  character, not just KFM":
  1. **CI asset-gate fix (#36) + a character validator** — so failures are visible and the test net is real.
  2. **PNG sprite decode (#35)** — so modern HD art loads at all.
  3. **SFF v1 palette extraction (#25)** — so legacy art stops rendering invisible.
  4. **Dropped Statedef headers / `SprPriority` / juggle (#16)** and **get-hit controllers (#23)** — correct fights for arbitrary characters.
  5. **Stage `.def` + backgrounds (#29)** and **real lifebars / `fight.def` (#31)** — content actually shows up framed.
  Also outstanding: super-pause freeze (#24), RoundState threading (#21), `Width` (#10),
  `AssertSpecial` (#13), priority/trade (#20), hit sparks (#17), PalFX/AfterImage (#33), Clsn debug
  overlay (#34 — pull forward), power-bar HUD (#26), input-sample-in-catchup (#27), RNG-in-state (#28),
  replay/rollback (#38), team/tag modes (#39).

### Resuming the build loop

The reviewed agent build-loop harness is **outside the repo** and **machine-local** (this session:
`/Users/sdoumbouya/code/claude-env/.fp-loop/`, e.g. `fp-build-task.mjs`, `fp-loop-batch.mjs`). It is not
tracked and **not portable**. On a new workstation, either re-create the loop or drive items by hand:
pick the top remaining audit item → implement it (usually a dispatch arm in
`crates/fp-character/src/executor.rs` or an `fp-engine` mechanic) → gate on `make ci` **and** the
real-content tests with KFM linked → review → commit. Mark items done in audit doc 08 + the ledger 06.

## Invariants — don't relearn these the hard way

- **Clean-room is non-negotiable.** No Elecbyte/MUGEN engine source or copyrighted assets in the repo.
  `git ls-files` must show zero `.sff/.air/.cmd/.cns/.def/.snd/...`; the only tracked binary is
  `assets/banner.png`. KFM stays local-only behind the gitignored `test-assets` symlink.
- **Worktree discipline.** Work in a dedicated worktree; never edit a shared/main checkout directly.
- **`fp-vm` is a tree-walk evaluator**, not a bytecode/stack VM, despite the crate name and docstring.
- **CI green ≠ real-content safe (#36).** Real-KFM regression tests are no-ops on CI; run locally with
  KFM linked before trusting a fidelity change.
- **Never panic on bad content** — warn + safe-default; parse failures fall back to const-0.

## Pointers

[README](../../README.md) · [CLAUDE.md](../../CLAUDE.md) · [Architecture](../architecture.md) ·
[MUGEN Compatibility](../mugen-compatibility.md) · [Content Guide](../content-guide.md) ·
[Known Issues](../known-issues.md) · [Roadmap](../roadmap.md) · [Development](../development.md) ·
[Faithfulness Audit (08)](../knowledge-base/08-faithfulness-audit.md) ·
[Execution Plan (06)](../knowledge-base/06-execution-plan.md)
