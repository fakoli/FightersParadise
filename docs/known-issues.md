# Known Issues & Limitations

An honest catalog of what does **not** work yet in Fighters Paradise, what the
gaps mean for you in practice, and where to look in the code if you want to fix
them. This is the companion to the [Roadmap](roadmap.md): the roadmap is the
plan, this is the current honest state.

Fighters Paradise is a **playable two-character fighter** today — real Kung Fu
Man data drives a keyboard-controlled P1 vs. a dummy P2 with life bars, KO/round
flow, throws, supers, hitpause, and i-frames. The items below are the rough
edges that remain. None of them crash the engine (the
[never-crash discipline](../CLAUDE.md#error-philosophy-never-crash-on-bad-content)
holds throughout); they show up as missing visuals, unmodeled mechanics, or
infra gaps.

Each issue carries the matching number from the ranked
[Faithfulness Audit](knowledge-base/08-faithfulness-audit.md), which has the full
priority context. Where the audit's inline ✅ markers disagreed with the code,
this document follows the **code**.

> **Status legend:** **Stub** = crate/module exists but is empty or doc-only ·
> **Missing** = no implementation, falls through to a safe default · **Partial**
> = works for the common case but mechanically incomplete.

---

## Rendering & visuals

What the player sees is the area with the most visible gaps. Two whole crates
are still stubs, and several content-rendering paths are unwired.

### Stage backgrounds are not rendered (#29) — Stub

- **What's missing:** `fp-stage` is a 7-line doc-only stub. There is no
  `[BGDef]`/`[BG]`/`[Camera]` parser and no background draw path.
  ([crates/fp-stage/src/lib.rs](../crates/fp-stage/src/lib.rs))
- **Impact:** Matches render over a flat clear color
  (`frame.clear(0.1, 0.1, 0.15)`, [fp-app/src/main.rs:1167](../crates/fp-app/src/main.rs#L1167)).
  No scrolling stage, no parallax, no camera follow.
- **Workaround:** None — the fight is fully playable on the flat background.

### HUD text is colored quads, no font rendering (#30) — Missing

- **What's missing:** No FNT (MUGEN font) parser exists — only a doc-comment
  placeholder in [fp-formats/src/lib.rs](../crates/fp-formats/src/lib.rs) and the
  word "FNT" in that crate's `Cargo.toml` description. No ACT palette parser
  either.
- **Impact:** The KO / round-state readout is a centered solid-color marker
  (yellow KO, green/red winner), not text. There is no on-screen timer, combo
  counter, or any glyph rendering. The HUD is hand-rolled 1×1 indexed quads
  (`Hud`, [fp-app/src/main.rs:627](../crates/fp-app/src/main.rs#L627)).
- **Workaround:** Read the round/KO state from the colored marker.

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

### SFF v1 art renders with no colors (#25) — Missing

- **What's wrong:** SFF v1 stores each sprite's 256-color palette inline (the
  768-byte trailing PCX palette), but the v1 loader only decodes pixel
  *indices* — it never reads the palette. `palettes` is left empty for v1
  ([fp-formats/src/sff/mod.rs:128](../crates/fp-formats/src/sff/mod.rs#L128); the
  index-only decode is in
  [fp-formats/src/sff/v1.rs](../crates/fp-formats/src/sff/v1.rs)).
- **Impact:** WinMUGEN-era (SFF v1) sprites — typically intro/ending/motif art —
  have no colors to look up and render invisible. SFF v2 (modern, e.g. KFM's
  `kfm.sff`) is unaffected and fully colored.
- **Workaround:** Use SFF v2 content. Note also that SFF v1 sprites are
  cosmetically mislabeled `SpriteFormat::Png8`; this is harmless because the v1
  decode path short-circuits before the format match
  ([fp-formats/src/sff/v1.rs:144](../crates/fp-formats/src/sff/v1.rs#L144)).

### PNG sprites are not decoded (#35) — Missing

- **What's missing:** SFF v2 `Png8`/`Png24`/`Png32` sprite payloads hit an
  explicit `FpError::Unsupported` stub
  ([fp-formats/src/sff/compression.rs:341](../crates/fp-formats/src/sff/compression.rs#L341)).
  Only uncompressed/RLE8/RLE5/LZ5 indexed sprites decode.
- **Impact:** Modern HD characters that store sprites as PNG inside SFF v2 fail to
  load those sprites (true-color 24/32-bit also routes through this stub).
- **Workaround:** Use indexed (RLE/LZ5) SFF v2 content such as KFM.

### Hit sparks are not spawned or rendered (#17) — Missing

- **What's wrong:** The hit primitive `detect_hit_contact` already computes the
  spark anchor point and `HitResources` carries `sparkno`
  ([fp-combat/src/lib.rs](../crates/fp-combat/src/lib.rs)), but `fp-engine` never
  emits a spark request — there is no Explod/effect entity to draw it.
- **Impact:** Hits connect (damage, hitpause, sound all fire) but there is no
  visual impact spark.
- **Workaround:** None; the hit sound and life-bar drop confirm the hit landed.

### AfterImage / PalFX color effects do nothing (#33) — Missing

- **What's missing:** The `AfterImage`, `AfterImageTime`, and `PalFX` controllers
  have no dispatch arm; they fall to the debug-logged no-op branch
  ([fp-character/src/executor.rs:974](../crates/fp-character/src/executor.rs#L974)).
  There is no color-tint render uniform.
- **Impact:** Moves that should trail an afterimage or flash a palette effect
  (common on supers) render with no visual effect.
- **Workaround:** None — purely cosmetic.

---

## Gameplay fidelity

The fight engine is faithful for the KFM feature set, but several MUGEN
mechanics are still unmodeled. These affect more advanced characters more than
KFM.

### No global Pause / SuperPause freeze (#24) — Missing

- **What's missing:** Neither a `Pause` nor a `SuperPause` controller exists in
  `fp-character` or `fp-engine` (grep returns nothing). Hitpause is **per
  character only**, not a whole-match freeze.
- **Impact:** Super flashes and dramatic-freeze moments that rely on a global
  game-time freeze do not pause the opponent or the timer. Supers still fire
  (meter, damage, p1stateno) — they just don't freeze the screen.
- **Workaround:** None; gameplay continues without the freeze.

### RoundState / GameTime / MatchOver not exposed to triggers (#21) — Missing

- **What's wrong:** The `Match` tracks `round_state`/`timer`/`round_number`, but
  those values are not threaded into the trigger context. `RoundState`,
  `GameTime`, and `MatchOver` triggers return the safe default `0` (pinned by a
  test at [fp-character/src/lib.rs:2285](../crates/fp-character/src/lib.rs#L2285)).
- **Impact:** Character logic that gates on round phase (intro poses, win poses,
  round-based behavior) won't react.
- **Workaround:** None; affected branches simply never fire.

### Width controller is ignored (#10) — Missing

- **What's missing:** No `Width` dispatch arm; per-state push/collision width
  falls to the no-op branch. Only the static `[Size]` widths apply.
  ([fp-character/src/executor.rs:974](../crates/fp-character/src/executor.rs#L974))
- **Impact:** Moves that should temporarily change push width (e.g. crouching or
  a lunging attack) keep the default width.
- **Workaround:** None.

### AssertSpecial flags are not applied (#13) — Missing

- **What's missing:** `AssertSpecial` (NoWalk / NoAutoTurn / Intro and friends)
  has no dispatch arm and no asserted-flag set.
  ([fp-character/src/executor.rs:974](../crates/fp-character/src/executor.rs#L974))
- **Impact:** Per-tick behavior toggles a character asserts (disable walking,
  suppress auto-turn during a move, intro lockout) have no effect.
- **Workaround:** None.

### Dropped Statedef headers: SprPriority, juggle, facep2, persist flags (#16) — Missing

- **What's wrong:** `fp-formats` parses the `sprpriority`, `juggle`, `facep2`,
  `hitdefpersist`, and `movehitpersist` statedef headers, but
  `CompiledState::from_parsed` drops them at compile time
  ([fp-character/src/loader.rs:363](../crates/fp-character/src/loader.rs#L363)).
  There is also no `SprPriority` controller arm.
- **Impact:** No sprite-draw-priority ordering, no juggle limits, no per-state
  face-opponent override, no HitDef/MoveHit persistence across state changes.
- **Workaround:** None; characters relying on these headers behave subtly wrong.

### No on-hit power gain / transfer (#18) — Missing

- **What's wrong:** `resolve_attack` never adds power to attacker or defender on
  a connecting hit (no power writes in
  [fp-character/src/combat.rs](../crates/fp-character/src/combat.rs)). MUGEN's
  HitDef `getpower`/`givepower` are unmodeled.
- **Impact:** The super meter does **not** build from landing or taking hits. It
  only changes via explicit `PowerAdd`/`PowerSet` controllers and
  `TargetPowerAdd`.
- **Workaround:** Characters that script `PowerAdd` on hit still build meter; the
  automatic HitDef power gain does not.

### No priority / trade clash resolution (#20) — Missing

- **What's wrong:** `PriorityType` and `Priority{value, kind}` are **data only**
  in `fp-combat`; nothing compares two simultaneous HitDefs. Combat runs strictly
  sequentially — P1→P2 then P2→P1 — with no trade arbitration
  ([fp-engine/src/lib.rs:749](../crates/fp-engine/src/lib.rs#L749)).
- **Impact:** Two attacks that should clash/trade based on priority will instead
  both resolve in order; there are no proper trades or move cancels on clash.
- **Workaround:** None.

### Get-hit velocity controllers missing (#23) — Missing

- **What's missing:** `HitVelSet`, `HitFallSet`, `HitFallVel`, and
  `HitFallDamage` have no dispatch arms
  ([fp-character/src/executor.rs:974](../crates/fp-character/src/executor.rs#L974)).
- **Impact:** Get-hit common states cannot fine-tune knockback/fall via these
  controllers. Note: **basic knockback is already applied** in `resolve_attack`,
  so hits still launch and push correctly — only the controller-level tuning is
  absent.
- **Workaround:** Rely on the built-in knockback from the HitDef velocities.

### No power bar in the HUD (#26) — Missing

- **What's wrong:** The engine `Player` exposes no `power()`/`power_max()`
  accessor and the HUD draws life bars only
  ([fp-app/src/main.rs:627](../crates/fp-app/src/main.rs#L627)).
- **Impact:** The super meter exists and works in the simulation (power is
  tracked and carried across rounds), but you cannot **see** it.
- **Workaround:** None visually; supers still trigger when meter is available.

---

## Robustness & infrastructure

These don't affect the visible fight much today but matter for correctness,
testing, and the long-term customizable-engine vision.

### Input is sampled inside the catch-up loop (#27) — Partial

- **What's wrong:** The keyboard is read **inside** the fixed-timestep catch-up
  loop (`event_pump.keyboard_state()` at
  [fp-app/src/main.rs:1131](../crates/fp-app/src/main.rs#L1131), inside
  `while accumulator >= TICK_DURATION` at
  [fp-app/src/main.rs:1127](../crates/fp-app/src/main.rs#L1127)).
- **Impact:** On a frame that drains multiple ticks (e.g. after a hitch), every
  catch-up tick re-reads the same live keyboard, so a single press can be
  over-counted and timing isn't snapshot-once-per-frame. Mostly invisible at a
  steady 60 FPS.
- **Workaround:** Run on hardware that holds 60 FPS so each frame drains exactly
  one tick.

### RNG-in-state is non-functional (#28) — Missing

- **What's wrong:** Neither `Character` nor `EvalCtx` overrides the VM's
  `random()` seam, so it uses the trait default that returns a fixed `0`
  ([fp-vm/src/eval.rs:525](../crates/fp-vm/src/eval.rs#L525)). There is no
  per-entity seed/RNG field. (The VM does ship a deterministic Park-Miller `Rng`
  ready to be wired in — it's just not connected to the entity yet.)
- **Impact:** The `Random` trigger and any state logic gated on RNG always read
  `0`, so randomized AI/move selection is effectively disabled.
- **Workaround:** None; randomized branches are deterministic-zero.

### CI's real-content tests run as green no-ops (#36) — Infra gap

- **What's wrong:** Real-KFM tests are **asset-gated** — they early-return when
  `test-assets/` is absent. `test-assets` is a gitignored local-only symlink and
  the [CI workflow](../.github/workflows/ci.yml) has **no fetch/restore step**,
  so the entire real-content regression net executes as green no-ops on CI. Only
  synthetic tests truly run there.
- **Impact:** **A green CI badge does not mean real-content behavior is
  verified.** Regressions in any KFM-driven path can pass CI silently. This is a
  meta-multiplier — it can hide regressions in every other fix listed here.
- **Workaround:** Run `cargo test --workspace` **locally** with `test-assets`
  present to exercise the real-content suite before trusting a change.

### No VM fuzz / property tests (#37) — Missing

- **What's missing:** `fp-vm` has no `proptest`/`quickcheck` dependency and no
  `cargo-fuzz` target — only hardcoded adversarial-string smoke tests
  (e.g. [fp-vm/src/parser.rs](../crates/fp-vm/src/parser.rs)). The evaluator's
  never-panic contract is well covered by ~474 unit/integration tests but not by
  generative testing.
- **Impact:** Lower confidence that exotic/malformed expressions can't surface an
  unhandled edge. (No known panic — the never-panic contract is upheld in
  practice.)
- **Workaround:** None needed for normal content; relevant only for hardening.

### No replay / determinism / rollback (#38) — Missing

- **What's missing:** No state serialization (no `serde`/`bincode`), no replay
  capture, and no rollback netcode. The fixed-timestep, deterministic design is a
  prerequisite the engine already meets, but the save/restore machinery isn't
  built.
- **Impact:** No replays, no rollback-based netplay, no save states. Note this
  interacts with the RNG gap (#28) — true determinism also needs a seeded,
  serializable RNG.
- **Workaround:** None.

---

## Scope: single-match only (#39)

Today Fighters Paradise is a **1v1 local match** built around the KFM feature
set. The following broader scope is **not yet implemented**:

| Out of scope today | Notes |
|---|---|
| Team / Simul / Turns / Tag modes | Only one fighter per side; `Match` is strictly two-player. |
| Real second player or AI | P2 receives `MatchInput::none()` every tick — an idle dummy ([fp-app/src/main.rs:1133](../crates/fp-app/src/main.rs#L1133)). |
| `.act` external palette files | No parser; only in-SFF palettes are read. |
| Extended AIR features | Per-frame `scale`/`angle`/`Interpolate` blocks are not parsed — only `group, image, x, y, ticks, [flip], [blend]` ([fp-formats/src/air.rs](../crates/fp-formats/src/air.rs)). |
| Intro / ending storyboard playback (#32) | `fp-storyboard` parses storyboard `.def` into a typed scene model but never ticks or renders it — it has no consumer crate ([crates/fp-storyboard/src/storyboard.rs](../crates/fp-storyboard/src/storyboard.rs)). |

Also unimplemented for debugging convenience: a Clsn hitbox/hurtbox debug overlay
(#34).

---

## Smaller correctness notes

A few details that are easy to trip over but lower-impact:

- **Some GetHitVars stay at defaults.** `resolve_attack` populates 12 of 15
  GetHitVars; `GetHitVar(type)` (`hit_type`), `hitcount`, and `isbound` are
  **not** populated even though the HitDef carries the source data
  ([fp-character/src/combat.rs](../crates/fp-character/src/combat.rs)).
- **`HitOverride` is missing.** `NotHitBy`/`HitBy` i-frames work (#9), but the
  related `HitOverride` controller has no dispatch arm anywhere. (The audit's #9
  ✅ marker over-claims this — i-frames are done, `HitOverride` is not.)
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
