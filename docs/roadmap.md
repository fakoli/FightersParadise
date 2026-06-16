# Fighters Paradise — Enhancement Roadmap

> **Vision.** Make Fighters Paradise a *completely customizable fighting-game engine*: bring your own
> characters, get full MUGEN-format support, and follow clear guidance on how to structure content.
> Everything below is sequenced to serve that goal — the recurring acceptance test for each milestone
> is **"can someone bring their favorite MUGEN character in and have it work?"**

This roadmap describes where the engine goes *next*. For what is already true today, see the
[Architecture overview](architecture.md), the per-crate status in the root [README](../README.md),
and the honest [Known Issues](known-issues.md) list. For format coverage and authoring see
[MUGEN Compatibility](mugen-compatibility.md) and the [Content Guide](content-guide.md).

## Where we start from

Fighters Paradise is **not** an early stub project. It is a **complete, playable fighting game**
(v1.0, 2026-06-15): `fp-app` renders a two-player `fp_engine::Match` driven by *real* Kung Fu Man
data (.def/.sff/.air/.cmd/.cns/.snd), with a Title screen → character select → stage select →
fight flow, a keyboard P1 (remappable), a life **and power** HUD, KO/winner readout, and
best-of-3 rounds. KFM's signature throw, supers (meter), hitpause, i-frames, hit reactions,
jump+airjump+land, and damage multipliers all work end to end. Characters walk, run, and jump
correctly — bare velocity consts resolve (PRs #98/#99). The workspace is 14 crates / ~59k LOC with
**~2,600 `#[test]` attributes** (full suite ~2,644 passing including doc-tests);
`cargo clippy --workspace --all-targets -- -D warnings` is clean and CI is green.

**Update (audit-P run + v1.0 + behavioral-test-harness run):** the 23-PR audit run closed the bulk
of the milestone work below. `fp-stage` and `fp-ui` have **graduated from stubs** (typed stage
`.def` + parallax render; typed `fight.def` screenpack + `ScreenpackHud`), `fp-storyboard` is no
longer parser-only (it has a `StoryboardPlayer` + intro/ending overlay), and the executor dispatch
chain now handles **~40 controllers** (was ~30). An **original clean-room training-dummy** character
ships in `assets/trainingdummy/`, and CI loads + matches + validates it on every push — the
real-content safety net is finally real. Bare velocity consts resolve (PRs #98/#99) so characters
walk, run, and jump correctly. A **GUI-free behavioral test harness** (motion synthesizer, range-
of-motion table, evilken move-execution) now asserts in-game behavior headlessly — no window
needed. What remains is M4 (determinism/replay #38, modes #39) and M5 (the authoring moat); see
those sections below.

The work items are drawn from the [faithfulness audit](knowledge-base/08-faithfulness-audit.md),
which ranks 39 MUGEN-fidelity gaps by priority and effort. Each milestone below cites those item IDs
(e.g. `#24`) so you can trace a roadmap line back to its source analysis and its
[Known Issues](known-issues.md) entry. The audit markers themselves can lag the code — status here
reflects a verification pass against the source. **As of this run, M1–M3 are substantially
complete** (a handful of honest Partials remain, flagged inline); the audit is effectively done
except for #38/#39 and the net-new M5 work.

### Reading the priority/effort tags

| Tag | Meaning |
|-----|---------|
| **P0** | Blocks the vision directly — common content will not run correctly without it |
| **P1** | High value; visible to anyone loading non-KFM content |
| **P2** | Polish, robustness, or long-tail content support |
| **Effort** | `small` ≈ a few arms/fields · `medium` ≈ a subsystem touch · `large` ≈ new subsystem or cross-crate restructure |

---

## Milestone map at a glance

| Milestone | Theme | Headline outcome | Audit items | Status |
|-----------|-------|------------------|-------------|--------|
| **M1** | Core combat fidelity | A correct fight for arbitrary characters, not just KFM | #16, #20, #21, #23, #24, #13, #10, #17, #18 | **Done** (spark render Partial — #17) |
| **M2** | Complete the visual layer | Stages, real lifebars, text, legacy art, effects | #25, #29, #30, #31, #33, #34 | **Substantially done** (render-fidelity Partials remain) |
| **M3** | Content pipeline & robustness | Load modern art, validate content, make CI real | #35, #39 (.act), #36, #37, plus a validator | **Done** (.act/AIR parse-only — #39a) |
| **M4** | Determinism & modes | Reproducible matches, replays, teams, netplay base | #28, #38, #39 (modes) | **Started** (#28 done; #38/#39 open) |
| **M5** | Authoring experience & moat | Tooling, hot-reload, docs, packaging | (new — the customizable-engine moat) | **Not started** (the focus now) |

The milestones are roughly ordered, but M2 and M3 can proceed in parallel once M1's mechanics are in
place — they touch mostly disjoint crates (`fp-stage`/`fp-ui`/`fp-render` vs. `fp-formats`/CI).
**With M1–M3 landed, the live frontier is M4 (#38 replay/determinism, #39 modes) and the net-new M5
authoring moat.**

---

## M1 — Finish core combat fidelity — **DONE**

**Why this was first.** The engine already *plays* KFM, but KFM is the easy case: it never exercises
super-pause, priority clashes, sprite-layering, per-state push widths, or RoundState gating, and its
get-hit chain happens to work without the dedicated get-hit controllers. A real third-party character
leans on all of these. These were mostly missing **dispatch arms** in
`crates/fp-character/src/executor.rs` plus a few engine-level mechanics — high leverage, mostly
small/medium effort. **All M1 items merged in the audit-P run.** The one residual is hit-spark
*rendering* against common-fightfx authors (#17), which has its effect-entity infrastructure but no
`fightfx.sff` loader yet — kept Partial below.

| Item | Audit | Pri | Effort | Status | What it unblocks |
|------|-------|-----|--------|--------|------------------|
| Pause / SuperPause global freeze | [#24](known-issues.md) | P0 | large | **Done** | Super-move dramatic freeze. An `fp-engine` freeze timer halts *both* players except the pausing one; the trigger clock keeps ticking and `GameTime` is held during the freeze. |
| Dropped Statedef headers + SprPriority + juggle | [#16](known-issues.md) | P0 | large | **Done** | Sprite layering (`sprpriority` header + `SprPriority` controller + sprite draw-order), air-juggle limits (`juggle`), `facep2`/`hitdefpersist`/`movehitpersist` — all five headers now carried into `CompiledState` and consumed. |
| Priority / trade clash | [#20](known-issues.md) | P1 | large | **Done** | Simultaneous hits compare HitDef priority via `fp-combat resolve_clash` + an `fp-engine` reconciled pass (Hit/Miss/Dodge), instead of the old two-independent-pass double-connect. |
| Get-hit velocity/fall controllers | [#23](known-issues.md) | P1 | medium | **Done** | `HitVelSet` / `HitFallSet` / `HitFallVel` / `HitFallDamage` arms (+ HitDef `fall.damage`/`fall.xvelocity`) re-translate `GetHitVar`→velocity/fall in authored get-hit states. |
| RoundState / GameTime / MatchOver triggers | [#21](known-issues.md) | P1 | medium | **Done** | Threaded from the engine into the trigger context via `RoundView`; these previously read 0 forever and now gate intro-freeze and effects correctly. |
| Width controller | [#10](known-issues.md) | P1 | medium | **Done** | Per-state push/collision width override (crouch/attack/throw-bind), consulted by the push resolver. |
| AssertSpecial flags | [#13](known-issues.md) | P1 | medium | **Done** | `NoWalk` / `NoAutoTurn` / `Intro` per-tick flags, set fresh each tick and consulted by movement/turn logic and the engine face heuristic. |
| Hit-spark rendering | [#17](known-issues.md) | P1 | large | **Partial** | Effect-entity *infrastructure* is in (`fp-engine` effect list: spawn-on-hit / tick / expire, `fp-app` render; own-spark path works + tested). **But KFM shows no visible spark:** KFM authors *common*-fightfx `sparkno` and **no `fightfx.sff` loader exists yet**, and the `S`-prefix own-spark form is flattened upstream (`parse_resource_id` strips `S`→positive id). |
| On-hit power gain/transfer | [#18](known-issues.md) | P2 | medium | **Done** | `getpower`/`givepower` secondary meter gain with `getpower=0` suppression (the primary `PowerAdd`/statedef-`poweradd` path already worked). |

**Already done before this run (do not re-do):** cross-entity eval keystone + redirects/P2Dist
(audit #1/#2), `SelfState` (#3), jump/run/airjump velocity consts (#4), `Const720p` (#5),
`AnimElemTime` (#6), `GetHitVar(animtype)` (#7), throws (#8), `NotHitBy`/`HitBy` i-frames + the now-
landed **`HitOverride`** 8-slot half of #9 (#9b), `VelMul` (#11), airjump+land (#14/#15), damage
multipliers (#19), `SelfAnimExist` (#22), plus super meter, hitpause, and best-of-3.

> **M1 done when (met):** a non-KFM character with super moves, sprite-layered effects, per-state
> widths, and RoundState-gated intros plays correctly — supers freeze the screen, juggles end, and
> hits trade instead of double-connecting. **All met**; the only residual is that common-fightfx
> *sparks* don't render until a `fightfx.sff` loader lands (#17 Partial).

---

## M2 — Complete the visual layer — **SUBSTANTIALLY DONE**

**Why.** A match used to render two fighters over a **flat clear color** with a HUD made of
**hand-rolled colored quads** — no stage, no lifebar art, no text, and legacy (WinMUGEN) sprite art
rendering **invisible** because v1 palettes were never read. A customizable engine has to *show* the
content people bring. **In the audit-P run `fp-stage` and `fp-ui` graduated from stubs** and the
remaining items landed. Several render-fidelity details are still Partial (flagged below): they are
*parsed/wired* but not yet fully drawn, and lack real Elecbyte fixtures to conform against.

| Item | Audit | Pri | Effort | Status | What it unblocks |
|------|-------|-----|--------|--------|------------------|
| SFF v1 trailing palette extraction | [#25](known-issues.md) | P0 | medium | **Done** | The v1 decoder now extracts the trailing 768-byte VGA palette, so v1 sprites render in color — unblocking intro/ending/motif art and legacy content. |
| Stage `.def` + backgrounds/parallax/camera | [#29](known-issues.md) | P0 | large | **Partial** | `fp-stage` **graduated from stub**: typed `[BGDef]`/`[BG]`/`[Camera]`/`[StageInfo]` parser + parallax-camera render wired into `fp-app`. **Caveat:** `tile`/`velocity`/`mask`/`type=anim` and camera vertical-follow are *parsed but not yet rendered*, and there is no real Elecbyte stage `.def` fixture to conform against. |
| FNT parser + text rendering | [#30](known-issues.md) | P0 | large | **Partial** | FNT v1 parser in `fp-formats` + a `draw_text`/glyph path in `fp-render`. **Caveat:** asset-blocked — no real `.fnt` fixture ships (synthetic-tested), and it is consumed by the screenpack, not yet by the legacy quad HUD. |
| Real lifebars / `fight.def` screenpack | [#31](known-issues.md) | P1 | large | **Partial** | `fp-ui` **graduated from stub**: typed `fight.def` model + parser + `ScreenpackHud` renderer with `fp-app` load/fallback. **Caveat:** `[Combo]`/`[Face]` are parsed but not drawn, only a single `bg0` layer renders, there is no `fight.def`/`fight.sff` fixture (synthetic-tested), and it falls back to the quad HUD when absent. The power bar ([#26](known-issues.md)) is **Done** — `Player::power()`/`power_max()` + a blue meter. |
| PalFX / AfterImage + color-tint render | [#33](known-issues.md) | P2 | large | **Partial** | `PalFX`/`AfterImage`/`AfterImageTime` controllers + a PalFX color-tint render uniform (`palette.wgsl`) + `fp-app` draw wiring. **Caveat:** the afterimage trail is a motion-smear *approximation* (no true frame-history ghost ring); `sinadd`/`TimeGap`/`FrameGap`/`Trans`/`PalBright`/`PalContrast` are not modeled. |
| Clsn hitbox/hurtbox debug overlay | [#34](known-issues.md) | P2 | medium | **Done** | F1-toggle Clsn overlay backed by a new `fp-render` debug-box primitive — a force-multiplier for validating every M1 combat/throw/width fix. |

> **M2 done when (substantially met):** a character loads with its stage and screenpack, legacy v1
> art shows in full color, timer/round/KO text renders as glyphs, and a debug overlay can draw
> collision boxes — **all working**. The remaining Partials are render-fidelity polish (stage
> tile/velocity/anim layers, screenpack `[Combo]`/`[Face]`, true AfterImage ghosts) and the missing
> real stage/`fight.def`/`.fnt` fixtures.

---

## M3 — Content pipeline & robustness — **DONE**

**Why.** "Bring your own character" only works if the engine can *ingest* the formats real authors
ship today — including modern HD PNG sprites — and if it tells authors *why* their content fails
instead of silently degrading. It also requires that our own regression net actually runs. This
milestone is the difference between an engine that runs *our* fixture and an engine that runs the
community's catalog. **All M3 items merged in the audit-P run**; the only residual is that the `.act`
+ extended-AIR work (#39a) is parser-only, with the runtime/team-mode side living in M4.

| Item | Audit | Pri | Effort | Status | What it unblocks |
|------|-------|-----|--------|--------|------------------|
| PNG sprite decode (SFF v2 Png8/24/32) | [#35](known-issues.md) | P0 | medium | **Done** | The `png` crate is wired in: `Png8` decodes to indices + PLTE, `Png24`/`Png32` to RGBA via `decode_sprite_rgba`. Modern HD PNG-in-SFF characters now render (KFM is RLE/LZ5, so KFM was unaffected). |
| `.act` palette parser + extended AIR | [#39](known-issues.md) | P2 | large | **Partial** | `.act` palette parser + extended AIR (`scale`/`angle`/`Interpolate`) parsing landed (#39a). **Caveat: parser side only** — `.act` runtime consumption and team/turns/tag *modes* (#39) remain unimplemented (forward-looking, M4). |
| **Character validator / linter** | *(new)* | P0 | medium | **Done** | `cargo run -p fp-app -- validate <file.def>` loads a `.def` and reports actionable author problems (missing sprites/animations, unresolved state refs, expressions that failed to compile, no-op controllers, clean-room reminders). Run against real KFM it surfaced **23 real problems**. Pairs directly with the [Content Guide](content-guide.md). |
| CI asset-gate fix (make the safety net real) | [#36](known-issues.md) | P0 | medium | **Done** | A real CI gate now **loads + matches + validates** the shipped original `assets/trainingdummy` character on every push, so the regression net actually executes instead of early-returning green no-ops. (Real KFM remains a local-only gitignored symlink, never tracked.) |
| VM fuzz / property tests | [#37](known-issues.md) | P2 | medium | **Done** | A `proptest` property/fuzz harness now exercises the `fp-vm` lexer/parser/evaluator, proving the never-panic contract against randomized input. (The related #19 follow-up — `fp-vm` arithmetic funnels NaN/non-finite to `Bottom`, never surfacing a public NaN — is also **closed**.) |

### The conformance-fixture prerequisite (clean-room) — **shipped**

The CI fix (#36) and the validator both needed a **legally shippable golden character** to test
against, because **KFM is an Elecbyte asset we cannot redistribute** (CC BY-NC; it lives only behind
the gitignored `test-assets` symlink and is never tracked — see the clean-room note in the
root [README](../README.md) and [05 — Reimplementation Roadmap](knowledge-base/05-reimplementation-roadmap.md#a-kfm-equivalent-conformance-fixture)).

This is **done**: an original clean-room **"training dummy"** ships in `assets/trainingdummy/` with a
full `.def/.sff/.air/.cmd/.cns/.snd` file set. It serves three roles at once — a legally shippable
default character, the CI golden fixture (loaded+matched+validated on every push), and a worked
tutorial example for our own format (the role KFM plays for MUGEN). The clean-room contract was
updated (in `CLAUDE.md`) to permit this original content.

> **M3 done when (met):** PNG-based characters load and render, `cargo run -p fp-app -- validate
> my.def` gives an author a useful report, the original fixture ships, and CI actually runs the
> real-content regression suite (no more green no-ops). **All met.**

---

## M4 — Determinism & modes — **THE FRONTIER (next up)**

**Why.** With M1–M3 landed, this is the live frontier. A long-lived community engine needs
*reproducibility* (replays, training-mode rewind, eventually rollback netplay) and the *match modes*
real rosters expect (teams, turns, tag). These are infrastructure investments rather than
per-character fidelity fixes — and they are the foundation of the long-term moat. **#28 (RNG-in-state)
is already done; #38 and #39 are the open headline work.**

| Item | Audit | Pri | Effort | Status | What it unblocks |
|------|-------|-----|--------|--------|------------------|
| RNG-in-state | [#28](known-issues.md) | P1 | small | **Done** | `Character` now seeds a Park-Miller RNG *in its state*, and the `random` trigger returns a real `[0,999]` instead of the trait-default `0`. This unblocks probabilistic AI in the community long tail and is the prerequisite for the replay harness. |
| Replay / determinism / rollback + state serialization | [#38](known-issues.md) | P1 | large | **Open** | Still no `serde`/`bincode` whole-state path; only per-tick *input* snapshots and round-reset exist — no full-state `to_bytes`/`from_bytes` and no determinism/replay integration test. Add whole-`Match` serialization, a record/replay harness, and a determinism test. RNG-in-state (#28) is now in place, so this is unblocked. The groundwork for rollback netplay. |
| Team / turns / tag modes | [#39](known-issues.md) | P2 | large | **Open** | The engine is strictly 1v1 today (no simul/turns/tag). Generalize `Match` to multiple fighters per side, and wire `.act` palette *runtime* consumption for palette-swap rosters (the #39a parser side is already done). Content-neutral for KFM. |
| Netplay groundwork | *(new)* | P2 | large | **Open** | Once deterministic serialization (#38) and input-history record/replay exist, lockstep/rollback netcode becomes tractable. Explicitly *groundwork* — the determinism work is the hard prerequisite; the transport/sync layer is a later effort. |

> **M4 done when:** a recorded match replays bit-for-bit, training mode can rewind, and 2v2/turns
> matches run — with a clear path to networked play.

---

## M5 — Authoring experience & moat

**Why.** This is where the vision becomes a *product*. Everything above makes the engine *run* other
people's content correctly; M5 makes it *pleasant to author for* — the durable differentiator for a
"bring your own everything" engine. None of these are in the 39-item faithfulness audit (that audit
is about MUGEN parity); they are net-new investments toward the customizable-engine goal.

| Item | Pri | Effort | What it delivers |
|------|-----|--------|------------------|
| **Hot-reload** | P1 | medium | Watch a character's files and reload sprites/animations/states live in a running match. Turns the edit→test loop from minutes into seconds — the biggest day-to-day quality-of-life win for authors. Builds on the validator (M3). |
| **Authoring tooling** | P1 | large | A sprite/Clsn editor and animation previewer (leveraging the M2 debug overlay), an expression-trigger REPL over `fp-vm`, and content scaffolding (`new character` templates). Lowers the barrier from "edit raw CNS by hand" to a guided workflow. |
| **Documentation** | P0 | medium | Keep the [Content Guide](content-guide.md), [MUGEN Compatibility](mugen-compatibility.md) matrix, and format specs (`docs/format-specs/`) current as M1–M4 land. The compatibility matrix in particular must track which controllers/triggers are supported, partial, or unimplemented — it is the contract authors rely on. |
| **Packaging & distribution** | P1 | medium | A reproducible install path (binaries per platform; the SDL2 dependency made turnkey), a content-pack/manifest format so authors can bundle and share characters/stages, and the clean-room license guidance baked into the tooling so shared packs stay compliant. |
| **Default original content** | P1 | medium | The M3 conformance fixture (`assets/trainingdummy/`) already ships as a clean-room default character. *Remaining:* round it out into a full out-of-box experience — original stage, screenpack art, fonts, and motif — so a fresh download is immediately playable and *visually complete* without any copyrighted assets. |

> **M5 done when:** a newcomer can install the engine, scaffold a character from a template, edit it
> with live hot-reload, validate it, package it, and share it — without ever touching another
> project's copyrighted files.

---

## Cross-cutting principles (every milestone)

- **Never crash on bad content.** Parsers warn-and-skip; expressions that fail to compile fall back
  to const-0; missing sprites render invisible. M3's validator *surfaces* these instead of letting
  them stay silent — it does not change the runtime contract.
- **Clean-room stays airtight.** No Elecbyte/MUGEN engine source or copyrighted assets are ever
  shipped or tracked. KFM remains a local-only gitignored symlink. The M3 conformance fixture and M5
  default content exist precisely so we never need to.
- **Determinism is cheaper to bake in than retrofit.** Where M1/M2 work touches RNG or evaluation
  order, prefer the rollback-safe option now (see M4 #28) rather than reworking it later.
- **Pull the debug overlay (#34) forward.** Though listed under M2, the Clsn overlay multiplies the
  velocity of every M1 combat fix — implement it early.

## Effort-honest sequencing

The **shortest path to "runs my character, not just KFM"** — validator + CI net (#36), PNG decode
(#35), SFF v1 palette (#25), dropped headers/SprPriority/juggle (#16) and get-hit controllers (#23),
stage + screenpack (#29/#31) — **has now landed** (the screenpack/stage pieces with the
render-fidelity Partials noted in M2). The engine reliably loads and fights arbitrary characters and
renders their stages/lifebars.

The remaining sequence is the *moat*:

1. **M4 #28 RNG-in-state** — *done*, and it unblocks the replay harness.
2. **M4 #38 replay/determinism/state serialization** — the next big lever: whole-`Match`
   `to_bytes`/`from_bytes`, a record/replay harness, and a determinism test. Groundwork for rollback
   netplay.
3. **M4 #39 team/turns/tag modes + `.act` runtime consumption** — generalize `Match` beyond 1v1.
4. **M5 authoring moat** — hot-reload, the sprite/Clsn/expression tooling (leveraging the now-landed
   #34 debug overlay), packaging, and rounding out the original default content.

Sequence these after the engine reliably runs the community's existing catalog — which it now does.

---

*See also:* [Known Issues](known-issues.md) · [MUGEN Compatibility](mugen-compatibility.md) ·
[Content Guide](content-guide.md) · [Architecture](architecture.md) ·
[Faithfulness Audit](knowledge-base/08-faithfulness-audit.md) · [README](../README.md)
