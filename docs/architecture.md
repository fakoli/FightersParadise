# Fighters Paradise — Architecture

The authoritative architecture specification for Fighters Paradise, a clean-room
reimplementation of the [MUGEN](https://en.wikipedia.org/wiki/Mugen_(game_engine))
2D fighting engine in Rust. It runs unmodified MUGEN content files — `.def`,
`.sff`, `.air`, `.cmd`, `.cns`, `.snd` — and today drives a **playable
two-character match** off real Kung Fu Man data.

> Status snapshot (2026-06): a playable best-of-3 fight (P1 keyboard, P2 idle
> dummy), real KFM data end to end, ~1,724 `#[test]` functions across 14 crates
> (~1,769 passing including doc/integration tests). Only `fp-stage` and `fp-ui`
> are still true stubs. See [Known issues](known-issues.md) and the
> [Roadmap](roadmap.md). For the per-feature MUGEN-fidelity ledger see the
> [faithfulness audit](knowledge-base/08-faithfulness-audit.md).

Related docs: root [README](../README.md) · [MUGEN compatibility](mugen-compatibility.md)
· [Content guide](content-guide.md) · [Development](development.md) ·
[Format specs](format-specs/) · [Knowledge base](knowledge-base/).

---

## 1. Crate workspace & dependency graph

A single Cargo workspace (edition 2021, resolver v2, MIT) with **14 path-member
crates** under `crates/`. External and internal dependencies are declared once in
the root `Cargo.toml` `[workspace.dependencies]` and inherited via
`.workspace = true`.

```
                              ┌──────────────┐
                              │   fp-app     │  binary: SDL2 window, 60Hz loop,
                              │  (main.rs)   │  CLI, HUD, two-player wiring, audio
                              └──────┬───────┘
            ┌──────────────┬────────┼────────┬───────────────┬──────────┐
            ▼              ▼        ▼         ▼               ▼          ▼
       ┌─────────┐   ┌──────────┐  │   ┌───────────┐   ┌──────────┐ ┌──────────┐
       │fp-engine│   │fp-render │  │   │ fp-audio  │   │fp-physics│ │fp-input  │
       │ (Match) │   │  (wgpu)  │  │   │  (rodio)  │   │          │ │          │
       └────┬────┘   └────┬─────┘  │   └────┬─────┘   └────┬─────┘ └────┬─────┘
            │             │        │        │              │            │
            ▼             ▼        ▼        │              │            │
     ┌────────────┐  ┌──────────────┐      │              │            │
     │fp-character│  │  fp-formats  │◄──────┘ (formats also feeds      │
     │ (entity +  │  │  (parsers)   │         render/character/        │
     │  executor) │  └──────┬───────┘         engine/storyboard)       │
     └─────┬──────┘         │                                          │
   ┌───────┼──────┬─────────┼──────────┐                              │
   ▼       ▼      ▼         ▼           ▼                              │
┌──────┐┌──────┐┌────────┐┌──────────┐┌──────────┐                    │
│fp-vm ││fp-   ││fp-     ││fp-formats││fp-input  │◄───────────────────┘
│      ││combat││physics ││          ││          │
└───┬──┘└──┬───┘└───┬────┘└────┬─────┘└────┬─────┘
    └──────┴────────┴──────────┴───────────┴──────────────┐
                                                          ▼
                                                    ┌──────────┐
                                                    │ fp-core  │  Vec2, Rect,
                                                    │          │  SpriteId, FpError
                                                    └──────────┘

Not yet wired into the runtime loop:
  fp-stage      (STUB — no [BGDef]/[BG]/[Camera] parser)
  fp-ui         (STUB — HUD is hand-rolled quads in fp-app, not a screenpack)
  fp-storyboard (parser-only; no consumer crate depends on it)
```

Edges below are read directly from each crate's `Cargo.toml`:

| Crate | Depends on (workspace crates) | Role |
|-------|-------------------------------|------|
| `fp-core` | — | Shared types: `Vec2`, `Rect`, `SpriteId`, `FpError`/`FpResult` |
| `fp-formats` | `fp-core` | Binary/text ingestion: SFF v1/v2, AIR, CMD, DEF, CNS, SND |
| `fp-vm` | `fp-core` (+dev `fp-formats`) | CNS trigger-expression engine (lexer → Pratt parser → evaluator) |
| `fp-input` | `fp-core` | 60-frame ring buffer + MUGEN command recognition |
| `fp-physics` | `fp-core` | Euler integration, ground plane, AABB, push/bounds |
| `fp-combat` | `fp-core`, `fp-physics` | `HitDef` data model, Clsn hit primitive, pure `resolve_hit` |
| `fp-character` | `fp-core`, `fp-formats`, `fp-vm`, `fp-combat`, `fp-input` | Loader, `Character` entity, per-tick executor, cross-entity eval |
| `fp-engine` | `fp-core`, `fp-character`, `fp-physics`, `fp-input` (+dev `fp-formats`, `fp-combat`) | Two-player `Match` coordinator, round/best-of-N flow |
| `fp-render` | `fp-core`, `fp-formats` | wgpu palette-lookup sprite renderer |
| `fp-audio` | `fp-core` | rodio WAV decode + channel-managed playback |
| `fp-stage` | `fp-core` | **STUB** — stage background/camera |
| `fp-ui` | `fp-core` | **STUB** — lifebars/screenpack |
| `fp-storyboard` | `fp-core`, `fp-formats` | Storyboard `.def` parser + typed scene model (no tick/render) |
| `fp-app` | `fp-core`, `fp-formats`, `fp-character`, `fp-engine`, `fp-audio`, `fp-input`, `fp-physics`, `fp-render` | The binary: window, loop, CLI, HUD, audio routing |

`fp-vm` keeps `fp-formats` as a **dev-only** dependency so its integration tests
can drive real CNS triggers end to end; this is a one-way edge (`fp-formats` does
not depend on `fp-vm`), so there is no cycle. Likewise `fp-engine` declares
`fp-formats` and `fp-combat` as **dev-only** dependencies — they are used only by
its headless `Match` tests (synthetic `LoadedCharacter`/`HitDef` construction) and
reach runtime transitively through `fp-character`, so they are not runtime edges.
`fp-app` declares `fp-physics` but exercises it only transitively through
`fp-engine`/`fp-combat`.

---

## 2. Core architecture decisions

### 2.1 Fixed 60 Hz timestep + accumulator (render outside the tick loop)

MUGEN runs at exactly 60 ticks/second; we match it for fighting-game
determinism. The host loop accumulates real elapsed time and drains it in fixed
quanta, then renders **once** after the catch-up loop — never inside it.

- `TICK_DURATION = 16_666_667 ns` (1/60 s) — `crates/fp-app/src/main.rs:68`.
- Accumulator drain: `while accumulator >= TICK_DURATION { … accumulator -= TICK_DURATION; }`
  — `crates/fp-app/src/main.rs:1127-1146`.
- Render begins after the loop exits: `renderer.begin_frame()` at
  `crates/fp-app/src/main.rs:1166`, well past the closing brace of the `while`.

```
real frame (variable Δt)
        │
        ▼
 accumulator += Δt
        │
        ▼
 ┌─────────────────────────────┐   drained in fixed 16.667ms steps
 │ while accumulator ≥ TICK:    │   (0, 1, or N catch-up ticks per frame)
 │   Match::tick(p1, p2)        │
 │   play this tick's sounds    │
 │   accumulator -= TICK        │
 └─────────────────────────────┘
        │
        ▼
 cache current AIR sprites
        │
        ▼
 begin_frame → draw fighters → HUD → present     (exactly once per frame)
```

> **Known imprecision (audit #27):** the keyboard is sampled *inside* the
> catch-up loop (`event_pump.keyboard_state()` at
> `crates/fp-app/src/main.rs:1131`), so a frame that drains multiple ticks
> re-reads the same live input on each tick instead of snapshotting once per
> frame. Tracked in [Known issues](known-issues.md).

### 2.2 Struct-based entities (not ECS) — so the VM has direct field access

Entities are plain Rust structs, not an ECS world. A MUGEN character has a fixed,
well-known property set (`pos`, `vel`, `life`, `power`, `ctrl`, `var`/`fvar`
banks, animation cursor, …), and the trigger expression engine must read those
properties directly and cheaply every tick. A struct gives the evaluator
straight field access through the `EvalContext` trait with no component lookup
indirection. The live entity is `Character` (`crates/fp-character/src/lib.rs`);
the two players are distinct `Player` fields on `Match`
(`crates/fp-engine/src/lib.rs`), which is what makes the cross-entity split
borrows in §2.5 sound.

> Helper/projectile entities (a slot-map of child entities) are **not yet
> modeled** — the `Helper`/`Projectile` controllers route to a no-op, and
> `parent`/`helper`/`target`/`partner`/`playerid` redirects resolve to `None`.

### 2.3 CNS expression engine — compile at load, evaluate per tick

Every trigger and every controller parameter in a character's CNS is compiled to
an `fp_vm::Expr` **once, at load time**, and evaluated **per tick** against the
live entity.

- Load-time compile with a never-panic fallback: `CompiledExpr::compile` turns a
  raw expression string into an `Expr`, and a parse failure becomes a **const-0
  fallback with a warning** rather than a crash — `crates/fp-character/src/loader.rs:92`.
  Multi-value params are split on top-level commas into `CompiledParam.components`
  — `crates/fp-character/src/loader.rs:152`.
- Per-tick evaluation: `fp_vm::eval(expr, ctx)` walks the AST against the
  entity's `EvalContext` — `crates/fp-vm/src/evaluator.rs:360`.

> **Implementation note / doc drift to be aware of:** `fp-vm`'s crate name and
> its `lib.rs` docstring (`crates/fp-vm/src/lib.rs:1-6`) describe a *"bytecode
> compiler and stack-based virtual machine."* The shipped implementation is a
> **tree-walk evaluator** — `eval`/`eval_inner` recurse directly over the `Expr`
> AST (`crates/fp-vm/src/evaluator.rs:360-395`); there is no bytecode, opcode
> set, or operand stack in the source. The pipeline is therefore: **lexer
> (`lexer.rs`) → Pratt/precedence-climbing parser (`parser.rs`) → tree-walk
> evaluator (`evaluator.rs`)**. The "bytecode" framing is aspirational and should
> be read as "compiled to an AST at load, evaluated at runtime."

What the engine actually evaluates, all with a never-panic contract that
collapses every error path to `Value::Int(0)`:

- **Value model:** `Value::Int(i32) | Value::Float(f32)` with MUGEN coercions
  (`as_bool` nonzero-is-true, `to_float` int promotion, `to_int` saturating
  truncation). An internal `Eval::Bottom` sentinel carries error state and
  collapses to `0` at the public boundary — `crates/fp-vm/src/eval.rs`,
  `crates/fp-vm/src/evaluator.rs:220-247`.
- **Operators:** full 9-level MUGEN precedence via Pratt climbing; `**`
  right-associative; `/0`, `%0`, `ln(x≤0)` all yield `0`.
- **Redirects:** `parent/root/helper/target/enemy/enemynear/partner/playerid`
  plus `p1→root` and `p2→enemy` aliases, parsed as `Expr::Redirected` binding
  looser than every operator and resolved at eval time by hopping to the target
  context (`crates/fp-vm/src/parser.rs`, `crates/fp-vm/src/evaluator.rs:448`).
- **Member-keyed triggers:** `GetHitVar(member)` and `const(member)` route a bare
  identifier through the `trigger_str` seam.
- The only deliberately-unsupported real-content form is the `:=` assignment
  operator (lexed but not in the AST → a recoverable parse error).

See the [evaluator semantics](knowledge-base/07-evaluator-semantics.md) doc for
the full grammar and numeric rules.

### 2.4 Per-tick executor + state-controller dispatch

A character's behavior each tick is the MUGEN state machine: run the special
common states, then the current numbered state, with every controller gated by
its triggers. This lives in the executor.

- Tick entry: `Character::tick` (the public wrapper) → `tick_with`
  — `crates/fp-character/src/executor.rs:347-364`.
- **MUGEN-order execution:** when not in hitpause, invuln windows tick, then
  special states `-3, -2, -1`, then the current numbered state runs, each in
  order — `crates/fp-character/src/executor.rs:405-454`. Same-tick `ChangeState`
  re-entry is capped at 512 to stop infinite transition loops
  (`run_current_with_transitions`).
- **Controller gating:** `triggerall` is ANDed; numbered `trigger0..N` groups are
  ORed (with CB6 contiguity); `persistent` (0 = once-per-entry, 1 = every,
  n = every-nth) and `ignorehitpause` are honored —
  `gating_passes`/`run_controller` at `crates/fp-character/src/executor.rs:732-794`.
- **Hitpause freeze:** while frozen, only `ignorehitpause`-flagged controllers
  run; animation, state `Time`, and physics are frozen; `shaketime` and invuln
  decrement — `crates/fp-character/src/executor.rs:405-420`.
- **Dispatch:** a single `eq_ignore_ascii_case` if/else chain handles ~30
  controller types; anything else falls to a **debug-logged safe no-op** —
  `crates/fp-character/src/executor.rs:900-981`.
- After controllers: the air-jump engine built-in
  (`update_air_jump`, `:516`), then `apply_physics` → `integrate_position`
  (ground clamp) → `advance_time` → `advance_animation`.

Dispatched controllers (the ~30 with real arms) include `ChangeState`,
`SelfState`, `VelSet/VelAdd/VelMul`, `PosSet/PosAdd`, `CtrlSet`,
`ChangeAnim(+ChangeAnim2)`, `VarSet/VarAdd/VarRangeSet`, `PowerAdd/PowerSet`,
`AttackMulSet/DefenceMulSet`, `StateTypeSet`, `Turn`, `PlaySnd`, `HitDef`,
`NotHitBy/HitBy`, and the `Target*` family. Notable **no-ops / missing arms**:
`Width`, `AssertSpecial`, `SprPriority`, `Pause/SuperPause`, `AfterImage/PalFX`,
`HitVelSet/HitFallSet/HitFallVel/HitFallDamage`, `LifeAdd`, `Helper`,
`Projectile`, `Explod`, `EnvShake` — see the
[audit](knowledge-base/08-faithfulness-audit.md) and [Known issues](known-issues.md).

The loader also builds the state graph the executor runs: it merges all CNS files
first-wins (with `stcommon` last), then merges the `.cmd` `[Statedef -1]`
command→state bridge as a **supplement** (appending controllers), then appends
hardcoded engine ground-locomotion + auto-land controllers to `[Statedef -1]` —
`crates/fp-character/src/loader.rs:481-539`, with `BUILTIN_GROUND_LOCOMOTION_CNS`
at `:775`.

### 2.5 The cross-entity eval keystone: `EvalCtx{me, opponent, stage, anim}` + `Copy` `EvalEnv`

The hard problem: a character's trigger may read the **other** character
(`P2Dist`, `p2, life`, `enemy, statetype`) and the **stage** (screen-edge
distances), yet a `Character::tick` already holds `&mut self`. A naive design
would need two simultaneous mutable/immutable borrows of distinct entities.

The solution — the architectural keystone that unblocked redirects and distance
triggers — is a short-lived, borrow-checked context wrapper:

- **`EvalCtx<'a> { me, opponent, stage, anim }`** —
  `crates/fp-character/src/lib.rs:1911`:
  - `me: &'a Character` — the self target for self-triggers,
  - `opponent: Option<&'a EvalCtx<'a>>` — the other player's context,
  - `stage: StageView` — screen edges for edge-distance triggers,
  - `anim: AnimSet<'a>` — `me`'s loaded `.air` actions, for `SelfAnimExist(n)`.
- **`EvalEnv<'a>` is `#[derive(Clone, Copy)]`** —
  `crates/fp-character/src/executor.rs:122-134`: it bundles
  `{ opponent, stage, anim }` and is threaded *by value* through the whole
  dispatch. Being `Copy` means it costs nothing to pass to every gating check and
  controller call.
- At each eval site the executor reborrows `&*self` into a fresh
  `EvalCtx { me, .. }` that lives only for that one `eval` call and drops before
  any `&mut self` mutation — `eval_ctx` at
  `crates/fp-character/src/executor.rs:810`. This is why the whole thing
  type-checks with **no `unsafe`**.

`EvalCtx` overrides `redirect` to resolve `p2`/`enemy`/`enemynear` → opponent and
`root` → self (`crates/fp-character/src/lib.rs:2158`), and computes
`P2Dist`/`P2BodyDist` (facing-relative X), edge distances, `ScreenPos`,
`P2Life`/`P2StateNo`, and `SelfAnimExist` (`cross_entity_trigger` at `:2038`). A
bare `Character` is a **self-only** `EvalContext` whose `redirect` always returns
`None` (`crates/fp-character/src/lib.rs:1773`) — the cross-entity behavior only
exists inside the `EvalCtx` wrapper. The opponent context is built **one level
deep** (its own opponent is `None`), so nested `p2, p2, …` redirects bottom out —
matching how a non-helper sees the other player.

The two players being **distinct `Player` fields** on `Match` is what makes the
split borrow `(&mut self.p1.character, &self.p2.character)` legal at the call
site (`crates/fp-engine/src/lib.rs:696-699`).

### 2.6 The deferred-effects pattern (`TickReport`)

A character's tick cannot `&mut` another entity (the opponent is borrowed
immutably, and a tick can't reach into the global mixer). So instead of mutating,
a tick **emits requests** into a `TickReport`, and the `Match` applies them after
the tick returns. This mirrors the read-side keystone (§2.5): cross-entity
**reads** go through `Option<&EvalCtx>`; cross-entity/global **writes** go through
`TickReport`.

- `TickReport { sound_requests: Vec<SoundRequest>, target_ops: Vec<TargetOp> }`
  — `crates/fp-character/src/executor.rs:296-321`.
- `PlaySnd` pushes a `SoundRequest` (`:201`, emitted at `:1432`) — the `Match`
  surfaces it to `fp-audio`.
- `Target*` controllers push a `TargetOp` (`:244`, emitted e.g. at `:1485`) — the
  `Match` applies them to the opponent via `apply_target_ops`
  (`crates/fp-engine/src/lib.rs:711-733, 1305-1351`). `Target*` controllers are
  no-ops when `has_target == false`.

`sound_requests` are **replaced** (not accumulated) each tick, so the accessors
always reflect only the latest tick.

### 2.7 Never crash on bad content

Community content is messy, so the whole stack is built to degrade rather than
panic: parsers warn-and-skip malformed lines and bound their linked-list walks;
bad expressions compile to const-0; missing sprites render invisible (palette
index 0 is transparent); a headless audio device falls back to a `NullBackend`;
and bad/missing CLI assets degrade to a checkerboard test pattern. Errors use
`fp_core::FpError` (`Parse`/`NotFound`/`Io`/`Unsupported`); `panic!` is not part
of the content path.

---

## 3. Combat resolution pipeline

Combat is split across three crates by responsibility. **`fp-combat` is pure
data + geometry + a pure decision function — it never mutates a `Character`.**
The actual application of a hit (damage, GetHitVars, hitpause, knockback) lives in
`fp-character::combat::resolve_attack`, which the `Match` drives.

```
   attacker AIR (Clsn1)        defender AIR (Clsn2)
            │                          │
            ▼                          ▼
   ┌─────────────────────────────────────────┐
   │ detect_hit / detect_hit_contact          │  fp-combat/src/lib.rs:878,926
   │  place both box sets to world space       │  (pos + facing mirror), test
   │  (place_clsn) and test strict overlap     │  any_overlap; contact pt for sparks
   └───────────────────┬─────────────────────┘
                       │ overlap?
                       ▼
   ┌─────────────────────────────────────────┐
   │ resolve_hit(&HitDef, DefenderState)       │  fp-combat/src/lib.rs:1231
   │  → HitOutcome  (PURE decision ladder)     │  Guard / Hit / Miss
   │  Guard iff holding_back AND guardflag      │  picks air vs ground vel/hittime
   │  non-empty AND admits stance; else Hit     │  by airborne; gethit_state =
   │  if hitflag admits stance; else Miss       │  p2stateno or common 5000/5010/5020
   └───────────────────┬─────────────────────┘
                       │ HitOutcome
                       ▼
   ┌─────────────────────────────────────────┐
   │ resolve_attack(...)  THE APPLY BRIDGE     │  fp-character/src/combat.rs:130
   │  • i-frame gate: defender.invuln.blocks   │  blocked → None (like a miss)
   │  • hitonce/numhits via move_connect        │
   │  • damage = round(base · attack_mul        │  attacker/defender mults
   │            · defence_mul), clamped ≥ 0     │  (both default 1.0)
   │  • populate GetHitVars on the defender     │
   │  • hitpause/shaketime via max() both sides │  re-arm can't shorten a freeze
   │  • mark move_connect; set p2stateno;       │
   │    set has_target on the attacker          │
   └───────────────────┬─────────────────────┘
                       │ returns the connection (hit_sound, attacker_state)
                       ▼
       fp-engine applies p1stateno to the ATTACKER and appends the
       impact sound to the attacker's sound-request vec  (lib.rs:758-791)
```

**`HitDef` data model** (`crates/fp-combat/src/lib.rs:738-816`) carries
MUGEN-faithful defaults (`hitflag='MAF'`, `chainid=-1`, `hittimes air=20`,
`priority value=4`, `sparkno=-1`) and tolerant never-panic parsers for
`AttackAttr`, `HitFlags`, `AnimType`, `SoundId`.

**GetHitVars populated** by `resolve_attack`
(`crates/fp-character/src/combat.rs:217-247`): `xvel`, `yvel`, `yaccel`,
`animtype`, `damage`, `hitshaketime`, `hittime`, `slidetime`, `ctrltime`, `fall`,
`guarded`, `chainid`. **Not yet populated:** `hit_type` (`GetHitVar(type)`),
`hitcount`, `isbound` (they stay at defaults even though `HitDef` carries the
type data). Note `animtype` *is* now populated (`= (airborne ? air_animtype :
animtype).code()`, `combat.rs:232`).

**Not yet implemented in combat** (see [Known issues](known-issues.md)):
priority/trade clash arbitration (`PriorityType` is data only; combat runs
strictly sequentially P1→P2 then P2→P1), global `Pause/SuperPause` freeze (only
per-character hitpause exists), hit-spark spawning/rendering (anchor is computed
but never emitted), and on-hit power gain/transfer (`getpower`/`givepower`).

---

## 4. The two-player `Match` coordinator

`fp-engine`'s `Match` (`crates/fp-engine/src/lib.rs`) owns the two `Player`s, the
stage bounds, the round state machine, and the per-tick orchestration. `Match::tick`
(`:661`) runs a fixed **6-step order**:

| # | Step | Code |
|---|------|------|
| 1 | **Feed input** (facing-relative) — only while `RoundState::Fight`; otherwise the command source is cleared to `NoCommands` so nothing fires during intro/KO/win | `lib.rs:662-678` |
| 2 | **Tick both state machines**, each with the *other* player as opponent + the `StageView`; capture `sound_requests`; **apply each side's `Target*` ops** to the opponent (2a P1, 2b P2) | `lib.rs:680-733` |
| 3 | **Combat both directions** (P1→P2 then P2→P1) via `resolve_attack`; append impact sound; move the attacker into `p1stateno` | `lib.rs:735-792` |
| 4 | **Push + clamp:** `apply_push_and_bounds` separates overlapping bodies on **X** and clamps each inside `StageBounds` | `lib.rs:794-795, 804-835` |
| 5 | **Face the opponent** for neutral standing/idle characters (a simplified `facep2` baseline — no dedicated 5900 turn state) | `lib.rs:797-798` |
| 6 | **Advance the round** state machine + timer | `lib.rs:800-801` |

P1 ticks first against P2's pre-tick state, then P2 ticks against P1's
just-updated state — matching MUGEN's in-order per-player update. The split
borrows are sound because `p1`/`p2` are distinct fields.

**Ground plane & auto-land — in the executor, not the engine.** The Y clamp to
the ground plane and the data-driven auto-land both live in
`fp-character`'s executor: `GROUND_Y = 0.0` (`crates/fp-character/src/executor.rs:173`)
and `integrate_position` holds `pos.y = pos.y.min(GROUND_Y)` every tick
(`:2114-2119`), leaving `vel.y` untouched so common1's own land transition (state
50/51 → 52) fires from data. `fp-engine`'s `apply_push_and_bounds` touches only
`pos.x`.

**Round flow & best-of-N** (`crates/fp-engine/src/lib.rs:853-1026`, constants
`:220-236`): `Intro` (60 frames) → `Fight` (timer counts down) → `Ko`
(90 frames) → `Win`, then `resolve_match_or_next_round`. KO compares `life ≤ 0`;
time-over compares life; a draw credits neither. First to `rounds_to_win`
(default 2 → best-of-3) wins the match.

```
        ┌─────────┐  60f   ┌────────┐  timer→0 / KO  ┌──────┐  90f  ┌─────┐
        │  Intro  │ ─────▶ │  Fight │ ─────────────▶ │  Ko  │ ────▶ │ Win │
        └─────────┘        └────────┘                └──────┘       └──┬──┘
              ▲                                                        │
              │            resolve_match_or_next_round                 │
              └─────────  (another round if neither at N) ◀────────────┘
                          else → match over, declare winner
```

> **Super meter** is carried across rounds within a match: `reset_fighter_for_round`
> deliberately does *not* reset `power` (`crates/fp-engine/src/lib.rs:1186-1222`).
> Power is added by `PowerAdd`/`PowerSet` controllers and `TargetPowerAdd`; there
> is no power bar in the HUD yet, and no on-hit power gain.

---

## 5. Rendering (`fp-render`, wgpu)

A palette-lookup sprite renderer for 256-color indexed MUGEN sprites. The GPU
does the palette lookup, so palette swaps are cheap (no texture re-upload).

- **Two textures per sprite:** an `R8Unorm` *index* texture (one byte per pixel =
  the palette index) and a `256×1 Rgba8UnormSrgb` *palette* texture
  (`crates/fp-render/src/texture.rs`).
- **WGSL fragment shader** (`crates/fp-render/src/shaders/palette.wgsl`): sample
  the index, **discard index 0 as transparent** (`if (palette_index < 0.002)
  { discard; }`, `:45`), then look up the color in the palette texture (`:49`)
  and multiply by per-vertex alpha.
- **Projection:** orthographic, origin top-left, Y increases downward
  (`crates/fp-render/src/renderer.rs`).
- **Three blend pipelines** built once and selected per draw:
  `Normal` (alpha), `Additive` (`SrcAlpha + One`), `Subtractive` (reverse
  subtract) — `crates/fp-render/src/renderer.rs:214-257, 462-466`.
- **Per-sprite draw:** `draw_sprite` uploads a fresh 4-vertex quad with
  flip_h/flip_v UV swap, scale, center rotation, and alpha
  (`crates/fp-render/src/renderer.rs:372-477`).

> **Caveats:** the renderer is **unbatched** — `draw_sprite` begins a new render
> pass and a fresh bind group per sprite (fine for two fighters + a few HUD
> quads, won't scale to many sprites). The `AnimController` and `TextureAtlas`
> helpers are implemented and tested but **unwired** (the app advances animation
> cursors itself). SFF v1 inline palettes are not yet extracted, so WinMUGEN-era
> art renders colorless (audit #25); PNG-encoded SFF sprites are an explicit
> unsupported stub (audit #35).

The HUD today (`Hud`, `crates/fp-app/src/main.rs:627-731`) is **hand-rolled solid
quads** — two life bars + a centered KO/win marker drawn from 1×1 indexed quads —
*not* a `fight.def`/`fight.sff` screenpack, and there is no text rendering (no FNT
parser yet). The real screenpack belongs to `fp-ui` (still a stub).

---

## 6. Audio (`fp-audio`, rodio)

A rodio-backed channel mixer with MUGEN cut-off semantics and a hardened WAV
decoder (`crates/fp-audio/src/system.rs`, `sound.rs`):

- **Channel cut-off:** non-negative channels stop-then-play on reuse; negative
  channels are always-new and stack (`system.rs:223-241`).
- **Headless-safe:** `AudioSystem::default` falls back to a `NullBackend` when no
  device is available — it never panics (`system.rs:190`).
- **Hardened decode:** `validate_wav_spec` rejects formats rodio would panic on,
  plus a `MAX_DECODED_SAMPLES` budget against oversized data chunks
  (`sound.rs:15-67`).

The app routes per-tick `SoundRequest`s (from `PlaySnd` and `HitDef` impact
sounds) into the mixer after each tick (`crates/fp-app/src/main.rs:399-437,
1134-1142`). Sound *decode* is `fp-audio`'s job; the `.snd` container parse is
`fp-formats`'. **Not yet:** sound looping (read but ignored) and a separate
common/`fight.snd` file (common sounds fall back to the character's own `.snd`).

---

## 7. End-to-end data flow: one tick

Putting it together, here is a single 60 Hz tick of a live fight, from a key
press to a pixel. Steps map to the `Match::tick` order in §4.

```
[fp-app]   while accumulator ≥ TICK_DURATION:                    main.rs:1127
   │  ── (caveat #27: keyboard sampled here, inside the loop) ──
   │  keyboard → MatchInput (absolute directions)                main.rs:1131
   ▼
[fp-engine] Match::tick(p1_input, p2_input)                      lib.rs:661
   │
   │ (1) feed_input facing-relative (only in RoundState::Fight)  lib.rs:662
   │       └─[fp-input] 60-frame ring buffer push;               buffer.rs
   │           CommandMatcher backward-scan recognizes
   │           ~ / $ > + tokens → active command flags           command.rs
   │
   │ (2) p1.character.tick(loaded, Some(&p2.character), stage)   lib.rs:696
   │       └─[fp-character] tick_with:                           executor.rs:360
   │            build Copy EvalEnv{opponent, stage, anim}        executor.rs:122
   │            hitpause? run only ignorehitpause controllers
   │            else: special states -3,-2,-1 then current state executor.rs:430
   │              each controller gated by triggerall/groups,    executor.rs:772
   │              evaluated via a short-lived EvalCtx{me,..}      executor.rs:810
   │                └─[fp-vm] eval(expr, ctx) tree-walk → Value  evaluator.rs:360
   │                    redirects (p2/enemy/root) hop to opponent evaluator.rs:448
   │              dispatched controllers mutate self, OR emit:    executor.rs:900
   │                • PlaySnd     → TickReport.sound_requests
   │                • Target*     → TickReport.target_ops
   │            air-jump built-in; apply_physics; integrate_pos  executor.rs:516
   │              (ground clamp pos.y.min(GROUND_Y));             executor.rs:2119
   │              advance_time; advance_animation
   │       └─ surface p1 sound_requests; apply p1 target_ops→P2  lib.rs:700,711
   │          (repeat for P2 against just-updated P1)             lib.rs:720
   │
   │ (3) combat both directions: resolve_attack(P1→P2, P2→P1)    lib.rs:749
   │       └─[fp-combat] detect_hit (Clsn1×Clsn2 world overlap)  lib.rs:878
   │          → resolve_hit → HitOutcome (Guard/Hit/Miss)        lib.rs:1231
   │       └─[fp-character] APPLY: i-frame gate, damage·muls,     combat.rs:130
   │          GetHitVars, hitpause(max), knockback, p2stateno
   │       └─[fp-engine] attacker → p1stateno; append hit sound  lib.rs:758
   │
   │ (4) apply_push_and_bounds  (separate on X, clamp to stage)  lib.rs:794
   │ (5) face_each_other_when_neutral                            lib.rs:797
   │ (6) advance_round  (Intro→Fight→Ko→Win, best-of-N)          lib.rs:800
   │
   ▼  (back in fp-app, still inside the catch-up loop)
[fp-app]   play this tick's surfaced sound requests              main.rs:1134
   │          └─[fp-audio] decode + channel mixer (cut-off)      system.rs
   │       accumulator -= TICK_DURATION   (loop, drain remaining ticks)
   │
   ▼  (loop exits — ONCE per real frame, outside the tick loop)
[fp-app]   cache each fighter's current AIR-frame sprite         main.rs:1153
   │       begin_frame → clear → draw both fighters → HUD        main.rs:1166
   │          └─[fp-render] R8 index tex + 256×1 palette tex,    texture.rs
   │              WGSL palette lookup, index 0 discarded,         palette.wgsl:45
   │              blend pipeline per sprite                       renderer.rs:462
   │       present frame
```

The two keystones make this work without `unsafe`: cross-entity **reads** flow
through `Option<&EvalCtx>` (§2.5), and cross-entity / global **writes** flow
through `TickReport` (§2.6) for the `Match` to apply.

---

## 8. Clean-room status

Fighters Paradise is a clean-room reimplementation: **no Elecbyte/MUGEN engine
source and no copyrighted assets are shipped or tracked.** Kung Fu Man content
(CC BY-NC 3.0, Elecbyte) is used **only for local development** through a
gitignored `test-assets` symlink; zero content files (`.sff/.air/.cmd/.cns/.def/.snd`)
are git-tracked. The project's own code is MIT-licensed. See the root
[README](../README.md) and [MUGEN compatibility](mugen-compatibility.md) for the
full licensing and trademark notes. KFM content must never be redistributed with
this engine.

---

## 9. Further reading

- [MUGEN compatibility](mugen-compatibility.md) — supported formats, controllers, triggers.
- [Content guide](content-guide.md) — bring-your-own-character authoring.
- [Known issues](known-issues.md) — the live gap list (audit-numbered).
- [Roadmap](roadmap.md) — what's planned and in what order.
- [Development](development.md) — build, test, lint, run.
- Knowledge base: [engine architecture research](knowledge-base/03-engine-architecture.md),
  [evaluator semantics](knowledge-base/07-evaluator-semantics.md),
  [faithfulness audit](knowledge-base/08-faithfulness-audit.md),
  [execution plan](knowledge-base/06-execution-plan.md).
- Format specs: [SFF v2](format-specs/sff-v2.md).
