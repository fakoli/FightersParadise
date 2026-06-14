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

Fighters Paradise is **not** an early stub project. It is already a **playable two-character
fighter**: `fp-app` renders a two-player `fp_engine::Match` driven by *real* Kung Fu Man data
(.def/.sff/.air/.cmd/.cns/.snd), with a keyboard P1, a life HUD, KO/winner readout, and best-of-3
rounds. KFM's signature throw, supers (meter), hitpause, i-frames, hit reactions,
jump+airjump+land, and damage multipliers all work end to end. The workspace is 14 crates / ~59k LOC
with **~1,724 `#[test]` attributes** (full suite reported at 1,769 passing including doc-tests);
`cargo clippy --workspace --all-targets -- -D warnings` is clean and CI is green.

Two crates remain genuine 7-line stubs: **`fp-stage`** and **`fp-ui`**. Everything else is
implemented to varying depth. The gap between "plays KFM" and "runs *your* character" is what this
roadmap closes.

The work items are drawn from the [faithfulness audit](knowledge-base/08-faithfulness-audit.md),
which ranks 39 MUGEN-fidelity gaps by priority and effort. Each milestone below cites those item IDs
(e.g. `#24`) so you can trace a roadmap line back to its source analysis and its
[Known Issues](known-issues.md) entry. The audit markers themselves can lag the code — status here
reflects a verification pass against the source.

### Reading the priority/effort tags

| Tag | Meaning |
|-----|---------|
| **P0** | Blocks the vision directly — common content will not run correctly without it |
| **P1** | High value; visible to anyone loading non-KFM content |
| **P2** | Polish, robustness, or long-tail content support |
| **Effort** | `small` ≈ a few arms/fields · `medium` ≈ a subsystem touch · `large` ≈ new subsystem or cross-crate restructure |

---

## Milestone map at a glance

| Milestone | Theme | Headline outcome | Audit items |
|-----------|-------|------------------|-------------|
| **M1** | Core combat fidelity | A correct fight for arbitrary characters, not just KFM | #16, #20, #21, #23, #24, #13, #10, #17, #18 |
| **M2** | Complete the visual layer | Stages, real lifebars, text, legacy art, effects | #25, #29, #30, #31, #33, #34 |
| **M3** | Content pipeline & robustness | Load modern art, validate content, make CI real | #35, #39 (.act), #36, #37, plus a validator |
| **M4** | Determinism & modes | Reproducible matches, replays, teams, netplay base | #28, #38, #39 (modes) |
| **M5** | Authoring experience & moat | Tooling, hot-reload, docs, packaging | (new — the customizable-engine moat) |

The milestones are roughly ordered, but M2 and M3 can proceed in parallel once M1's mechanics are in
place — they touch mostly disjoint crates (`fp-stage`/`fp-ui`/`fp-render` vs. `fp-formats`/CI).

---

## M1 — Finish core combat fidelity

**Why this is first.** The engine already *plays* KFM, but KFM is the easy case: it never exercises
super-pause, priority clashes, sprite-layering, per-state push widths, or RoundState gating, and its
get-hit chain happens to work without the dedicated get-hit controllers. A real third-party character
will lean on all of these. Until M1 lands, "bring your own character" means "bring a character that
behaves like KFM." These are mostly missing **dispatch arms** in
`crates/fp-character/src/executor.rs` plus a few engine-level mechanics — high leverage, mostly
small/medium effort.

| Item | Audit | Pri | Effort | What it unblocks |
|------|-------|-----|--------|------------------|
| Pause / SuperPause global freeze | [#24](known-issues.md) | P0 | large | Super-move dramatic freeze; today supers fire with no freeze. Needs an engine-level timer in `fp-engine` that halts *both* players except the pausing one — `hitpause` is per-character only and does not cover this. |
| Dropped Statedef headers + SprPriority + juggle | [#16](known-issues.md) | P0 | large | Sprite layering (`sprpriority`), air-juggle limits (`juggle`), `facep2`/`hitdefpersist`/`movehitpersist`. All five headers are parsed by `fp-formats` (`cns.rs:79-90`) but dropped at compile (`loader.rs:334-375`); carry them into `CompiledState`, add a `SprPriority` dispatch arm, and add a juggle-points counter decremented in `resolve_attack`. |
| Priority / trade clash | [#20](known-issues.md) | P1 | large | Simultaneous hits should compare HitDef priority (Hit/Miss/Dodge). Today combat runs `P1→P2` then `P2→P1` as two independent passes, so both attacks always land. Requires restructuring into one reconciled pass. |
| Get-hit velocity/fall controllers | [#23](known-issues.md) | P1 | medium | `HitVelSet` / `HitFallSet` / `HitFallVel` / `HitFallDamage` — four missing arms. Basic knockback already applies in `resolve_attack`, so the struck character moves; these let authored get-hit states re-translate `GetHitVar`→velocity/fall correctly. |
| RoundState / GameTime / MatchOver triggers | [#21](known-issues.md) | P1 | medium | Characters gate intro-freeze and effects on `RoundState`; today these read 0 forever. The `RoundState` enum and `Match::round_state` already exist in `fp-engine`; thread them into the trigger context (cheap once `Match::tick` passes engine context). |
| Width controller | [#10](known-issues.md) | P1 | medium | Per-state push/collision width (crouch/attack/throw-bind). Needs a width-override field consulted by `resolve_push`; ties into throw correctness. |
| AssertSpecial flags | [#13](known-issues.md) | P1 | medium | `NoWalk` / `NoAutoTurn` / `Intro` per-tick flags. Add a per-tick flag set cleared each tick, consulted by movement/turn logic and the engine face heuristic. |
| Hit-spark rendering | [#17](known-issues.md) | P1 | large | `sparkno`/`sparkxy` are authored on nearly every attack but never spawned. `fp-combat` already computes a contact-center anchor and discards it. Needs an Explod/effect entity + render path (shared with M2's effect work). |
| On-hit power gain/transfer | [#18](known-issues.md) | P2 | medium | `getpower`/`givepower` secondary meter gain (with `getpower=0` suppression). Lower urgency because the primary meter path (`PowerAdd`/statedef `poweradd`) already works. |

**Already done in this area (do not re-do):** cross-entity eval keystone + redirects/P2Dist
(audit #1/#2), `SelfState` (#3), jump/run/airjump velocity consts (#4), `Const720p` (#5),
`AnimElemTime` (#6), `GetHitVar(animtype)` (#7), throws (#8), `NotHitBy`/`HitBy` i-frames (the
implemented half of #9), `VelMul` (#11), airjump+land (#14/#15), damage multipliers (#19),
`SelfAnimExist` (#22), plus super meter, hitpause, and best-of-3. Note **`HitOverride` (the other
half of #9) is still missing** — it has no dispatch arm anywhere.

> **M1 done when:** a non-KFM character with super moves, sprite-layered effects, per-state widths,
> and RoundState-gated intros plays correctly — supers freeze the screen, juggles end, and
> hits trade instead of double-connecting.

---

## M2 — Complete the visual layer

**Why.** Right now a match renders two fighters over a **flat clear color** with a HUD made of
**hand-rolled colored quads** — no stage, no lifebar art, no text, and legacy (WinMUGEN) sprite art
renders **invisible** because v1 palettes are never read. A customizable engine has to *show* the
content people bring, including the stage and screenpack they ship with it. This milestone is where
`fp-stage` and `fp-ui` graduate from stubs.

| Item | Audit | Pri | Effort | What it unblocks |
|------|-------|-----|--------|------------------|
| SFF v1 trailing palette extraction | [#25](known-issues.md) | P0 | medium | SFF v1 stores its 256-color palette inline (trailing 768-byte VGA palette); the v1 decoder reads only pixel indices, so `palettes` is empty and every v1 sprite renders colorless/invisible. This blocks intro/ending/motif art and most legacy content. Self-contained `fp-formats` fix. |
| Stage `.def` + backgrounds/parallax/camera | [#29](known-issues.md) | P0 | large | `fp-stage` is a 7-line stub: no `[BGDef]`/`[BG]`/`[Camera]`/`[StageInfo]`/delta/parallax parser exists. Build the typed stage model, the layered background renderer (with parallax delta), and camera tracking, then wire it into the `fp-app`/`fp-engine` render path. *Asset-blocked for full conformance* — no Elecbyte stage fixture exists in `test-assets`; the motif system backgrounds share the BG-element model and can partially exercise the parser. |
| FNT parser + text rendering | [#30](known-issues.md) | P0 | large | No `fnt.rs` module and no glyph/`draw_text` path exist; the HUD draws KO/round as solid quads. Add the FNT parser to `fp-formats` and a text-draw path to `fp-render`. Prerequisite for real lifebars. *Asset-blocked:* no `.fnt` fixture ships yet. |
| Real lifebars / `fight.def` screenpack | [#31](known-issues.md) | P1 | large | `fp-ui` is a 7-line stub; today's HUD is solid quads, not a `fight.def`/`fight.sff` screenpack. Build the screenpack model and renderer. Depends on FNT (#30) and SFF v1 (#25). Also surfaces the power bar ([#26](known-issues.md)) — expose `power()`/`power_max()` on the engine `Player` and draw the meter (small, complements the already-working meter). |
| PalFX / AfterImage + color-tint render | [#33](known-issues.md) | P2 | large | `AfterImage`/`AfterImageTime`/`PalFX` fall to no-op; `SpriteDrawParams` has no color tint and the shader has no PalFX uniform. Cosmetic but heavily used by flashy characters. |
| Clsn hitbox/hurtbox debug overlay | [#34](known-issues.md) | P2 | medium | `RenderFrame` has no line/box primitive; Clsn data exists but is never drawn. **High developer leverage** — a force-multiplier for validating every M1 combat/throw/width fix. Worth pulling *forward* alongside M1 in practice. |

> **M2 done when:** a character loads with its stage and screenpack, legacy v1 art shows in full
> color, timer/round/combo text renders as glyphs, and a debug overlay can draw collision boxes.

---

## M3 — Content pipeline & robustness

**Why.** "Bring your own character" only works if the engine can *ingest* the formats real authors
ship today — including modern HD PNG sprites — and if it tells authors *why* their content fails
instead of silently degrading. It also requires that our own regression net actually runs. This
milestone is the difference between an engine that runs *our* fixture and an engine that runs the
community's catalog.

| Item | Audit | Pri | Effort | What it unblocks |
|------|-------|-----|--------|------------------|
| PNG sprite decode (SFF v2 Png8/24/32) | [#35](known-issues.md) | P0 | medium | `decompress_png` is an explicit `FpError::Unsupported` stub; no `image`/`png` dependency is wired, so any v2 PNG sprite is invisible. Modern HD characters use PNG-in-SFF heavily. KFM is RLE/LZ5 so KFM is unaffected — this is purely about *other people's* characters. |
| `.act` palette parser + extended AIR | [#39](known-issues.md) | P2 | large | No `.act` parser exists (KFM uses embedded palettes); needed for content that ships external palette files and palette-swap rosters. Extended AIR (`scale`/`angle`/`Interpolate`) lines are silently dropped today. Tackle opportunistically as non-KFM content demands. |
| **Character validator / linter** | *(new)* | P0 | medium | A `fp-app`/CLI subcommand that loads a `.def` and reports problems an author can act on: missing sprites/animations, unresolved state references, expressions that failed to compile (currently a silent const-0 fallback), unsupported controllers that hit the no-op branch, and clean-room license reminders. **This is the single most important "bring your character in" enabler** — it turns the engine's never-crash discipline into actionable author feedback. Pairs directly with the [Content Guide](content-guide.md). |
| CI asset-gate fix (make the safety net real) | [#36](known-issues.md) | P0 | medium | `test-assets` is gitignored (real KFM is a local symlink) and CI has no fetch step, so **every real-content / regression test early-returns green as a no-op on CI** — only one synthetic test truly runs. This silently hides regressions in *every* fix in M1/M2. Provision an original conformance fixture (see below) and wire a CI restore step so the net is real. Outsized correctness value for the effort. |
| VM fuzz / property tests | [#37](known-issues.md) | P2 | medium | No `proptest`/`quickcheck`/`cargo-fuzz` anywhere; existing "fuzz" tests iterate hand-written literals and the real-KFM no-panic test is asset-gated (no-op on CI). The never-panic contract on adversarial community content is currently *unproven* against randomized input. Add a fuzz/property harness for the `fp-vm` lexer/parser/evaluator. |

### The conformance-fixture prerequisite (clean-room)

The CI fix (#36) and the validator both want a **legally shippable golden character** to test
against, because **KFM is an Elecbyte asset we cannot redistribute** (CC BY-NC; it lives only behind
the gitignored `test-assets` symlink and is never tracked — see the clean-room note in the
root [README](../README.md) and [05 — Reimplementation Roadmap](knowledge-base/05-reimplementation-roadmap.md#a-kfm-equivalent-conformance-fixture)).

Build an **original "training dummy"** with a full `.def/.sff/.air/.cmd/.cns/.snd` file set
exercising the engine's breadth (idle/walk/jump/crouch, a normal with a HitDef, a special via `.cmd`,
a couple of get-hit states, and — once M1 lands — a super and a throw). Ship it in `assets/`. This
gives us three things at once: a legally shippable default character, a CI golden fixture, and a
worked tutorial example for our own format (the role KFM plays for MUGEN).

> **M3 done when:** PNG-based characters load and render, `cargo run -p fp-app -- validate my.def`
> gives an author a useful report, the original fixture ships, and CI actually runs the real-content
> regression suite (no more green no-ops).

---

## M4 — Determinism & modes

**Why.** A long-lived community engine needs *reproducibility* (replays, training-mode rewind,
eventually rollback netplay) and the *match modes* real rosters expect (teams, turns, tag). These are
infrastructure investments rather than per-character fidelity fixes, so they come after the content
pipeline — but they are the foundation of the long-term moat.

| Item | Audit | Pri | Effort | What it unblocks |
|------|-------|-----|--------|------------------|
| RNG-in-state | [#28](known-issues.md) | P1 | small | `Character` does not override `EvalContext::random()`, so every `random` trigger reads the trait default `0` and probabilistic AI in the community long tail is silently deterministic-wrong. Add a Park-Miller RNG field *in rollback state* (so it serializes). KFM uses no `random`, so this is invisible for KFM but required for everything else — and a prerequisite for the replay harness. **Do this before #38.** |
| Replay / determinism / rollback + state serialization | [#38](known-issues.md) | P1 | large | No `serde`/`bincode` anywhere; only per-tick *input* snapshots and round-reset exist — there is no full-state `to_bytes`/`from_bytes` and no determinism/replay integration test. Add whole-`Match` serialization, a record/replay harness, and a determinism test. Depends on RNG-in-state (#28). The groundwork for rollback netplay. |
| Team / turns / tag modes | [#39](known-issues.md) | P2 | large | The engine is strictly 1v1 today (no simul/turns/tag). Generalize `Match` to multiple fighters per side. Content-neutral for KFM; schedule when supporting rosters that need it. |
| Netplay groundwork | *(new)* | P2 | large | Once deterministic serialization (#38) and input-history record/replay exist, lockstep/rollback netcode becomes tractable. Explicitly *groundwork* in this milestone — the determinism work is the hard prerequisite; the transport/sync layer is a later effort. |

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
| **Default original content** | P1 | medium | Ship original sprites/sounds/fonts/motif and the M3 conformance fixture as the out-of-box experience, so a fresh download is immediately playable *without* any copyrighted assets. |

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

If you want the **shortest path to "runs my character, not just KFM,"** the highest-leverage subset
across milestones is:

1. **M3 validator + CI asset-gate fix (#36)** — so you can *see* what breaks and trust the test net.
2. **M3 PNG decode (#35)** — so modern art loads at all.
3. **M2 SFF v1 palette (#25)** — so legacy art isn't invisible.
4. **M1 dropped headers/SprPriority/juggle (#16)** and **get-hit controllers (#23)** — so arbitrary
   characters fight correctly.
5. **M2 stage + screenpack (#29/#31)** — so the content actually shows up framed.

The large, audacious items (rollback netplay groundwork in M4, full authoring tooling in M5) are the
*moat* — sequence them after the engine reliably runs the community's existing catalog.

---

*See also:* [Known Issues](known-issues.md) · [MUGEN Compatibility](mugen-compatibility.md) ·
[Content Guide](content-guide.md) · [Architecture](architecture.md) ·
[Faithfulness Audit](knowledge-base/08-faithfulness-audit.md) · [README](../README.md)
