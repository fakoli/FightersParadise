# Session Handoff — 2026-06-15 — Parity wave, live validation, and learnings

> Companion to `2026-06-15-mugen-parity-autonomous-run.md` (the per-task run log).
> This file is the **session-level** handoff: milestones, the queued next-session
> milestone, and an aggregated **Session Learnings** section. Newest = source of truth.

## Repo state (verify with git/gh)

- **`main` tip:** `dac304c` (after the visual-validation harness PR #69).
- **Open PRs:** none.
- **Verification on `main`:** `cargo test --workspace` = **2439 passed / 0 failed**; clippy `-D warnings`
  clean; `cargo fmt --all --check` clean (now a CI gate).
- **Other worktree:** `../fp-work` on `docs/comprehensive-guide` (another instance; untouched).
- **fakoli-state:** 31 tasks — **25 done**, **6 ready** (the F017 follow-ups, below), 0 in_progress.
  Ledger mirror: `.fakoli-state/LEDGER.md`; canonical: `fakoli-state list`.

## Milestones

| # | Milestone | Status |
|---|-----------|--------|
| M1 | **MUGEN parity wave** — 25 tasks across 9 waves, 24 PRs (#44–#67): SFF v2 color, helpers + projectiles, redirects, team Simul/Turns, CPU AI, determinism + replay, full PalFX/AfterImage, AIR transforms, `.act` runtime, stage tiling/anim, screenpack combo/portraits, controller/trigger coverage, validate-CLI for stages/scenes, fmt CI gate | ✅ DONE |
| M2 | **Live visual validation** — `visual-validation` workflow + `scripts/visual_capture.sh`; vision agents confirmed KFM (SFF v2) full color, evilken (SFF v1) color, title menu, keyboard walks P1 (PR #69) | ✅ DONE |
| M3 | **Parity follow-ups (F017)** — Proj* triggers, fp-app TeamMode wiring + partner, TeamMatch round/draw, `.snd` ADX, doc/semantic cleanups, FNT v2. Queued as claimable tasks T026–T031 | 🔜 QUEUED (next session) |
| M4 | **Beyond parity ("enhanced MUGEN")** — the original stretch goal: e.g. rollback netplay (the determinism/replay foundation from T019/T020 is in place), richer AI, broader asset-type breadth, content tooling | 🧭 DIRECTION (not yet specced) |

## Next session — claimable tasks (M3 / feature F017)

These are the reviewer-surfaced, non-blocking follow-ups, already in fakoli-state as `ready`.
Start with: `fakoli-state next` (or `fakoli-state claim T0XX --worktree`), then `/fakoli-state:execute`.

| Task | Pri | What |
|------|-----|------|
| **T026** | medium | Surface projectile hit/contact state to the `Proj*` trigger family (`ProjHit`/`ProjContact`/`ProjGuarded`/`ProjContactTime`/`NumProj`) — they currently read 0 (follow-up from T013). |
| **T027** | medium | Wire `TeamMode` (Simul/Turns) into `fp-app` (CLI flag/menu) and resolve the `partner` redirect live in Simul. |
| **T028** | medium | `TeamMatch` round-flow: single-round / no-life-restore inner mode + a real Draw result (not P1-biased). |
| **T029** | low | `.snd` ADX audio — decode or detect-and-skip (never panic); update compatibility matrix. |
| **T030** | low | Doc/semantic cleanups: stale `SparkSource` doc in fp-combat; `EnvColor time=-1` persist-until-cleared. |
| **T031** | low | FNT v2 sprite-font detection/support (vs the supported v1 bitmap font). |

The same orchestration harness can drive them: workflow script at
`…/workflows/scripts/parity-wave-*.js` (implement→review→auto-fix→PR pipeline); compose a wave from
file-disjoint tasks (see scheduling note in Learnings).

## Session Learnings (aggregated insights)

A running list of what made this session work — apply these next time.

**Engine / format domain**
1. **SFF v2 palettes are RGBA quadruplets (4 B/color); v1 are RGB triplets (3 B/color).** Routing v2
   through the v1 RGB path is exactly what produced KFM's black silhouettes (fixed in T001). `palette()` is
   now version-aware — keep it that way.
2. **"Tracked-deferred" ≠ "silent no-op".** The acceptance bar for controller coverage was "nothing
   silently does nothing", not "implement everything". Recording `HitAdd`/`AttackDist`/`TargetDrop` as
   intentionally-deferred-with-reason (pending the entity graph) satisfied parity-honesty without premature
   implementation (T015).
3. **A bug invisible to `cargo test` is still real.** Rendering/visual defects pass CPU tests; the live
   screencapture + vision-judge loop is the only way to catch them. But prefer making visual bugs
   *unit-testable* (T001 asserts decoded RGBA bytes) so the screen is confirmation, not the only signal.

**Orchestration / git**
4. **`fakoli-state plan` preserves existing task status** — re-planning to add tasks does NOT reset
   `done`/`ready` tasks (only new ones get promoted). Safe to extend the PRD incrementally; back up
   `state.db` if paranoid.
5. **After merging a wave, sync with `git fetch origin main && git reset --hard origin/main`, not
   `git pull --ff-only`.** Workflow worktrees live under `.claude/worktrees/` *inside* the main checkout; a
   stray `git add -A` can dirty the parent index and make `pull --ff-only` abort. `origin/main` is
   authoritative after a squash-merge, and only untracked `.fakoli-state/`/`.claude/`/`target/` survive a
   hard reset (nothing authored is lost — it's all in the merged PRs).
6. **`gh pr merge --delete-branch` warns "cannot delete local branch … used by worktree" — harmless.** The
   remote branch is deleted and the PR merged; clean the local worktree/branch separately.
7. **Hotspot-file scheduling.** `fp-engine/src/lib.rs`, `fp-character/src/executor.rs`, and
   `fp-app/src/main.rs` are touched by many tasks. Keep each wave to **≤1 task per hotspot file** → merges
   are conflict-free, no rebases. When two tasks are tightly coupled on the same files, **combine them into
   one PR** (did this for T013+T014).
8. **Crate-scoped verification in agents, workspace-scoped in the orchestrator.** Agents run only their
   crate's `cargo test`/`clippy` (fast — `fp-formats` doesn't pull SDL2/wgpu); the orchestrator runs
   `cargo test --workspace` on the warm main checkout after each wave as the integration gate.
9. **Workflow `args` can arrive string-encoded.** Guard every script top with
   `if (typeof args==='string') args = JSON.parse(args)` — otherwise `args.tasks` is `undefined` and a
   `pipeline([])` silently no-ops (lost the first Wave-1 launch to this).
10. **The implement→review→auto-fix→re-review loop earns its keep.** 30/31 tasks passed first review; the
    one MUST_FIX (T015) was caught and auto-fixed before merge. Default reviewers to MUST_FIX when an
    acceptance criterion isn't *clearly demonstrated*.

**Live capture (macOS)**
11. **A backgrounded SDL window beachballs (pinwheel).** Before `screencapture`, **activate** the window
    (`osascript … set frontmost of process "fp-app" to true`) so it pumps events; always `kill -9` after so
    nothing lingers. The title-menu "hang" was purely this capture artifact, not an app bug.
12. **Vision-capable subagents are the unlock.** A workflow agent's `Read` tool renders PNGs *visually*, so
    each judge can assert "KFM is full-color, not silhouettes" or "P1 moved right between frames".
13. **Capture serial, judge parallel.** Overlapping GUI windows corrupt geometry/focus, so frames are
    captured one app-launch at a time; the read-only vision judging then fans out concurrently.
14. **`fp-app` is a bare cargo binary** → computer-use screenshots are filtered out; macOS's own
    `screencapture -x -R x,y,w,h` (window geometry via `osascript`) is the working path. Needs Screen
    Recording granted to the controlling app; a locked screen has no GUI session (deferred the check once
    for this reason).

## Invariants (don't relearn)

- **Clean-room contract is inviolable:** synthetic in-memory fixtures only; nothing third-party/copyrighted
  committed; nothing under `test-assets/` (real KFM/evilken are local-only, gitignored).
- **Never panic on bad content:** parsers/evaluator return `FpResult`/recoverable errors; `unwrap`/`expect`
  only in tests.
- **fakoli-state is the ledger** (`git_ops_mode: record_only`); agents own all git. Lifecycle:
  `claim → submit --commands/--files-changed/--pr-url → apply --approve` (→ done).
