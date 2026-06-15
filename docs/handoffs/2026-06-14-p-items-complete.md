# Session Handoff — 2026-06-14 (P-items complete)

> Newest handoff = current pickup point (per [CLAUDE.md](../../CLAUDE.md) → "Session handoffs").
> This supersedes [2026-06-14-engine-merge-and-docs.md](./2026-06-14-engine-merge-and-docs.md).

## TL;DR

- **The entire 39-item faithfulness audit ("P-items") is now addressed on `main`** via **23 AI-reviewed,
  squash-merged PRs (#5–#23)** plus this docs-sync PR. Workspace tests went **~1769 → ~2045**, all green;
  `clippy --all-targets -D warnings` clean; CI green.
- **`fp-stage`, `fp-ui`, and `fp-storyboard` are no longer stubs.** `fp-stage` parses stage `.def` +
  renders parallax backgrounds; `fp-ui` is a `fight.def` screenpack model+renderer; `fp-storyboard`
  gained a `StoryboardPlayer` and intro/ending playback.
- **The CI safety net is now real (#36):** an **original, clean-room `assets/trainingdummy/` character**
  ships and CI loads + runs a match + validates it (no more all-green-no-op). A `fp-app -- validate <def>`
  linter exists (found 23 real problems in KFM).
- **Next = the forward-looking work** the audit deferred: **#38** replay/determinism/rollback + state
  serialization, **#39** team/turns/tag modes (+ `.act` runtime consumption), and the **M5 moat**
  (hot-reload, authoring tooling, packaging, default content).

## Repo state (verify with git/gh)

- `main` tip: this docs-sync PR, atop `0736ada` ("audit-P32 … (#23)").
- All feature branches/worktrees for the run were merged and deleted (remote + local).
- Merge cadence: no required reviews; CI = build+test+clippy on Ubuntu. **Real-content (KFM) tests are
  CI no-ops still for KFM** (it's gitignored) — but CI now *does* run real content against the shipped
  `trainingdummy`. The authoritative quality gate this run was the **local `cargo test --workspace` with
  `test-assets` linked**, not CI.

## What this session did (wave-by-wave)

Drove the audit to completion in conflict-aware parallel batches; each PR = implement (parallel agents)
→ dual adversarial review → auto fix-gate → local KFM-linked verify → PR → AI-review-in-body → squash-merge.

- **#25** SFFv1 palette, **#35** PNG decode, **#28** RNG-in-state, **#26** power-bar HUD, **#27** input
  timing, **#13** AssertSpecial, **#10** Width, **#23** get-hit vel/fall, **#9b** HitOverride, **#18**
  getpower/givepower, **#34** Clsn overlay, **#37** VM proptest fuzz, **#21** RoundState/GameTime/MatchOver,
  **#16** Statedef headers/SprPriority/juggle, **#20** priority/trade, **#24** SuperPause, **#36**
  validator+CI+fixture, **#19** fp-vm NaN→Bottom, **#30** FNT parser+text, **#29** stage, **#31** screenpack,
  **#33** PalFX/AfterImage, **#17** hit-spark effect entity, **#32** storyboard, **#39a** .act + extended AIR.

## Honest partials (do NOT assume these are fully done)

- **#17 sparks** — effect-entity infrastructure only. KFM authors common-fightfx `sparkno` and **no
  `fightfx.sff` loader exists**, so KFM shows no visible spark. The `S`-prefix own-spark form is flattened
  upstream (`fp-character::parse_resource_id` strips `S`→positive id). Own-spark path works + tested.
  Follow-ups: a fightfx `.sff/.air` loader; preserve the `S`-prefix sign in the parser.
- **#29 stage** — `tile`/`velocity`/`mask`/`type=anim` and camera vertical-follow are parsed but not
  rendered; no real Elecbyte stage fixture.
- **#31 screenpack** — `[Combo]`/`[Face]` parsed-not-drawn; single `bg0` layer; no `fight.def` fixture.
- **#33 PalFX** — afterimage trail is a motion-smear approximation (no frame-history ghost ring);
  `sinadd`/`TimeGap`/`Trans`/`PalBright`/`PalContrast` unmodeled. The per-draw PalFX uniform shares one
  buffer (mirrors the pre-existing vertex-buffer pattern) — worth a runtime spot-check that two characters
  tint independently.
- **#32 storyboard** — scene fade in/out + per-scene clearcolor + BGM not applied; the fixed 60-frame
  intro timer is not tied to storyboard length.
- **#30 FNT** — asset-blocked (synthetic-tested); consumed by the screenpack, not the legacy quad HUD.
- **#39a** — parser side only; team/turns/tag **modes** (#39) remain.

## Next concrete steps (forward-looking)

1. **#28→#38 replay/determinism:** add `serde`/`bincode` whole-`Match` `to_bytes`/`from_bytes`, an
   input record/replay harness, and a bit-for-bit determinism test. RNG-in-state (#28, the seed is plain
   `Cell<i32>`) is the prerequisite and is already done — wire `fp-engine` to seed P1/P2 distinctly.
2. **#39 modes:** generalize `Match` beyond strict 1v1 (simul/turns/tag); add `.act` *runtime* palette
   swaps (the parser exists).
3. **M5 moat:** hot-reload (watch a character's files), authoring tooling (leverage the #34 Clsn overlay +
   the validator), packaging, and shipping more original default content (build on `trainingdummy`).
4. **Cheap polish backlog:** wire a `fightfx.sff` loader so #17 sparks actually show for KFM; fix the
   `S`-prefix flattening; wire FNT into the legacy quad HUD; the stage tile/anim render.

## Invariants & orchestration gotchas (don't relearn the hard way)

- **Clean-room:** Elecbyte/MUGEN content is never tracked. The ONLY tracked content is the project's own
  originals: `assets/banner.png` + `assets/trainingdummy/*` (`.sff`/`.snd` are gitignored globally except
  those paths). KFM stays local-only behind the gitignored `test-assets` symlink.
- **Worktree/sandbox:** feature worktrees live in the sibling dir `/Users/.../fp-wt-*`. cargo/git there
  need the sandbox disabled (writes outside the project root otherwise fail with bogus "command not
  found"). Each worktree gets an APFS CoW clone (`cp -cR`) of `main/target` for a warm cache + a
  `test-assets` symlink. zsh: never name a shell var `path`; cwd resets between shell calls.
- **GitHub:** strip the personal PAT on every call — `env -u GITHUB_TOKEN gh|git …` (uses the keyring
  `gho_` token). Repo auto-merge is flaky (`gh pr merge --auto` often errors "not allowed" on the last
  call of a batch); a CI-poll loop merges stragglers with `gh pr merge --squash` once CI is SUCCESS.
- **Parallelism rule learned:** never run **two public-struct-extending** PRs in the same parallel batch —
  adding a field to a struct built with exhaustive literals in *other crates'* tests forces cross-crate
  `..Default::default()` edits and a guaranteed conflict (hit on `CompiledState`/`AnimFrame`). New fields
  should carry a `Default` and be constructed via `..Default::default()`.
- **Never panic on bad content** holds throughout; parse failures fall back to const-0; the validator
  surfaces them instead of changing the runtime contract.
