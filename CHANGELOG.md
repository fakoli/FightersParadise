# Changelog

All notable changes to Fighters Paradise will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

> **From sprite viewer to playable fighter.** This cycle turns the project from a set of mostly-stub
> crates into a **playable two-character match** driven by real Kung Fu Man data, then brings the
> documentation back in line with the code. Only `fp-stage` and `fp-ui` remain true stubs. See the
> [faithfulness audit](docs/knowledge-base/08-faithfulness-audit.md) for the ranked fidelity ledger.

### Added — Engine & gameplay

- **fp-engine**: Two-player `Match` coordinator with a fixed 6-step tick (feed input → tick both state
  machines + apply `Target*` ops → resolve combat both directions → push + bounds clamp → face-opponent →
  advance round), an `Intro → Fight → Ko → Win` round state machine, and **best-of-3** rounds (default
  `rounds_to_win = 2`; power carries across rounds within a match).
- **fp-character**: Live `Character` entity + per-tick **MUGEN-order executor** (special states −3/−2/−1
  then the current numbered state), with controller gating (`triggerall` AND, numbered-group OR with CB6
  contiguity, `persistent`/`ignorehitpause`). ~30 state controllers dispatched (`ChangeState`, `SelfState`,
  `Vel*`, `Pos*`, `ChangeAnim`, `Var*`, `Power*`, `AttackMulSet`/`DefenceMulSet`, `StateTypeSet`, `Turn`,
  `PlaySnd`, `HitDef`, `NotHitBy`/`HitBy`, the `Target*` throw family, …); any unhandled controller falls to
  a debug-logged safe no-op.
- **fp-character**: `LoadedCharacter::load` pipeline — `.def` parse, required SFF+AIR + optional CMD+SND,
  first-wins CNS merge with `stcommon` last, `.cmd` `[Statedef -1]` command→state bridge merged as a
  supplement, then engine-injected built-in ground locomotion + auto-land (50/51→52). Every trigger and
  controller parameter is compiled to an `fp_vm` AST at load time (const-0 fallback on parse error, never a
  panic).
- **Cross-entity eval keystone**: `EvalEnv { opponent, stage, anim }` (`Copy`) threaded through the
  executor; a short-lived `EvalCtx { me, opponent, stage, anim }` resolves `p2`/`enemy`/`enemynear`→opponent
  and `root`→self, and computes `P2Dist`/`P2BodyDist`, edge distances, `ScreenPos`, `P2Life`/`P2StateNo`,
  and `SelfAnimExist`. This unblocked redirects and all cross-entity triggers.
- **Deferred-effects pattern**: a tick cannot `&mut` another entity, so a character emits `sound_requests`
  (`PlaySnd`) and `target_ops` (`Target*` throws) into a `TickReport` that `fp-engine` applies after both
  characters tick.
- **fp-combat**: `HitDef` data model with MUGEN-faithful defaults, tolerant never-panic parsers
  (`AttackAttr`/`HitFlags`/`AnimType`/`SoundId`), the `Clsn1`×`Clsn2` hit-detection primitive
  (`detect_hit`/`detect_hit_contact`), and the pure `resolve_hit → HitOutcome` Guard/Hit/Miss decision ladder.
- **Hit application** (`fp_character::combat::resolve_attack`): detect → resolve → **apply** bridge that
  mutates both characters — damage scaled by `attack_mul × defence_mul`, mirrored knockback, 12 populated
  `GetHitVars` (including `animtype`), `hitpause` set on both sides via `max()`, `NotHitBy`/`HitBy` i-frame
  gating consulted before applying, and `hitonce`/`numhits` handling.
- **Throws** (`fp-engine::apply_target_ops`): `TargetState`/`TargetBind`/`TargetLifeAdd`/`TargetFacing`/
  `TargetVelSet`/`TargetVelAdd`/`TargetPowerAdd` applied to the opponent; the `HitDef` `p1stateno` moves the
  attacker. KFM's signature throw works end to end.
- **Super meter**: `power`/`power_max` on `Character`, driven by `PowerAdd`/`TargetPowerAdd`; supers fire
  when meter is available; meter persists across rounds within a match.
- **Hitpause / hit-reactions / movement**: per-character hitpause freeze (only `ignorehitpause` controllers
  run while frozen, anim/time/physics frozen), get-hit common states 5000+ runnable, jump + air-jump
  (double jump) + data-driven auto-land, friction idle-stop via the `const()` threshold seam, `AnimElemTime`
  per-element offset table.
- **~15 faithfulness-audit items closed** this cycle (verified against code; see
  [doc 08](docs/knowledge-base/08-faithfulness-audit.md)): the cross-entity eval keystone (#1/#2),
  `SelfState` (#3), forward/back/run/air-jump velocity constants (#4), `Const720p`/`Const1280p` coordinate
  scaling (#5), `AnimElemTime(n)` (#6), `GetHitVar(animtype)` (#7), throw controllers (#8), `NotHitBy`/`HitBy`
  i-frames (#9, `HitOverride` still pending), `VelMul` (#11), friction threshold seam (#12, no explicit
  snap line yet), air-jump consts (#14), ground-plane Y clamp + auto-land (#15), damage multipliers (#19),
  `SelfAnimExist` (#22) — plus super-meter `PowerAdd`, hitpause, best-of-3 rounds, and audio.

### Added — Formats, audio & I/O

- **fp-formats/sff**: SFF v1↔v2 auto-detection plus a full SFF v2 pipeline on the **real MUGEN 1.0 header
  layout** (512-byte header, 28-byte sprite + 16-byte palette sub-headers, LData/TData blocks, on-demand
  decode). Faithful **LZ5** and **RLE5** decoders (ports of the Elecbyte/Ikemen algorithms) join RLE8 and
  raw; LZ5/RLE8 verified against real `kfm.sff`, RLE5 against a synthesized fixture. SFF v1 PCX container
  decode added. PNG decode and SFF v1 palette extraction remain explicit stubs.
- **fp-formats/cns**: CNS parser — `[Statedef N]` headers + `[State N]` controllers with `triggerall` /
  numbered trigger groups, splitting only on the **first `=`** so trigger expressions survive verbatim.
- **fp-formats/snd**: SND container parser (directory linked-list walk, raw RIFF/WAVE payload lookup; PCM
  decode is `fp-audio`'s job).
- **fp-vm**: CNS expression engine — tolerant lexer → Pratt parser (full 9-level MUGEN precedence) →
  **tree-walk evaluator** with never-panic numeric semantics (div/mod-by-zero, unknown trigger, unresolved
  redirect all collapse to `0`). Redirects (`parent`/`root`/`helper`/`target`/`enemy`/`enemynear`/`partner`/
  `playerid` + `p1`/`p2` aliases), range literals, member-keyed `GetHitVar`/`const`, `command="…"`, and an
  entity-owned Park-Miller RNG seam.
- **fp-audio**: rodio-backed channel mixer with MUGEN cut-off semantics, a hardened WAV decoder, and a
  `NullBackend` headless fallback (never panics without an audio device).
- **fp-app**: SDL2 + wgpu host running the playable match — 60Hz accumulator loop, four CLI run modes
  (two `.def` → match; one `.def`; `.sff [.air]` viewer; no-args default two-KFM match), per-side GPU sprite
  caches, a hand-rolled solid-quad **life HUD + KO/winner marker**, and per-tick sound-request routing to
  `fp-audio`.
- **fp-input**: backward-scanning command matcher now understands MUGEN `~` (release), `/` (hold),
  `$` (direction-detect), `>` (strict-immediate), and `+` (simultaneous).

### Changed — Documentation overhaul

- **Corrected the stale "early v0.1.0 / mostly stubs" status** across `README.md` and `CLAUDE.md`: the
  per-crate status tables now reflect reality (only `fp-stage` and `fp-ui` are true stubs) and the test
  count is updated (~1769 passing incl. doc tests; **1724** `#[test]` attributes) — the old docs claimed
  131 tests.
- **New authoritative docs under `docs/`**: [`mugen-compatibility.md`](docs/mugen-compatibility.md),
  [`content-guide.md`](docs/content-guide.md), [`known-issues.md`](docs/known-issues.md),
  [`roadmap.md`](docs/roadmap.md), and [`development.md`](docs/development.md), cross-linked from
  [`architecture.md`](docs/architecture.md) and the root README.
- Clarified `fp-vm` is a **tree-walk evaluator**, not the "bytecode + stack VM" the crate name/old docs
  implied.
- Documented the known caveats surfaced by the audit: input sampled inside the catch-up loop (#27) and the
  CI asset-gate (#36 — real-content tests run as green no-ops on CI because `test-assets/` is gitignored
  with no fetch step).

### Notes

- **Clean-room contract holds**: no Elecbyte/MUGEN engine source or copyrighted assets are tracked or
  shipped. Real KFM content (CC BY-NC 3.0) is local-only behind the gitignored `test-assets` symlink;
  `git ls-files` tracks zero content files.
- **CI is green** on Ubuntu (`clippy -D warnings` → build → test); `cargo clippy --workspace --all-targets
  -- -D warnings` is clean. macOS SDL2 linking is auto-handled by `.cargo/config.toml`.
- Still pending (roadmap / [known issues](docs/known-issues.md)): `fp-stage` background rendering, `fp-ui`
  screenpack + real lifebars, FNT text rendering, SFF v1 palette extraction + PNG decode, hit-spark
  rendering, `Pause`/`SuperPause` global freeze, and ~17 more ranked items in
  [doc 08](docs/knowledge-base/08-faithfulness-audit.md).

### Earlier in development (now superseded above)
- **fp-input**: Input state types (Button, Direction, InputState), 60-frame ring buffer, MUGEN command sequence matcher with `compile_command()` parser
- **fp-physics**: PhysicsBody with Euler integration, ground plane clamping (Y=0), gravity, `on_ground()`/`in_air()` predicates
- **fp-formats/cmd**: CMD file parser for MUGEN input command definitions (`[Command]` and `[Defaults]` sections)
- **fp-app**: Playable character mode with SDL2 key mapping, hardcoded common state machine (idle, walk, crouch, jump), physics-driven movement

## [0.1.0] - 2025-01-01

### Phase 2: Animate a Character
- **fp-formats/air**: AIR animation file parser with frame sequences, collision boxes (Clsn1/Clsn2), blend modes, flip flags, loopstart
- **fp-render**: AnimController for animation playback with tick timing, looping, and frame advancement
- **fp-app**: Animation viewer mode with Left/Right arrow action cycling and Space to restart

### Phase 1: Draw a Sprite
- **fp-core**: Vec2, Rect, SpriteId, AnimId, SoundId, FpError/FpResult
- **fp-formats/sff**: SFF v2 parser with RLE5, RLE8, LZ5 decompression, linked sprites, shared palettes
- **fp-formats/def**: DEF file parser (INI-style key-value config)
- **fp-render**: wgpu rendering pipeline with R8Unorm indexed textures, WGSL palette lookup shader, orthographic projection
- **fp-app**: SDL2 window with 60Hz fixed timestep game loop, test pattern, static sprite display
- Cargo workspace with 14 crates
