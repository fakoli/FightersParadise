# 04 — Fighters Paradise Codebase Review (verified)

A ground-truth review of the workspace as of this knowledge-base compile (2026-06-13, commit
`52db1f7`). Every claim below was checked against the actual source, not just the README — and the
**docs match reality** (a rare and good sign).

## Verdict up front

**Architecturally sound, honestly in Phase 3.** The four foundational systems (parsers, rendering,
input, physics) are production-quality. The playable demo is a solid proof of concept. The heavy
lifting — expression VM, data-driven state machine, combat, round flow — is genuinely still ahead,
and the stubs are honest (7-line headers, **zero `todo!()`/`unimplemented!()`** hiding work).

## Crate status (verified)

14 crates, ~5,400 LOC of real code across 6 implemented crates; 8 stubs (~7 lines each).

| Crate | LOC | Status | Reality |
|-------|-----|--------|---------|
| **fp-core** | 617 | ✅ Done | `Vec2`, `Rect`, `SpriteId`, `AnimId`, `SoundId`, `FpError`/`FpResult`. Deps: thiserror/tracing only. |
| **fp-formats** | 1219 | ✅ Done | SFF v2 (RLE8/RLE5/LZ5), AIR (frames + Clsn + blend), CMD (commands + timing), DEF (INI). `nom`-based. |
| **fp-render** | 1625 | ✅ Done | wgpu pipeline, palette-lookup WGSL shader, 3 blend modes, anim controller, texture atlas (unused yet). |
| **fp-input** | 1042 | ✅ Done | 60-frame ring buffer, command compiler/matcher (678 LOC), button/direction state. |
| **fp-physics** | 195 | ✅ Done | Euler integration, gravity, ground-plane clamp, jump arcs. Simple but complete. |
| **fp-app** | 961 | ✅ Done | SDL2 window, 60Hz accumulator loop, **hardcoded 7-state machine**, loads SFF/AIR/CMD. |
| **fp-vm** | 7 | ❌ Stub | Bytecode compiler + stack VM for expressions. **Critical — blocks Phase 4.** |
| **fp-combat** | 7 | ❌ Stub | HitDef, damage, juggle, guard. |
| **fp-character** | 7 | ❌ Stub | Character struct + CNS state machine. (fp-app's hardcoded SM is the temporary stand-in.) |
| **fp-stage** | 7 | ❌ Stub | Stage loading, backgrounds, camera. |
| **fp-audio** | 7 | ❌ Stub | Sound mixer, BGM, SFX. (`rodio` in deps, unused.) |
| **fp-ui** | 7 | ❌ Stub | Lifebars, menus, select screen. |
| **fp-storyboard** | 7 | ❌ Stub | Cutscenes. |
| **fp-engine** | 7 | ❌ Stub | Game coordinator + round flow. **Critical — no real match yet.** |

## Dependency graph (verified)

The demo app is **standalone** — it does not yet depend on the higher-level stub crates:

```
fp-app (binary)
  ├─ fp-formats ─ fp-core, nom
  ├─ fp-render  ─ fp-formats, fp-core, wgpu, bytemuck, pollster
  ├─ fp-input   ─ fp-core
  ├─ fp-physics ─ fp-core
  └─ sdl2, wgpu, tracing
```

Clean DAG, **no cycles**. External stack (root `Cargo.toml`): `thiserror 2`, `tracing 0.1`,
`nom 7`, `wgpu 24`, `sdl2 0.37`, `bytemuck 1`, `rodio 0.20`, `slotmap 1`, `pollster 0.4`.
Workspace-level deps inherited via `.workspace = true`.

> **Observation:** `slotmap` is declared but unused — clearly reserved for the helper/projectile
> arena (correct instinct; see [03 § 6](03-engine-architecture.md#6-helpers-projectiles-explods--the-object-model)).

## Parser completeness

| Format | State | Notes |
|--------|-------|-------|
| **SFF v2** | ✅ Full | RLE8/RLE5/LZ5 decode, sprite/palette linking, graceful EOF truncation with `warn!`. **PNG = error stub.** Entry: `SffFile::load` / `from_bytes` / `sprite(g,i)` / `decode_sprite` / `palette`. |
| **AIR** | ✅ Full | `[Begin Action N]`, frame entries, Clsn1/Clsn2, Loopstart, blend modes. → `AirFile`/`AnimAction`/`AnimFrame`. |
| **CMD** | ✅ Full | `[Defaults]` + `[Command]` (name, command, time, buffer.time). Deliberately skips `[Statedef]`/`[State]` (reserved for CNS). |
| **DEF** | ✅ Full | INI-style, case-insensitive section/key lookup. |
| **SFF v1** | ❌ Missing | **Not implemented** — but the largest *legacy* content library is v1 (PCX). Gap for full compatibility. |
| **CNS** | ❌ Missing | Phase 4. Needs the trigger-expression parser + state-controller model. |
| **SND** | ❌ Missing | Phase 8. |
| **FNT** | ❌ Missing | Phase 9. |

## The game loop & demo (fp-app)

- **Fixed 60Hz accumulator:** `TICK_DURATION = 16_666_667 ns`; accumulate wall-clock, drain in
  fixed ticks, render outside the tick loop. Textbook-correct.
- **Hardcoded state machine (Phase 3 placeholder):** 7 states — IDLE(0), WALK_FWD(12), WALK_BACK(13),
  CROUCH(20), JUMP_START(40), AIRBORNE(50), LANDING(52) — with KFM-tuned physics constants
  (`WALK_FWD_VEL=2.4`, `JUMP_VEL_Y=-8.4`, `GRAVITY=0.44`, etc.).
- **Per-tick:** poll SDL2 keyboard → `InputState` → push to `InputBuffer` → run `CommandMatcher` →
  state machine + physics + animation.
- **Demo modes:** no args = checkerboard; `<sff>` = static viewer; `<sff> <air>` = playable;
  `<sff> <air> <cmd>` = playable + command recognition.

**Can do:** walk/crouch/jump with arc + horizontal drift, animate per state, buffer input 60 frames,
detect command sequences (e.g. QCF), flip on direction change, apply blend modes.
**Cannot do:** data-driven states, hit anything, take damage, multi-character rounds, projectiles,
stage background, UI/lifebars, sound.

## Rendering (fp-render)

- **Palette-indexed:** sprite texture is `R8Unorm` (one channel → palette index 0–255); palette
  texture is 256×1 RGBA. **Index 0 = transparent (discarded).** Palette swap = rebind the palette
  texture, no re-upload — the GPU palette-lookup design pays off here.
- **Blend modes:** Normal (`SrcAlpha,OneMinusSrcAlpha`), Additive (`One,One`), Subtractive.
- **Projection:** orthographic, origin top-left, Y increases downward.
- **Sprite caching:** textures uploaded once, cached in `HashMap<SpriteId, CachedSprite>` per
  character (unbounded — fine for one character, a scaling concern for large rosters).

## Input (fp-input)

- **`InputBuffer`:** fixed 60-frame ring buffer (`get(frames_ago)`).
- **`compile_command("~D, DF, F, x")`** → `Vec<CommandElement>` (Direction / Button / NotDirection /
  NotButton), hand-written recursive-descent parser handling `~` release, grouping, charge notation.
- **`CommandMatcher::check_commands(buffer, facing_right)`** → matched `(name, buffer_time)` pairs.

## Tests

**131 unit tests, verified**, well distributed: fp-formats 59 (SFF 33 / AIR 10 / CMD 10 / DEF 6),
fp-render 22, fp-input 18, fp-core 15, fp-app 9, fp-physics 8. Parsers test against **synthetic byte
arrays inline** (no external fixtures). **No integration tests** yet (no cross-crate end-to-end).

## Gaps & technical debt (prioritized)

**Blocking a real game:**
1. **`fp-vm`** (0 LOC) — no expression evaluation → blocks every data-driven behavior. *Start here.*
2. **CNS parser** (missing in fp-formats) — no state data to interpret.
3. **`fp-character`** (0 LOC) — no persistent entity, no helper/projectile system; the hardcoded SM
   must move here and become data-driven.
4. **`fp-combat`** (0 LOC) — no hit detection/damage/juggle/guard.
5. **`fp-engine`** (0 LOC) — no round flow / P1-vs-P2 coordination. The loop currently lives in
   fp-app and must migrate here.

**Architectural fragility:**
- Hardcoded state machine in fp-app couples demo logic to one character; refactor target for Phase 5.
- `state_to_action()` is a hardcoded `match` — will break for non-standard action maps; must become
  data-driven from DEF/AIR.
- No multi-character support; no character-vs-character push/collision.
- Per-character unbounded sprite cache; large rosters will want the (already-present) `TextureAtlas`
  or a texture array.

**Missing but not critical:** SFF v1 (legacy compat — actually important for *content* breadth),
PNG sprites in SFF v2, collision-box debug overlay, replay/netcode.

**Quality positives:** no panics on bad content (FpResult + safe defaults + `warn!`), clean error
types (thiserror), clean dependency DAG, zero clippy warnings, good parser test coverage.

## Documented design decisions (from `docs/`)

- `docs/architecture.md` — crate dependency graph, 60Hz fixed timestep, struct-based entities (not
  ECS), bytecode-compiled expressions, GPU palette lookup.
- `docs/format-specs/sff-v2.md` — detailed SFF v2 binary layout, sub-header offsets, compression,
  sprite/palette linking.
- `CLAUDE.md` — build/run, crate status, Rust conventions (edition 2021, `#![warn(missing_docs)]`,
  thiserror, tracing, never-panic), and the four design principles.

These are consistent with what the code actually does. The roadmap and gap analysis live in
[05-reimplementation-roadmap.md](05-reimplementation-roadmap.md).
