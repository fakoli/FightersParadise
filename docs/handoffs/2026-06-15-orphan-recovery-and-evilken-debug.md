# Session handoff — evilken debugging + orphaned-workflow recovery (2026-06-15)

> Pickup point for the next session. Supersedes
> [2026-06-15-forward-looking-and-menu.md](2026-06-15-forward-looking-and-menu.md).

## Repo state (verified)

- **`main` tip: `3888643`** (was `a8528c1` at session start — the menu-handoff commit `#33`).
- **No open PRs. No active feature branches** (3 stale remote `feat/*` branches —
  `feat/power-hud-input`, `feat/rng-in-state`, `feat/sff-sprite-decode` — predate this
  session, from the PR #26–28 era; harmless leftovers, left as-is).
- Worktrees: only the shared checkout (`FightersParadise`, main) and the unrelated
  `fp-work` (docs/comprehensive-guide). All session landing worktrees cleaned up.
- `cargo test --workspace` + `cargo clippy --workspace --all-targets -D warnings` green
  on `main` with `test-assets/` linked. ~2210 tests.

## What this session did

Resolved the user's reported evilken bugs — a **warning flood** ("Need to debug this" +
full load log) and a **black scene** ("the scene was black but I saw a countdown") — via
**5 merged PRs** (#34–#38). Every PR: implemented in an isolated CoW worktree, gated on a
full local `clippy + test --workspace` (with test-assets), an independent adversarial AI
code review (`fakoli-crew:critic`, all PASS), and green CI, then squash-merged.

| PR | What | Crate(s) |
|----|------|----------|
| #34 | AIR parser tolerates `[Begin Action N, label]`, `Clsn2Defaultf:` typo, trailing frame-col junk (`2..A`) | fp-formats |
| #35 | Ship **original clean-room** `assets/data/common1.cns`; loader falls back to it when `stcommon` resolves to a missing file (evilken) | fp-character |
| #36 | Parse the 2-arg MUGEN trigger forms `TimeMod=d,c` / `HitDefAttr=S,NA` / `Proj*=v,op t` (generalized the AnimElem comma-tail fold; `suppress_comma_tail` guard) | fp-vm, fp-character |
| #37 | **Black-screen fix**: default `active_palette` to the costume palette (pal1) when a char ships `.act` palettes — `LoadedCharacter::default_palette_index()` + `build_player` `or_else` | fp-character, fp-app |
| #38 | Downgrade SFF v1 shared-palette "reusing previous" log from `warn!`→`debug!` (was hundreds of lines/load) | fp-formats |

**End-to-end verified:** `cargo run -p fp-app -- validate test-assets/evilken/evilken.def`
now emits **0 warnings** (was a flood); evilken's pal1 (evilken.act) decodes to a colorful
costume (test asserts `rgb_sum > 10_000`), so it renders in color, not near-black.

### Root causes (for future reference)
- evilken is **SFF v1**, CvS/`.act`-costume style: indexed SFF whose embedded palette is a
  dark placeholder; real colors live in `pal1 = evilken.act` (MUGEN default costume = pal1).
  We defaulted `active_palette=None` → SFF-embedded → black. Fix #37 defaults to pal1.
- KFM/trainingdummy ship **no** `.act` files → `default_palette_index()==None` → unchanged
  (the #37 fix is surgical: only `.act`-shipping characters are affected).

## ⚠️ The orphaned-workflow saga (resolved, but read this)

A prior `Workflow` run ("Debug evilken: VM two-arg triggers…", a ~52-min dynamic workflow
dispatched **before a context compaction**) was still alive in the harness at session start.
It had left **uncommitted** edits in worktrees `fp-wt-{air,common1,triggers}`, and it kept
**re-creating** those worktrees (under new `task/*` branch names) every time I `git worktree
remove`'d them — a live, bursty writer (confirmed by a 100s hash-stability probe).

- I could **not** find its `wf_` run id to `TaskStop` it (the truncated id from the summary
  didn't match; no `wf_` id is persisted in readable task outputs).
- **Recovery strategy that worked:** review + build/test-gate each uncommitted diff, capture
  patches to `/tmp/fp-patches`, and **replay them into fresh hermetic landing worktrees** the
  orphan didn't know about. This decoupled correctness from the race entirely.
- The orphan's diffs were **90% correct but incomplete** in two spots I had to finish myself:
  (1) a non-exhaustive `match` in `proptest_fuzz.rs` (new `Expr` variants), and (2) a
  pre-existing `task_4_11` test in `cns_integration.rs` that still asserted the *old* contract
  (TimeMod must error) — Fix A intentionally reverses it (TimeMod now parses; a tail mid-`&&`
  chain folds). Lesson: **green "new tests" ≠ green workspace; only `cargo test --workspace`
  on a frozen tree is trustworthy.**
- The orphan **never committed/pushed/PR'd** (its task said not to), so it never touched
  merged `main`. It **completed on its own** mid-session (notification arrived) and stopped
  regenerating worktrees. Final sweep confirmed: no `task/*` branches, no junk PRs.

If a similar orphan appears again: detach (patch→fresh worktree), don't race it; it can't
corrupt merged `main` without a push, and dynamic workflows do eventually terminate.

## Invariants / gotchas (don't relearn)

- **`env -u GITHUB_TOKEN` on every `gh`/`git` network call** — uses the keyring `gho_` token,
  NOT the user's personal PAT. Permanent.
- Sibling-dir worktrees need `dangerouslyDisableSandbox: true` for cargo/git; CoW-clone
  (`cp -cR target`) + symlink `test-assets` per worktree for warm incremental builds.
- **Never add a field to `LoadedCharacter` / `CompiledState` / `AnimFrame`** — they're built
  via struct literals in 3 crates' tests, so a new field forces cross-crate churn. **Derive a
  method instead** (e.g. `default_palette_index()` is computed over the existing `palettes` Vec).
- Merge gate = local `clippy --workspace --all-targets -D warnings` + `test --workspace` WITH
  test-assets, then AI critic PASS, then CI green, then `gh pr merge --squash --delete-branch`
  (the `--delete-branch` warns when a worktree still holds the branch — harmless).
- `cargo test NAME1 NAME2` fails ("unexpected argument"); pass one filter or run the suite.
- `fp-app/main.rs` is the universal render bottleneck — sequence fp-app-heavy PRs.

## Next steps (forward-looking backlog, unchanged)

- Stage tiling/velocity/anim + camera vertical-follow render (fp-stage + fp-app).
- Motif polish: real portraits / VS-screen art / fight.def screenpack beyond the hand-rolled HUD.
- Audit item #39: team / turns / tag modes (large architectural change — deferred).
- M5 authoring "moat". See [roadmap.md](../roadmap.md) and [known-issues.md](../known-issues.md).
