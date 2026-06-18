# Known Issues & Limitations

An honest catalog of what does **not** work yet in Fighters Paradise, what the
gaps mean for you in practice, and where to look in the code if you want to fix
them. This is the companion to the [Roadmap](roadmap.md): the roadmap is the
plan, this is the current honest state.

## v1.0 follow-ups (non-blocking)

The v1.0 build (full front-end + directory discovery + HUD customization) is
feature-complete and green (~2644 tests as of the behavioral-test-harness run).
These rough edges were surfaced by the final-review pass and live validation;
none crash the engine:

- **`stages/` under a game-root directory argument is not yet auto-discovered.**
  Pointing `fp-app` at a game root resolves `chars/` and `data/` (motifs), but
  stage discovery still keys off the loaded motif's `select.def` directory — a
  `<root>/stages/` folder is not yet merged into stage-select on the directory
  route. (`crates/fp-app/src/main.rs` `discover_stages`.)
- **Bare no-id `Proj*` triggers** (`ProjHit` with no id, meaning "any of my
  projectiles") evaluate to 0; only `ProjHit<id>`/`ProjContact<id>` etc. are
  wired. Documented parity gap. (`crates/fp-character/src/lib.rs`
  `parse_proj_trigger`.)
- **Mid-match save-state restore drops cosmetic explods/projectiles.** These
  display-only entities are excluded from `MatchSnapshot` by design; record/replay
  reproduces a match from its start under a fixed seed, so determinism is
  unaffected — only an exposed mid-match save-state would notice.
- **`.snd` ADX entries are recognised-and-skipped, not decoded** (WAV/PCM only).
- No regression test yet covers cross-entity redirected assignment
  (`enemy, var(n) := v`) write timing.

Fighters Paradise is a **playable two-character fighter** today — real Kung Fu
Man data drives a keyboard-controlled P1 vs. a dummy P2 with life bars, KO/round
flow, throws, supers, hitpause, and i-frames. A large batch of audit fixes has
since landed: the **stage, screenpack, and storyboard crates have all graduated
from stubs**, SFF v1 palettes + SFF v2 PNG decode, the power-bar HUD, the RNG
seam, ~10 new state controllers (AssertSpecial, Width, the get-hit-vel family,
HitOverride, SprPriority, Pause/SuperPause, PalFX/AfterImage), RoundState/
GameTime/MatchOver triggers, priority/trade clash, a Clsn debug overlay, a
character-validator CLI + real CI gate, and the FNT/ACT parsers all merged. The
items below are the rough edges that **remain**. None of them crash the engine
(the
[never-crash discipline](../CLAUDE.md#error-philosophy-never-crash-on-bad-content)
holds throughout); they show up as the last presentation gaps, a few
deliberately-simplified mechanics, and the forward-looking scope (replay,
team/tag modes) that is genuinely not started.

Each issue carries the matching number from the ranked
[Faithfulness Audit](knowledge-base/08-faithfulness-audit.md), which has the full
priority context. Where the audit's inline ✅ markers disagreed with the code,
this document follows the **code**.

> **Status legend:** **Resolved** = the gap is closed (implemented + tested) ·
> **Partial** = works for the common case but mechanically incomplete (the caveat
> is spelled out) · **Missing** = no implementation, falls through to a safe
> default. (No crate is a **Stub** anymore — `fp-stage`, `fp-ui`, and
> `fp-storyboard` have all graduated.)

---

## Rendering & visuals

This used to be the area with the most visible gaps; most of it is now wired.
The stage, screenpack, and storyboard crates have graduated from stubs and SFF
v1/PNG sprites decode. What remains here is a short list of **Partials** —
content paths that render in the common case but defer specific sub-features
until a real Elecbyte fixture exists to tune against.

### Stage backgrounds: parallax renders, tile/anim deferred (#29) — Partial

- **What works:** `fp-stage` has graduated from a stub into an ~850-line crate
  with typed `[StageInfo]`/`[BGDef]`/`[BG]`/`[Camera]` parsing
  ([crates/fp-stage/src/lib.rs](../crates/fp-stage/src/lib.rs)), and `fp-app`
  renders the parallax background layers with a horizontal camera that follows
  the fighters (`world_to_screen_x` offsets by `camera_x`,
  [fp-app/src/main.rs:895](../crates/fp-app/src/main.rs#L895)).
- **Remaining fidelity gaps:** `tile = x,y`, per-layer scroll `velocity`,
  `mask`, and `type = anim` (animated AIR-driven elements) are **parsed into the
  typed model but not yet rendered**, and the camera's **vertical follow** /
  floor-tension fields are parsed but the camera only scrolls horizontally.
- **Asset note:** no real Elecbyte stage `.def` fixture ships (clean-room), so
  the parser + camera math are covered by synthetic-fixture tests; the default
  match still falls back to a flat clear color when no stage is supplied.

### HUD font rendering: FNT parser + glyph path done; legacy quad HUD still markerless (#30) — Partial

- **What works:** A MUGEN bitmap-font FNT v1 parser now exists
  ([fp-formats/src/fnt.rs](../crates/fp-formats/src/fnt.rs)), and `fp-render`
  has a `draw_text` / glyph draw path. The new **screenpack** HUD ([#31](#real-lifebars--fightdef-screenpack-31--partial))
  consumes it to render names, the round announcer, and the timer as real glyphs.
  The `.act` external-palette parser also landed
  ([fp-formats/src/act.rs](../crates/fp-formats/src/act.rs)).
- **Remaining gaps:** The **legacy hand-rolled quad HUD** (used when no
  `fight.def` is found) is *not* wired to `draw_text` — its KO / round-state
  readout is still a centered solid-color marker (yellow KO, green/red winner),
  with no timer or combo glyphs (`Hud`,
  [fp-app/src/main.rs](../crates/fp-app/src/main.rs)). FNT is also
  **asset-blocked**: no real `.fnt` fixture ships, so the parser is synthetic-
  tested only.
- **Workaround:** Supply a `fight.def` screenpack to get glyph text; otherwise
  read the round/KO state from the colored marker.

### Real lifebars / fight.def screenpack (#31) — Partial

- **What works:** `fp-ui` now parses a `fight.def` into a typed
  [`ScreenpackLayout`](../crates/fp-ui/src/screenpack.rs) and renders the life
  bars, power bars, names, round announcer, and timer from `fight.sff` + fonts
  via [`ScreenpackHud`](../crates/fp-ui/src/renderer.rs). `fp-app` loads a
  screenpack when a `fight.def` is found next to the P1 character (or via
  `FP_SCREENPACK`) and falls back to the hand-rolled quad HUD otherwise (no
  regression for the default match).
- **Remaining fidelity gaps** (acceptable first cut, called out so they aren't
  mistaken for full MUGEN fidelity):
  - **Only one background layer per bar.** `parse_lifebar_side` /
    `parse_powerbar_side` read just `p1.bg0` — MUGEN allows layered `p1.bg1`,
    `p1.bg2`, …, which are silently dropped.
  - **Font slots stop at the first gap.** `parse_fonts` collects `font0,
    font1, …` only while contiguous. A malformed `[Files]` with `font0,
    font1, font3` (no `font2`) drops `font3`, and any later `font = 3`
    reference then resolves to no font and its text is silently skipped (a
    `warn!` is logged, but there is no on-screen cue).
  - **Bar fill ignores the authored `range.x` pixel magnitude.** `bar_fill_uv`
    uses only the *sign* of `(x0, x1)` to pick left/right anchoring and scales
    the whole front-sprite width by the fraction; a screenpack whose front
    sprite width differs from its authored `range` span will mis-size the
    fill. Coordinate-accurate scaling is deferred until a real `fight.sff`
    fixture exists to tune against (clean-room / asset-blocked).
- **Asset note:** no `fight.def`/`fight.sff` ships (clean-room), so the parser
  and fill math are covered by synthetic-fixture unit tests; the GPU draw path
  is exercised only with a real screenpack linked locally.

### SFF v1 art now decodes with its inline palette (#25) — Resolved

- **What was wrong:** SFF v1 stores each sprite's 256-color palette inline (the
  768-byte trailing PCX VGA block), but the v1 loader used to decode pixel
  *indices* only and left `palettes` empty, so v1 art rendered invisible.
- **What's done now:** [fp-formats/src/sff/v1.rs](../crates/fp-formats/src/sff/v1.rs)
  extracts each 8-bit sprite's trailing 768-byte VGA palette (`0x0C` marker +
  256 RGB triplets) into the [`SffPalette`](../crates/fp-formats/src/sff/palette.rs)
  table; every data-owning sprite contributes its own palette (a linked/data-less
  sprite reuses the most recent real one), so WinMUGEN-era intro/ending/motif art
  now has colors to look up.
- **Note:** SFF v1 sprites are still cosmetically tagged `SpriteFormat::Png8`;
  this stays harmless because the v1 decode path short-circuits before the format
  match.

### SFF v2 PNG sprites now decode (#35) — Resolved

- **What was missing:** SFF v2 `Png8`/`Png24`/`Png32` payloads hit an explicit
  `FpError::Unsupported` stub; only uncompressed/RLE8/RLE5/LZ5 sprites decoded.
- **What's done now:** `decode_png`
  ([fp-formats/src/sff/compression.rs](../crates/fp-formats/src/sff/compression.rs))
  decodes embedded PNG datastreams via the `png` crate. Indexed PNG8 yields
  palette indices + a PLTE palette that flow through the 256-color indexed render
  path; truecolor PNG24/PNG32 yield flat RGBA (via the `decode_sprite_rgba`
  path), with a `MAX_PNG_PIXELS` guard against decompression bombs. Modern HD
  PNG-packed characters now load their sprites.

### Hit sparks: own-spark infrastructure only; KFM still shows none (#17) — Partial

- **What works now:** `fp-engine` has a real effect-entity path. On a connecting
  hit it classifies the attacker's `sparkno` via
  [`SparkSource`](../crates/fp-combat/src/lib.rs), and for an **attacker-own**
  spark it spawns an [`Effect`](../crates/fp-engine/src/lib.rs) at the contact
  anchor (`detect_hit_contact`'s overlap center), ticks it frame-by-frame against
  the attacker's own AIR, and drops it when its (bounded) lifetime expires;
  `fp-app` draws the live effects additively over the fighters
  ([draw_effects](../crates/fp-app/src/main.rs)).
- **What's still missing — and why KFM shows no spark:** A spark is spawned **only**
  for the attacker-own form (`SparkSource::Own`). Two things keep that path dark
  for conventional content:
  1. **No `fightfx.sff` is loaded.** A *common* `sparkno` (any non-negative value)
     resolves against the shared `fightfx` set, which the engine does not load yet,
     so it is a documented best-effort **skip** (the hit still lands; no effect).
  2. **The `S`-prefix is lost upstream.** MUGEN authors an own-spark as `Sxx`, but
     `fp-character`'s `parse_resource_id`
     ([executor.rs](../crates/fp-character/src/executor.rs)) strips the `S` and
     keeps a *positive* id, so an authored own-spark currently classifies as
     `Common`. The `Own` path is therefore reachable only by a *literal-negative*
     `sparkno`, which real content rarely uses.
- **Impact:** **Kung Fu Man (and any conventional character) renders no impact
  spark.** KFM's `sparkno` values are all common-`fightfx` (`0/1/2/3/40`, plus one
  `-1` "none") with zero own-sparks, so the default KFM-vs-KFM match spawns no
  effects. Hits still connect (damage, hitpause, sound all fire) — this is
  *own-spark infrastructure*, not a working KFM spark.
- **To finish it:** load a `fightfx.sff`/`fightfx.air` set and route `Common`
  sparks to it, and/or preserve the `S`-prefix in `parse_resource_id` so the
  authored own-spark form reaches `SparkSource::Own`.
- **Workaround:** None for the visual; the hit sound and life-bar drop confirm the
  hit landed.

### AfterImage / PalFX color effects: tint works, trail is approximated (#33) — Partial

- **What works:** The `PalFX`, `AfterImage`, and `AfterImageTime` controllers now
  have real dispatch arms
  ([fp-character/src/executor.rs](../crates/fp-character/src/executor.rs)). PalFX
  drives a **color-tint render uniform** in the palette shader
  ([palette.wgsl](../crates/fp-render/src/shaders/palette.wgsl)), so a flashing
  super tints correctly, and `fp-app` draws a fading AfterImage trail behind the
  fighter (`draw_afterimage_trail`,
  [fp-app/src/main.rs:931](../crates/fp-app/src/main.rs#L931)).
- **Remaining fidelity gaps:** the trail is a **motion-smear approximation** — it
  re-uses the *current* frame stepped back along the character's facing with
  decaying alpha, **not** a true frame-history ghost ring. PalFX's
  `sinadd`/`PalBright`/`PalContrast`/`Trans` and AfterImage's `TimeGap`/`FrameGap`
  are not modeled.
- **Workaround:** None needed — the visible tint and smear read correctly for the
  common super-flash case.

---

## Gameplay fidelity

The fight engine is faithful for the KFM feature set, and a large batch of the
mechanics that used to be unmodeled have landed. The items in this section are
now **Resolved** — they are kept here (briefly) as a record of what was closed,
with the remaining honest caveats called out where they exist.

### Global Pause / SuperPause freeze (#24) — Resolved

- **What's done:** `fp-engine` now owns a whole-match `Freeze` timer
  ([fp-engine/src/lib.rs:590](../crates/fp-engine/src/lib.rs#L590)) driven by the
  `Pause` / `SuperPause` controllers. While a freeze is active the frozen
  players' simulation **and** the round timer / `GameTime` are held; only the
  `SuperPause` triggerer (tracked via `FreezeExempt`) keeps ticking. A `Pause`
  freezes everyone. Super flashes and dramatic-freeze moments now stop the screen.

### RoundState / GameTime / MatchOver exposed to triggers (#21) — Resolved

- **What's done:** the coordinator now pushes a
  [`RoundView`](../crates/fp-character/src/lib.rs) (`round_state` / `game_time` /
  `match_over`) onto each character every tick via `set_round_view`, and the
  `RoundState`, `GameTime`, and `MatchOver` triggers read it
  ([fp-character/src/lib.rs:2332](../crates/fp-character/src/lib.rs#L2332)).
  Character logic that gates on round phase (intro/win poses) now reacts. A bare
  self-only `Character` with no coordinator still falls back to safe defaults.

### Width controller (#10) — Resolved

- **What's done:** the `Width` controller has a dispatch arm that applies a
  per-state push/collision width override on top of the static `[Size]` widths.

### AssertSpecial flags (#13) — Resolved

- **What's done:** `AssertSpecial` has a dispatch arm that sets the per-tick
  asserted flags (`NoWalk`, `NoAutoTurn`, `Intro`, …); they are consulted for the
  tick and cleared each frame, as MUGEN expects.

### Dropped Statedef headers: SprPriority, juggle, facep2, persist flags (#16) — Resolved

- **What's done:** `CompiledState::from_parsed` now keeps the `sprpriority`,
  `juggle`, `facep2`, `hitdefpersist`, and `movehitpersist` headers instead of
  dropping them. A `SprPriority` controller arm + sprite draw-order ordering and
  air-juggle limits are wired through.

### On-hit power gain / transfer (#18) — Resolved

- **What's done:** `resolve_attack` now applies the HitDef `getpower` /
  `givepower` on a connecting hit, so the super meter builds from landing **and**
  taking hits (in addition to explicit `PowerAdd`/`PowerSet`/`TargetPowerAdd`).

### Priority / trade clash resolution (#20) — Resolved

- **What's done:** `fp-combat` gained a pure `resolve_clash`, and `fp-engine`
  runs a reconciled pass over two simultaneous HitDefs so attacks that should
  clash/trade by priority do so instead of both landing blindly in sequence.

### Get-hit velocity controllers (#23) — Resolved

- **What's done:** `HitVelSet`, `HitFallSet`, `HitFallVel`, and `HitFallDamage`
  all have dispatch arms, and the HitDef now carries `fall.damage` /
  `fall.xvelocity`. Get-hit common states can fine-tune knockback/fall (basic
  knockback was already applied in `resolve_attack`).

### Power bar in the HUD (#26) — Resolved

- **What's done:** the engine `Player` exposes `power()` / `power_max()` and the
  HUD draws a **blue power bar** under each life bar (`draw_power_bar`,
  [fp-app/src/main.rs:813](../crates/fp-app/src/main.rs#L813)). The super meter is
  now visible.

### HitOverride controller (#9b) — Resolved

- **What's done:** an 8-slot `HitOverride` controller now exists alongside the
  already-working `NotHitBy` / `HitBy` i-frame windows (#9). (The audit's #9 ✅
  marker that once over-claimed this is now accurate.)

---

## Robustness & infrastructure

These don't affect the visible fight much today but matter for correctness,
testing, and the long-term customizable-engine vision.

### Input is sampled once per frame (#27) — Resolved

- **What's done:** the keyboard is now snapshotted **once per frame**, outside
  the fixed-timestep catch-up loop, and the snapshot is reused for every catch-up
  tick. A multi-tick frame after a hitch no longer over-counts a single press.

### RNG-in-state (#28) — Resolved

- **What's done:** `Character` now owns a Park–Miller RNG state (seeded from
  `DEFAULT_RNG_SEED`, re-seedable via `seed_rng`) and overrides the VM's
  `random()` seam ([fp-character/src/lib.rs:2414](../crates/fp-character/src/lib.rs#L2414)),
  so the `random` trigger returns a real value in `[0, 999]`. The state lives on
  the entity as a plain `i32` so it serializes for future replay/rollback (#38).
  Randomized AI/move selection now works and is deterministic for a fixed seed.

### fp-vm arithmetic NaN handling (#19) — Resolved

- **What's done:** `arith()` now funnels any NaN / non-finite result to `Bottom`
  ([fp-vm/src/eval.rs](../crates/fp-vm/src/eval.rs)), so no NaN can escape the
  evaluator into a public `Value`. The previously-open NaN follow-ups are closed.

### CI now exercises real content via the shipped training dummy (#36) — Resolved

- **What was wrong:** real-KFM tests were asset-gated and `test-assets/` is a
  gitignored local-only symlink with no CI fetch step, so the real-content
  regression net ran as green no-ops on CI.
- **What's done:** the repo now ships an **original, clean-room training-dummy
  character** (`assets/trainingdummy/`) and CI **loads, matches, and validates**
  it, so the loader → match → validator path is exercised on every CI run. (The
  real-KFM suite is still local-only by design; run `cargo test --workspace` with
  `test-assets` linked to exercise genuine KFM content too.)

### VM fuzz / property tests (#37) — Resolved

- **What's done:** `fp-vm` now has `proptest`-based property/fuzz tests across the
  lexer, parser, and evaluator, hardening the never-panic contract beyond the
  hardcoded adversarial smoke tests.

### Content import overlays are text-only (F034 T088) — By design

- **What it is:** `fp-app import --out <dir>` writes a repaired, **loadable**
  overlay of a character (`fp-app <dir>` discovers and runs it; see
  [content-guide.md §3.5](content-guide.md)). The overlay repairs **text** only —
  `CNS`/`CMD`/`AIR` are written as repaired copies; the binary assets
  (`SFF`/`SND`/`ACT`) are **reported, never modified**: the overlay `.def`
  references them at their original absolute paths.
- **Why:** binary repair (e.g. re-encoding an `.sff`, fabricating a missing
  sprite) is out of scope and would risk altering content the author shipped. A
  flagged missing/zero-dim sprite is surfaced in the report for a human to fix at
  the source, not auto-rewritten.
- **Clean-room rule (restated):** an overlay (and the `.fp-cache/` IR cache) is
  **derived, local-only engine output — never committed**. The write-guard
  *refuses* any output destination inside the tracked `assets/` tree
  (canonicalised + prefix-matched), and `*.imported/` + `.fp-cache/` are
  git-ignored. The clean-room license reminder prints on **every** import run. Only
  ship content you have the right to distribute.

### No replay / determinism / rollback (#38) — Missing

- **What's missing:** No state serialization (no `serde`/`bincode`), no replay
  capture, and no rollback netcode. The fixed-timestep, deterministic design is a
  prerequisite the engine already meets, but the save/restore machinery isn't
  built.
- **Impact:** No replays, no rollback-based netplay, no save states. The RNG
  groundwork is already in place — `Character`'s Park–Miller state is a plain,
  serializable `i32` carried on the entity (#28, Resolved) — so what's left is the
  save/restore + capture machinery itself.
- **Workaround:** None.

---

## Scope: team / turns / tag modes (#39)

Today Fighters Paradise is a **1v1 local match** built around the KFM feature
set. The multi-fighter **modes** are still forward-looking; some of their
*plumbing* (the `.act` and extended-AIR parsers) has landed parser-side.

| Status | Item | Notes |
|---|---|---|
| **Missing** | Team / Simul / Turns / Tag modes | Only one fighter per side; `Match` is strictly two-player. The big unimplemented chunk of #39. |
| **Missing** | Real second player or AI | P2 receives `MatchInput::none()` every tick — an idle dummy ([fp-app/src/main.rs](../crates/fp-app/src/main.rs)). |
| **Partial** | `.act` external palette files (#39a) | A parser now exists ([fp-formats/src/act.rs](../crates/fp-formats/src/act.rs)), but the result is **not yet consumed at runtime** — only in-SFF palettes drive rendering. |
| **Partial** | Extended AIR `scale`/`angle`/`Interpolate` (#39a) | Now **parsed** into the typed `Frame` model (`scale`, `angle`, `interpolate` fields, [fp-formats/src/air.rs](../crates/fp-formats/src/air.rs)); the renderer does **not yet apply** per-frame scale/rotation/interpolation. KFM uses none. |
| **Partial** | Intro / ending storyboard playback (#32) | `fp-storyboard` has graduated from parser-only to a real [`StoryboardPlayer`](../crates/fp-storyboard/src/player.rs) that ticks scenes, and `fp-app` overlays it during Intro/ending. **Not yet applied:** per-scene fadein/fadeout, per-scene `clearcolor`, and BGM; the intro's fixed-60-frame timer is not tied to storyboard length. |

The Clsn hitbox/hurtbox debug overlay (#34) is **done** — toggle it with **F1**
(it draws `fp-render` debug-box primitives over the active boxes).

---

## Smaller correctness notes

A few details that are easy to trip over but lower-impact:

- **Some GetHitVars stay at defaults.** `resolve_attack` populates 12 of 15
  GetHitVars; `GetHitVar(type)` (`hit_type`), `hitcount`, and `isbound` are
  **not** populated even though the HitDef carries the source data
  ([fp-character/src/combat.rs](../crates/fp-character/src/combat.rs)).
- **Friction has no explicit snap-to-zero line (#12).** The `friction.threshold`
  key is loaded and exposed via the `const()` seam, so KFM's own
  `abs(vel x) < Const(threshold)` check fires correctly, but there's no
  engine-side snap line. Functionally fine for KFM, mechanically incomplete.
- **The `:=` assignment operator is unsupported.** It lexes but has no AST node,
  so it surfaces as a recoverable parse error — the only deliberately
  unsupported real-content expression form
  ([fp-vm/src/parser.rs](../crates/fp-vm/src/parser.rs)).
- **The VM is a tree-walk evaluator, not a bytecode/stack VM.** The crate name
  and some docs describe "bytecode compiler + stack VM"; the implementation
  recurses directly over the AST. This is a naming/doc drift, not a functional
  bug — expression evaluation is correct and never panics.

---

## See also

- [Roadmap](roadmap.md) — the plan to close these gaps, prioritized.
- [MUGEN Compatibility](mugen-compatibility.md) — what MUGEN content works today,
  format by format.
- [Architecture](architecture.md) — the design these limitations sit within.
- [Faithfulness Audit](knowledge-base/08-faithfulness-audit.md) — the full ranked
  39-item gap map with priority reasoning.
- Root [README](../README.md) and [CLAUDE.md](../CLAUDE.md) for build/run.
