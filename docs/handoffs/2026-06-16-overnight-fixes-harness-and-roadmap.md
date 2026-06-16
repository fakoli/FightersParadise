# Session Handoff — 2026-06-16 — movement fix, GUI-free test harness, knowledge base, vision roadmap

> Newest handoff = source of truth. This run was driven autonomously with delegation to
> subagents/workflows; the main session was used for coordination, diagnosis, and operator interaction.

## Repo state (verify with git/gh)

- **`main` tip:** `39939f0` (vision roadmap + task backlog, PR #111). Last *code* gate: `ded2715`
  (evilken redo) = **2648 tests pass**; docs commits since don't change tests. clippy `-D warnings` +
  `cargo fmt --check` clean (CI gates).
- **Open PRs:** none. All work merged (PRs ~#76–#111).
- **fakoli-state:** PRD approved; 51 tasks in the DB (F001–F021 / T001–T051), all done. The NEW backlog
  (37 tasks, F022–F034 / T052–T088) is **documented in `docs/roadmap-task-backlog.md` but NOT yet loaded
  into the DB** — see "Next steps."

## What this session shipped

**The movement bug (the user's #1 complaint) — root-caused, fixed, live-validated.** It was never the
keyboard: input reached the engine, but the bare MUGEN velocity consts (`const(velocity.walk.fwd)`, and
`velocity.jump.*.y`) resolved to **0** (only the `.x`-suffixed forms were matched), so characters entered
walk/jump states with zero velocity. Fixed in PRs #98 (walk/run) + #99 (jump), each with a behavioral
regression test; **live-validated on screen** (real CGEvent key-holds + screencapture: P1 walks both ways).

**GUI-free behavioral test harness (the consistent, computer-use-free testing the user asked for):**
- `fp_input::synth::synth_command` — motion synthesizer, self-validated by replaying through the real
  `CommandMatcher` (PR #100).
- Range-of-motion table on the trainingdummy (PR #102).
- Asset-gated evilken **move-execution** test — proves specials + a power-gated super fire, with a
  negative control (PR #103).

**Other landed work:** trainingdummy QCF/DP specials (#97); evilken live-bug fix (stuck-punch =
finite-looping-anim `AnimTime=0` never observable; CNS `[State N: Label]` colon-header recovery; load
warn-flood silenced) — first attempt (#107) **regressed jump-start and was reverted (#108)**, then re-done
correctly (#110) with the root cause nailed (stale forced-`0` anim_time leaking into the next state's
first-tick trigger → fixed by re-seeding `anim_time` on state entry; jump-start 3-tick guard test added).
Docs de-staled (#104/#105/#106): test counts → 2644-era, behavioral harness documented. Ledger
task-authoring standard codified in CLAUDE.md (#109).

**Content + reference assets (local-only, never committed):**
- `test-assets/community/` grew to ~22 folders (15 varied characters + KOF98/SF3 stage packs) via a Chrome
  subagent using the operator's logged-in session; RARs extracted with The Unarchiver (`unar`).
- Ikemen GO downloaded to `~/ikemen-go-oracle` as a **behavioral-only** black-box oracle (GPL — never
  read into our code). ⚠️ Unsigned: to run it the operator must de-quarantine
  (`xattr -dr com.apple.quarantine ~/ikemen-go-oracle`).

**Knowledge base (5 docs in `docs/knowledge-base/`, for future planning):** behavioral-test-harness
design; MUGEN mechanics reference (clean-room, public spec); player-expectations product deep-dive
(masher→master primer); content-import-pipeline design; game-engines-for-2d-fighting paper notes.

**Vision roadmap + backlog:** `docs/roadmap.md` rewritten as a two-track vision (Fidelity = complete MUGEN
mechanics; Experience = masher→master player needs; + Content-Import enabler), milestones M6–M14.
`docs/roadmap-task-backlog.md` = 37 ledger-standard tasks (F022–F034 / T052–T088) with implementation
guides/pseudocode + per-mechanic verification + reference pointers.

## Next steps (for review, then execute)

1. **Review** `docs/roadmap.md` + `docs/roadmap-task-backlog.md`. Recommended milestone order:
   M6 (AI identity + locomotion floor) → M7 (legibility) → M8 (input/trigger completeness) →
   M9 (training/Lab) → (M10 ∥ M11) → M12 → M13 (content-import) → M14.
2. **Load the backlog into the fakoli-state DB** (deliberately deferred for your review): reformat
   `roadmap-task-backlog.md` from its `### Fxxx`/`#### Txxx` doc shape into the PRD parser's
   `## Features`(`### Fxxx:`) + `## Tasks`(`### Txxx:`) structure, append to `.fakoli-state/prd.md`, then
   `parse → plan → score → review_prd --approve → review tasks`. The source draft is also at
   `.fakoli-state/roadmap-prd-expansion.md`.
3. **Top-priority fidelity tasks** (already documented as F022/F023, surfaced by the evilken hunt):
   wire `AILevel` (0=human/1–8=AI; closes the self-AI-hijack class), and complete the **engine-default
   `common1.cns` movement states** (0/20/40/50/crouch) so non-self-contained characters (like evilken)
   can walk/jump.

## Invariants / gotchas (don't relearn)

- **The full-workspace integration gate is mandatory after every wave.** A *general* engine change
  (T056's anim-timing fix) passed its crate-scoped tests but broke an `fp-app` test it didn't run; the
  gate caught it. Agents touching shared engine behavior must run `cargo test -p fp-app` too.
- **Don't paper over a red gate by loosening a test** — investigate whether it's a real regression
  (the anim fix was; we reverted + re-did it correctly rather than relax the jump-start assertion).
- **Workflow review agents need Bash** to `gh pr diff` the branch (the no-Bash `fakoli-crew:critic`
  peer reviewer reviewed stale trees); both reviewers are now full-tools with distinct lenses.
- **Ledger task standard** (CLAUDE.md): every task carries full fields + implementation *guides*
  (approach + pseudocode + gotchas, not finished code) + verification that exercises the specific
  mechanic. Apply to every task, every time.
- **Clean-room held throughout:** community downloads + Ikemen GO are local-only/gitignored; only the
  originals (`assets/trainingdummy`, `assets/data`, dojo bg) are tracked.
- **Reusable workflows** under `.fakoli-state/`: `wave-workflow.js` (implement→dual-review→fix→PR, takes
  per-task `model`/`reviewModel`/`peerModel`), `roadmap-synthesis.js`, `final-review.js`. Sync after
  merges with `git fetch origin main && git reset --hard origin/main` (untracked survives).
