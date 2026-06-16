# Fighters Paradise — Vision Roadmap

> **Vision.** Grow Fighters Paradise from a *MUGEN-compatible engine* into a **full-featured,
> customizable fighting-game SYSTEM** — one that runs the breadth of community MUGEN content
> faithfully, *teaches* a player from masher to master, and has a clean path to evolve **beyond**
> MUGEN (study tools, netplay, an authoring moat). Every milestone is sequenced to serve two
> recurring acceptance tests: **"can someone bring their favorite MUGEN character in and have it
> work?"** and **"does a newcomer who installs this become *literate* in the genre without leaving
> the app?"**

This roadmap describes where the system goes **next**. For what is already true today, see the
[Architecture overview](architecture.md), the per-crate status in the root [README](../README.md),
and the honest [Known Issues](known-issues.md). For format coverage and authoring see
[MUGEN Compatibility](mugen-compatibility.md) and the [Content Guide](content-guide.md).

The work items below are grounded against the **real v1.0+ source as of 2026-06-16** (PRs #44–#99),
not the historical audit markers — several items the older audit listed as "Partial/Open" have
since shipped (called out in *Already shipped*). They draw on the
[faithfulness audit](knowledge-base/08-faithfulness-audit.md) and three planning deep-dives:
the [MUGEN-mechanics reference](knowledge-base/2026-06-16-mugen-mechanics-reference.md),
the [player-expectations product deep-dive](knowledge-base/2026-06-16-player-expectations-product-deep-dive.md),
and the [content-import pipeline design](knowledge-base/2026-06-16-content-import-pipeline.md).

---

## Already shipped (v1.0+) — do not re-propose

Fighters Paradise is **not** an early stub. It is a complete, playable fighting game: Title →
character select → stage select → fight, plus a Setup/Options screen with live key remapping and
HUD customization, all keyboard- or gamepad-navigable. Verified present in the source today:

- **Combat core:** throws, supers (meter), hitpause, i-frames, hit reactions, jump/airjump/land,
  damage multipliers, priority/trade **clash**, the get-hit-vel family, `HitOverride` (8-slot),
  juggle limits, `SprPriority`, per-state `Width`, `AssertSpecial`, Pause/SuperPause **global
  freeze**, on-hit power gain/transfer. (~40 controller dispatch arms.)
- **Helpers, projectiles, the Explod subsystem** (`Explod`/`ModifyExplod`/`RemoveExplod`), helper
  lifecycle + `DestroySelf`, `target`/`parent`/`root`/`helper`/`partner`/`playerid` redirects,
  per-projid `ProjContact/ProjHit/ProjGuarded(+Time)`.
- **Hit-spark rendering (#17 — resolved):** an original clean-room `fightfx.sff`/`fightfx.air`
  ships under `assets/data/`, is loaded per match, and renders **common** sparks for KFM/conventional
  content (not just attacker-own sparks).
- **VM:** the `:=` in-expression assignment operator (T036), `random` ∈ [0,999] from in-state RNG,
  `RoundState`/`GameTime`/`MatchOver`, `HitDefAttr`, `FrontEdgeDist`/`BackEdgeDist`/`ScreenPos`,
  `SelfAnimExist`, PalFX/AfterImage color-tint, arithmetic NaN→Bottom.
- **Input:** 60-frame ring buffer + command recognition incl. `~ / $ > +` and reversed diagonals
  (`FU/BU/FD/BD`); a serializable `InputBufferSnapshot`.
- **Determinism & replay (#38 — core resolved):** whole-`Match` `snapshot()`/`restore_snapshot`
  (bincode), character fingerprints, a serde **`ReplayLog` + `MatchRecorder` + `replay_match`** that
  reproduces a match **bit-for-bit**, proven by `tests/determinism.rs`.
- **Modes:** team **Simul/Turns** (`--simul`/`--turns`), a difficulty-laddered **CPU AI substrate**
  (`AiDifficulty`/`AiTuning`/`CpuAi`) driving P2, and a `CommandSource` seam to drive a fighter from
  an arbitrary input source.
- **Content ingestion:** SFF v1 (PCX + trailing palette) and SFF v2 (RLE8/RLE5/LZ5/raw + PNG8/24/32)
  both render in full color; AIR (incl. scale/angle/Interpolate parse), CMD, DEF, CNS, SND, FNT v1,
  ACT palette parsers; Shift-JIS CNS/CMD/.def decode; SFF v2 sub-header resilience; directory
  discovery (`chars/`, `stages/`, `data/<motif>/`); a `validate` linter CLI; CI gate over the shipped
  clean-room `assets/trainingdummy`.
- **Presentation:** typed stage `[BGDef]`/`[BG]`/`[Camera]` + parallax camera, typed `fight.def`
  screenpack + `ScreenpackHud` (life/power bars, names, timer, round announcer, **combo counter
  drawn**), storyboard intro/ending overlay, F1 Clsn debug overlay.

**Honest residual partials** (tracked in [Known Issues](known-issues.md), folded into the tracks
below where they matter): stage `tile`/`velocity`/`mask`/`type=anim` + vertical camera follow not
rendered; screenpack layered bg / `range.x` magnitude; AfterImage is a smear approximation
(`sinadd`/`TimeGap`/`PalBright` unmodeled); per-frame AIR scale/angle/Interpolate parsed-not-applied;
`.act` palette parsed-not-consumed-at-runtime; `.snd` ADX skip-not-decoded; `stages/` not
auto-discovered under a bare game-root arg.

---

## The system in two tracks (plus one enabler)

Everything ahead falls into two product tracks and one cross-cutting enabler:

- **Track A — Fidelity:** complete the *MUGEN mechanics* so arbitrary community content runs
  faithfully, not just the KFM/evilken feature set. (Mechanics reference is the source of truth.)
- **Track B — Experience:** serve the *player* — the masher → master learning curve (legibility →
  teachability → competition) and the game-feel that makes a fight read as a game, not a tech demo.
- **Enabler — Content-Import:** an offline preprocessing step that makes bring-your-own-content
  *clean, auditable, and fast to load* — the substrate both tracks lean on for real-content reach.

The tracks are largely **parallel** (they touch mostly disjoint crates: Fidelity in
`fp-character`/`fp-vm`/`fp-input`/`fp-combat`; Experience in `fp-app`/`fp-render`/`fp-ui`/`fp-engine`;
Import in `fp-app`/`fp-formats`). Sequencing below interleaves them by impact-per-effort.

---

## Milestone map

| Milestone | Track | Headline outcome | Feature(s) | Depends on |
|-----------|-------|------------------|-----------|------------|
| **M6 — AI identity & locomotion floor** | Fidelity | Every player knows if it is human/CPU-driven; characters that define only specials still move, guard, and round-init correctly. Closes the #1/#2 mechanics gaps + the self-AI trap. | F022, F023 | — |
| **M7 — See the Fight** | Experience | A learner sees which box hit them, reads their botched fireball, and reads a move's startup/active/recovery — in-app, on a real match. | F026 | — (F1 overlay + ring buffer exist) |
| **M8 — Input & trigger completeness** | Fidelity | Charge moves fire; the remaining read-triggers (edges/geometry/target/hit introspection) resolve to real values instead of 0. | F024, F025 | M6 (F023 for some triggers) |
| **M9 — The Lab** | Experience | Training mode from the menu: dummy control (stance/block/infinite), reset, and record→playback of a setup. | F027 | M7; `CommandSource`, `MatchSnapshot` (exist) |
| **M10 — Teach the player** | Experience | Menu-selectable CPU difficulty + teaching behaviors; an interactive tutorial/trials runner; in-app movelist. | F028, F029 | M9 |
| **M11 — Game-feel & study** | Experience | Readable risk/reward (hitstop scaling, comeback legibility, input leniency); a replay study UI (scrub + overlays) on the shipped replay core; rollback-ready audit. | F030, F031 | M7; replay core (exists) |
| **M12 — Projectile/hit polish & SuperPause fidelity** | Fidelity | Bare no-id `Proj*`, full GetHitVar set, SuperPause defence/invuln windows, last no-op controller. | F032, F033 | — |
| **M13 — Content-Import pipeline** | Enabler | An offline `import` step: an auditable repair report, a repaired text overlay, and a local-only IR cache so loads stop re-flooding warnings and start fast. | F034 | — (reuses existing tolerant parsers) |
| **M14 — Authoring moat** | Both | Hot-reload, sprite/Clsn/expression tooling, content-pack packaging, and a visually-complete original out-of-box experience. | (net-new, scoped later) | M13 |

**Recommended execution order (impact × dependency):**
**M6 → M7 → M8 → M9 → (M10 ∥ M11) → M12 → M13 → M14.**
M6 and M7 are the two cheapest, highest-leverage steps: M6 closes the top two ranked fidelity gaps
with mostly-field-plumbing, and M7 productizes substrate (box mapping, ring buffer) that already
exists. M13 (Import) is independent and can run in parallel with any later milestone once it starts.

---

## Track A — Fidelity (complete MUGEN mechanics)

> **Goal:** close every remaining MUGEN-mechanic gap surfaced by the
> [mechanics reference](knowledge-base/2026-06-16-mugen-mechanics-reference.md), verified by grep
> against the source — not the stale historical audit. Each feature ID maps to the
> PRD-appendable blocks in `.fakoli-state/roadmap-prd-expansion.md`.

### M6 — AI identity & the locomotion floor *(highest fidelity value)*
- **F022 — AI Identity & Self-AI Safety.** Wire a first-class "who drives this player" field
  (`ai_level` 0 = human, 1–8 = engine AI), plumb it from the match wiring and the CPU difficulty,
  implement the `AILevel` trigger, and audit var-init so a human can never satisfy a cheap-AI gate
  (the evilken `Var(30)=59` trap). *Closes mechanics-ref §4, the #1/#2 ranked gaps.*
- **F023 — Engine-Default Common States.** Author a complete clean-room `common1.cns` (movement
  0–106, guard 120–155, win/lose 170/175, intro 190/191, round-init 5900) so a character that
  defines only its specials inherits correct stand/walk/crouch/jump/guard/round-init — today only a
  synthesized `[Statedef -1]` carries locomotion. Plus the engine friction snap-to-zero stop-floor.
  *Closes mechanics-ref §1; known-issues #12.*

### M8 — Input & trigger completeness
- **F024 — Charge Command Motions.** Parse `~NN`/`/NN` hold-duration tokens and enforce the hold in
  the backward-scan matcher so `~60$B, F, x`-style charge moves fire instead of erroring the whole
  command to a const-0 fallback. Single shared charge timer (MUGEN's documented limitation).
  *Closes mechanics-ref §2.*
- **F025 — Trigger Coverage Completion.** Wire the missing read-triggers from data already in
  `EvalCtx`/`StageView`/targets: `GameWidth`/`GameHeight`/`LeftEdge`/`RightEdge`/`TopEdge`/
  `BottomEdge`; `NumTarget`/`HitVel`/`HitOverridden`; `TeamSide`/`PlayerIDExist`. (`HitDefAttr`,
  `FrontEdgeDist`, `BackEdgeDist`, `ScreenPos` already ship.) *Closes the trigger-coverage gap.*

### M12 — Projectile/hit polish & SuperPause fidelity
- **F032 — Projectile & Hit-State Completeness.** Bare no-id `Proj*` aggregation, `ProjCancelTime`/
  `NumProjID`, and the three unpopulated GetHitVars (`hit_type`/`hitcount`/`isbound`).
- **F033 — SuperPause Defence/Invuln Fidelity & Controller Polish.** SuperPause `p2defmul`/
  `unhittable` windows; implement the last no-op controller (`LifebarAction`).

---

## Track B — Experience (masher → master)

> **Goal:** serve the *ideal user* — Persona B ("The Learner"), then Persona C ("The Competitor") —
> per the [player-expectations deep-dive](knowledge-base/2026-06-16-player-expectations-product-deep-dive.md).
> The deep-dive's sequencing principle is **legibility → teachability → competition**: you cannot
> teach footsies to a player who cannot see hitboxes, and you cannot run a ladder for players who
> never onboarded.

### M7 — See the Fight *(legibility; highest impact-per-effort)*
- **F026 — Legibility Layer.** Productize the developer F1 overlay + input ring buffer + AIR frame
  data into a *player-facing* layer: color-coded hitbox/hurtbox view, on-screen input display (last
  N inputs + recognized command name), and computed **startup/active/recovery + on-block advantage**.
  The hard parts (box mapping, ring buffer, fixed tick) already exist; this is surfacing + small new
  computation. *Closes deep-dive gaps #2/#3/#4.*

### M9 — The Lab *(teachability foundation)*
- **F027 — Training Mode, Dummy Control, Record/Playback.** A real Training mode from the menu (not
  just the existing select-flow shortcut): `GameMode::Training` (no timeout / no auto-KO), a
  `DummyCommandSource` (Stand/Crouch/JumpLoop/BlockAll/BlockAfterFirst/CPU), infinite life/meter,
  reset-to-start, and **record→playback of a setup** built on the shipped snapshot/replay core.
  *Closes deep-dive gap #5; Tip 9.*

### M10 — Teach the player
- **F028 — Teaching CPU & Difficulty Ladder.** Wire the existing `AiDifficulty`/`AiTuning` substrate
  to a menu selector (Easy/Normal/Hard — today P2 is hardcoded Normal), then add teaching *behavior
  modes* (Pure Blocker, Reactive DP, Whiff Punisher) — the solo learner's foil. *Closes gap #15.*
- **F029 — Learn to Play: tutorial, trials, movelist.** An in-app movelist/character-info screen
  derived from `.cmd`; a data-driven interactive **tutorial/trials runner** (Block High/Low,
  Attack-Block-Throw, Fireball, Anti-air/DP, a BnB combo) — the strategic bet that turns a runtime
  into a *product* (be the teacher the genre lacks). *Closes gaps #1/#13.*

### M11 — Game-feel & study
- **F030 — Readable Risk/Reward & Momentum.** Hitstop that scales with hit strength, comeback/meter
  legibility (low-life / max-meter visual state), and input leniency (jump-buffer + a validated
  command-buffer window) — the feel knobs that live in the experience layer. (The screenpack
  `[Combo]` counter already draws.) *Closes deep-dive gaps #8/#9.*
- **F031 — Study & Compete.** A replay **study UI** — load a `ReplayLog`, scrub frame-by-frame with
  the M7 overlays toggled on — built on the *already-shipped* replay core; plus a rollback-ready
  snapshot **audit + harness** (save/advance/restore/re-advance byte-equality) as netplay groundwork.
  *Closes deep-dive gaps #7/#14; the remaining UI/audit half of #38.*

---

## Enabler — Content-Import / Preprocessing

### M13 — Content-Import pipeline
- **F034 — Content Import / Preprocessing Pipeline.** An offline `fp-app import` step that runs the
  *existing* tolerant load and emits (a) a structured **repair report** (human + stable JSON, three
  severity tiers), (b) a repaired **text overlay** (CNS/CMD/AIR/DEF; SFF/SND reported, never
  modified), and (c) a local-only, hash-keyed **IR cache** of the compiled state graph so cold loads
  skip re-parse/compile and the load-time warning flood stops recurring. It **never adds tolerance**
  — it calls the F018 tolerant parsers as the repair oracle. **Clean-room is a first-class acceptance
  criterion:** every writing path refuses to write under tracked `assets/`, writes only under a
  gitignored output/cache root, prints the license reminder, and is tested against
  synthetic/trainingdummy fixtures — never committed third-party content.
  *Closes the "content runs but floods warnings / no author-facing repair surface / cold load
  re-parses everything" gap;
  see [content-import-pipeline](knowledge-base/2026-06-16-content-import-pipeline.md).*

---

## M14 — Authoring moat *(where the system becomes a product)*

Net-new investments toward the customizable-engine goal (not in the fidelity audit). Scoped in
detail after M13 lands; sketched here so the vision is complete:

- **Hot-reload** — watch a character's files and reload sprites/animations/states live in a running
  match; turns edit→test from minutes into seconds. Builds on the validator + the import overlay.
- **Authoring tooling** — a sprite/Clsn editor and animation previewer (leveraging the M7 overlay),
  an expression-trigger REPL over `fp-vm`, and `new character` scaffolding templates.
- **Packaging & distribution** — reproducible per-platform binaries (SDL2 made turnkey), a
  content-pack/manifest format so authors can bundle and share characters/stages, and clean-room
  license guidance baked into the tooling so shared packs stay compliant.
- **Visually-complete original content** — round out `assets/trainingdummy/` into a full out-of-box
  experience (original stage, screenpack art, fonts, motif) so a fresh download is immediately
  playable and visually complete without any copyrighted assets.

> **M14 done when:** a newcomer can install, scaffold a character from a template, edit it with live
> hot-reload, validate it, package it, and share it — without ever touching another project's
> copyrighted files.

---

## Cross-cutting principles (every milestone)

- **Never crash on bad content.** Parsers warn-and-skip; bad expressions → const-0; missing sprites
  → invisible; unresolved redirects → 0. New surfaces (overlays, import, training) inherit this.
- **Clean-room stays airtight.** No Elecbyte/MUGEN engine source or copyrighted assets are ever
  shipped or tracked. KFM and community downloads remain local-only behind the gitignored
  `test-assets/` symlink. Import overlays and IR caches are *derived from third-party content → they
  are third-party content* — local-only, never committed. New default content is original/clean-room.
- **Determinism is cheaper baked in than retrofit.** The fixed-60Hz tick + snapshot/replay core is
  the foundation of replay study, rollback, and record/playback — keep every new tick-affecting
  feature inside the deterministic input layer so it reproduces.
- **Reuse the substrate; do not fork it.** Overlays read live `Match` state (so they work on replays
  for free); training dummies are a `CommandSource` impl (no executor change); import calls the
  tolerant parsers as its oracle; common states are CNS text the existing loader compiles.

---

*See also:* [Known Issues](known-issues.md) · [MUGEN Compatibility](mugen-compatibility.md) ·
[Content Guide](content-guide.md) · [Architecture](architecture.md) ·
[Faithfulness Audit](knowledge-base/08-faithfulness-audit.md) ·
[Mechanics Reference](knowledge-base/2026-06-16-mugen-mechanics-reference.md) ·
[Player-Expectations Deep-Dive](knowledge-base/2026-06-16-player-expectations-product-deep-dive.md) ·
[Content-Import Pipeline](knowledge-base/2026-06-16-content-import-pipeline.md) · [README](../README.md)
