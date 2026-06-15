# Handoff — 2026-06-15 — MUGEN-parity autonomous run (25 tasks)

## Repo state (verify with git/gh, don't trust blindly)

- **`main` tip:** `220de10` — "T022: format workspace and add cargo fmt CI gate (#67)".
- **Open PRs:** none from this run (all merged). 
- **Branch:** `main`, clean, in sync with `origin/main`.
- **Verification on `main`:** `cargo test --workspace` = **2439 passed / 0 failed** (exit 0);
  `cargo clippy --workspace --all-targets -- -D warnings` = clean (exit 0); `cargo fmt --all --check` =
  clean (exit 0, now a CI gate).
- **Other worktree:** `../fp-work` on `docs/comprehensive-guide` (another instance; untouched here).

## What this session did

An autonomous orchestration run drove a fakoli-state PRD (`.fakoli-state/prd.md`, approved) of
**16 features / 25 tasks** to completion across **9 waves**. Each task was implemented in an isolated
git worktree by a subagent, self-reviewed by a second agent against acceptance criteria, auto-fixed +
re-reviewed on a MUST_FIX, opened as a PR, then merged by the orchestrator only after build + the task's
tests + clippy were green and review was not MUST_FIX. A `cargo test --workspace` integration gate ran on
`main` after every wave. fakoli-state tracked claims/evidence/acceptance (`git_ops_mode: record_only`;
the ledger mirror is `.fakoli-state/LEDGER.md`).

**24 PRs merged (#44–#67), all 25 tasks accepted `done`:**

| Area | Tasks / PRs |
|------|-------------|
| **SFF v2 color (headline)** | T001 #44 — v2 palettes are RGBA quads not RGB triplets; **fixes KFM black silhouettes** (former task #32), regression-tested |
| Hit sparks | T002 #49 — common-vs-own spark source asserted (feature pre-existing) |
| Stages | T003 #55 (tiling + per-layer velocity), T004 #58 (animated `type=anim` layers + vertical camera) |
| Screenpack HUD | T005 #45 (combo + multi-layer bg), T006 #53 (`[Face]` portraits) |
| Effects | T007 #59 (true AfterImage frame-history ring), T008 #62 (full PalFX: add/mul/sinadd/invertall/color) |
| AIR / palettes | T009 #63 (per-frame scale/angle/Interpolate at render), T010 #50 (`.act` runtime override) |
| Storyboard | T011 #61 (per-scene fade/clearcolor/BGM + length-driven timing) |
| **Entity graph** | T012 #56 (helper slot-map + Helper controller + parent/root/helper redirects), T013+T014 #65 (Projectile controller + target/partner/playerid redirects) |
| Controllers/triggers | T015 #48 (EnvShake/EnvColor/RemapPal/Trans/Angle/Life/clipboard + tracked-deferred), T016 #46 (RoundNo/RoundsExisted) |
| Modes / AI | T017 #64 (TeamMatch: N-per-side Simul/Turns), T018 #66 (deterministic CPU AI for P2) |
| Determinism | T019 #57 (whole-Match serialization), T020 #60 (asset-free determinism + record/replay test) |
| Compatibility | T021 #54 (asset-type matrix in `docs/mugen-compatibility.md` + `.snd` gap) |
| Validation (user milestone) | T025 #52 (`validate` CLI now lints stages + scenes) |
| Input (user milestone) | T024 #47 (keyboard movement/attack mapping verified + hardened) |
| Hygiene | T023 #51 (CNS trigger-group contiguity / CB6), T022 #67 (workspace fmt + `cargo fmt --check` CI gate) |

## Where work stopped / next steps

All 25 PRD tasks are done. Recommended follow-ups (each logged from a reviewer SHOULD_FIX, none blocking):

1. **Proj\* triggers** (`ProjHit`/`ProjContact`/`ProjGuarded`/`ProjContactTime`/`NumProj`) still evaluate
   to 0 — projectile hit/contact state isn't surfaced to those triggers yet (T013 covered spawn/move/hit
   + the owner's `target` redirect). Wire per-projectile counters into `fp-vm` evaluation.
2. **`partner` redirect in 1v1** — implemented + unit-tested via `EntityGraph::with_partner`, but
   `Match::tick` wires it to `None` in 1v1; surface it through `TeamMatch` Simul mode.
3. **fp-app TeamMode wiring** — `TeamMatch` (Simul/Turns) exists in `fp-engine` but `fp-app` still drives
   the 1v1 `Match` directly; add a CLI flag / menu to select team modes.
4. **TeamMatch semantics** — inner `Match` still runs its own round flow (life restore); consider a
   single-round/no-restore inner mode for Simul/Turns, and a real Draw result (currently P1-biased).
5. **`.snd` ADX audio** — documented unsupported in `docs/mugen-compatibility.md`; only WAV/PCM decode.
6. **Live visual verification still owed** — the SFF v2 color fix and keyboard input are verified by unit
   + integration tests, but a live windowed screencapture could NOT run this session (screen locked while
   unattended; SDL2 window had no GUI session). When at the machine, confirm visually with:
   `RUST_LOG=error cargo run -p fp-app -- test-assets/kfm/kfm.def` (KFM should now render in **color**),
   and tap arrow keys + an attack to confirm input. See "Live-debugging the windowed app" in CLAUDE.md.

## Invariants / gotchas (don't relearn)

- **SFF v2 palettes are RGBA (4 bytes/color), v1 are RGB (3 bytes/color).** `SffFile::palette()` is now
  version-aware; do not route v2 through the v1 RGB path (that was the silhouette bug).
- **Merging while workflow worktrees are live can dirty the parent index** (worktrees live under
  `.claude/worktrees/` inside the main checkout; a stray `git add -A` leaks). After merging a wave, sync
  with **`git fetch origin main && git reset --hard origin/main`**, not `git pull --ff-only` (which aborts
  on the dirtied index). `origin/main` is authoritative after a squash-merge.
- **`gh pr merge --delete-branch` warns** "cannot delete local branch … used by worktree" — harmless; the
  remote branch is deleted and the PR is merged. Clean the local worktree/branch separately.
- **fakoli-state lifecycle:** `claim` → `submit --commands … --files-changed … --pr-url …` → `apply
  --approve` (→ done). `git_ops_mode: record_only` so agents own all git. Ledger mirror in
  `.fakoli-state/LEDGER.md`; canonical state via `fakoli-state list`.
- **Scheduling bottleneck:** `fp-engine/src/lib.rs`, `fp-character/src/executor.rs`, and
  `fp-app/src/main.rs` are touched by many tasks — waves were kept to ≤1 task per hotspot file to avoid
  merge conflicts. T013+T014 were combined into one PR for the same reason.
- **Clean-room intact:** every task used synthetic in-memory fixtures; nothing added under `test-assets/`;
  no third-party assets tracked.
