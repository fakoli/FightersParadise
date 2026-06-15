# Session Handoff — 2026-06-15 (forward-looking phase: polish, controller, menu)

> Newest handoff = current pickup point (per [CLAUDE.md](../../CLAUDE.md) → "Session handoffs").
> Supersedes [2026-06-14-p-items-complete.md](./2026-06-14-p-items-complete.md).

## TL;DR

Fighters Paradise is now a **complete, navigable, clean-room fighting game**. This run merged
**PRs #5–#32** (all AI-reviewed, no human review, zero failures landed). Phase 1 closed the entire
39-item faithfulness audit + replay/determinism; Phase 2 added the forward-looking layer the user
asked for: **visible polish, gamepad support, real-character robustness, and a full menu/screen
system**. `cargo run -p fp-app` boots a Title menu → character-select → fight → repeat, **playable
out-of-box clean-room over the shipped `trainingdummy` roster — no external content required**.

- `main` tip: `62eaab9` ("FL menu(2/2) … (#32)"). Tests ~2100+; `clippy -D warnings` clean; CI green.
- No crate is a stub. Shipped originals: `assets/banner.png`, `assets/trainingdummy/*`,
  `assets/data/{fightfx.sff,fightfx.air,font.fnt,system.def,select.def}`.

## What Phase 1 delivered (audit + M4 #38) — see the prior handoff for detail
All 39 audit P-items (#5–#23), docs sync + handoff (#24), and **#38 replay/determinism** (#25):
whole-`Match` snapshot/restore, input record/replay (byte-equal), a determinism test, distinct
P1/P2 RNG seeds, and a `CharacterFingerprint` guard against cross-`.def` restores.

## What Phase 2 (forward-looking) delivered

| PR | What |
|----|------|
| #26 | **Real-character parser robustness** — CNS `[Statedef N, label]` now parses (was dropped); fp-vm `target,`/redirect-prefixed trigger now parses as RHS of `&&`/`||`/relational. Found by validating **evilken/CVTW2RYU**. |
| #27 | **SDL2 game-controller support** — pure `fp-input` mapping + `fp-app` GameController, hotplug, keyboard-or-pad P1, pad-1 P2. `Start` is a no-op into `MatchInput` (engine has no pause path). |
| #28 | **`.act` runtime palettes** — loader reads `[Files] pal1..12`; `PaletteTexture::from_override`. (Render-wire landed in #30.) |
| #29 | **Hit-sparks render** — ships an original `fightfx.sff`+`.air`; fixes the `S`-prefix own/common spark distinction; routes common→fightfx, own→char SFF. A default KFM match now shows a spark per connect. |
| #30 | **Real HUD text** — ships an original `font.fnt`; HUD draws round timer + `ROUND N`/`KO`/`P1 WINS`/`DRAW` (quad fallback if font missing). Wires the `.act` override into `draw_player` (`--p1-pal`/`--p2-pal`). |
| #31 | **Menu parsers** — `fp-ui` `SystemDef`/`SelectDef` + a clean-room default motif (`assets/data/system.def`+`select.def`, trainingdummy roster). |
| #32 | **Menu state machine** — `fp-app` Title→Select→Fight→Title; no-args boots the menu (replacing the old local-KFM default); CLI (`p1.def`/viewer/`validate`) preserved; never-panic graceful degrade. |

## Deferred-polish backlog (honest follow-ups, in rough priority)

**Presentation (the obvious "make it look finished" next step):**
- Stage: `tile`/`velocity`/`type=anim` + camera **vertical-follow** are parsed but **not rendered** (#29).
- Menu: text-only on a solid bg — **motif background art, character portraits, the VS-screen draw**
  (geometry is parsed), and a brief **KO results pause** before returning to Title.
- Screenpack: `[Combo]`/`[Face]` parsed-not-drawn; single `bg0` layer; no `fight.def` fixture (#31).
- PalFX afterimage is a **motion-smear approximation** (no frame-history ghost ring); `sinadd`/`TimeGap`/
  `Trans`/`PalBright`/`PalContrast` unmodeled (#33).
- Storyboard: scene fade in/out + per-scene clearcolor + BGM not applied; the fixed-60-frame intro
  timer is not tied to storyboard length (#32-audit).
- Guard sparks: the fightfx asset ships a guard spark (group 120) but it is **unwired** (the engine only
  spawns from `sparkno` on a *connecting* hit — guard sparks need a blocked-hit path).

**Engine/correctness follow-ups:**
- `Start` button → a real **pause** path in `fp-engine` (controller Start is currently a no-op).
- `target,`/`Target,` redirects **parse** but **eval 0** — needs the target/helper graph (the `Target*`
  throw controllers exist; a general target/helper redirect graph does not).
- `S0` own-spark sign-encoding degrades to common-0 (a tagged enum would be fully faithful).
- CNS: a malformed `[State 2OS]` controller header lets its body leak into the open statedef
  (pre-existing `cns.rs` nuance — flagged in #26 review). Consider an i64 statedef number too.
- `.act` render-wire is verified at the seam but no `.act` ships on the default roster (evilken local-only)
  — a visible costume swap needs a roster char with `.act` files.
- **Public-struct-extension ripple:** `CompiledState`/`AnimFrame`/`LoadedCharacter`/`HitDef` gained
  non-`Default` fields this run, forcing `..Default::default()` edits in other crates' test literals.
  Deriving `Default` on these would stop the ripple. (See the orchestration note below.)

**Big remaining forward-looking items (not started):**
- **#39 team/turns/tag modes** — generalize `Match` beyond strict 1v1. Large architectural change.
- **M5 moat** — hot-reload (live-edit a character mid-match), authoring tooling (a sprite/Clsn editor
  on the #34 debug overlay, an `fp-vm` expression REPL, content scaffolding), packaging, more original
  default content.

## How to pick up / play-test

```sh
cargo run -p fp-app                 # Title menu over the shipped trainingdummy roster (no KFM needed)
cargo run -p fp-app -- --p1-pal 2   # (with a .act-carrying char, e.g. local evilken) alternate costume
cargo run -p fp-app -- test-assets/kfm/kfm.def      # direct two-KFM match (skips the menu)
cargo run -p fp-app -- validate <some.def>          # the character linter (found 23 real KFM problems)
cargo test --workspace              # full suite; clippy --workspace --all-targets -- -D warnings clean
```
A connected gamepad just works (dpad/stick + A/B/X/Y + LB/RB; SF-style 6-button).

## Orchestration notes (for a fresh session continuing the loop)
- Each feature was built in a sibling-dir git worktree with an APFS CoW clone of `main/target` + a
  `test-assets` symlink; cargo/git there need `dangerouslyDisableSandbox` (sandbox blocks writes outside
  the project root). `env -u GITHUB_TOKEN` on every `gh`/networked-`git` (use the keyring login, not the PAT).
- zsh: never name a shell var `path`. cwd resets between Bash calls. Auto-merge is flaky → a CI-poll loop
  merges stragglers with `gh pr merge --squash` once CI is SUCCESS. Merge gate = **local** `cargo test
  --workspace` with `test-assets` linked (CI is real-content-blind except the shipped trainingdummy).
- **Don't run two public-struct-extending PRs in one parallel batch** (forced cross-crate `..Default::
  default()` test edits collide). `fp-app/main.rs` is the universal render bottleneck → sequence
  fp-app-heavy PRs or region-partition + rebase.
- Original synthetic assets (trainingdummy, fightfx, font, default motif) are the clean-room pattern for
  shipping content — the only ASCII in a synthesized `.sff`/`.fnt` is its `Elecbyte*` format magic.
