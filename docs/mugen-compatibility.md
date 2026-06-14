# MUGEN Compatibility Matrix

This is the honest, code-grounded answer to one question: **will my MUGEN character run in
Fighters Paradise?** It enumerates which file formats parse, which state controllers execute,
which triggers resolve, and where our behavior deliberately (or not-yet) diverges from
Elecbyte MUGEN 1.0.

Fighters Paradise is a clean-room reimplementation — no MUGEN engine source or copyrighted
assets are used. The reference behavior below is derived from the public format documentation
and the freely-distributable Kung Fu Man (KFM) character, which drives the engine end to end
today.

> **Status legend**
> - **Done** — implemented and exercised by tests / real KFM content.
> - **Partial** — works for the common case; specific sub-features are missing or simplified (see notes).
> - **Missing** — not implemented; parsed-but-ignored, or returns a safe default (`0` / no-op / invisible).
> - **Stub** — the owning crate is an empty placeholder.

Related reading: [Architecture](architecture.md) · [Content Guide](content-guide.md) ·
[Known Issues](known-issues.md) · [Roadmap](roadmap.md) ·
[Faithfulness Audit (KB 08)](knowledge-base/08-faithfulness-audit.md) · root [README](../README.md).

The audit doc numbers (`#NN`) referenced throughout map to the ranked gap list in
[knowledge-base/08-faithfulness-audit.md](knowledge-base/08-faithfulness-audit.md).

---

## TL;DR — what runs today

A character built like **Kung Fu Man** (SFF v2, indexed-color sprites, standard `common1`-style
states, throws, supers, basic AI gates) loads and plays a full best-of-3 match: movement,
jumping/air-jumping, normals, the signature throw, meter-driven supers, hitpause, i-frames,
hit reactions, and damage scaling all work. What is **not** yet there is mostly *presentation and
long-tail content*: stage backgrounds, real lifebars/screenpack, font text, hit sparks, SFF v1
(WinMUGEN-era) and PNG sprite colors, and a number of cosmetic / advanced controllers.

---

## 1. File formats

Parsers live in [`fp-formats`](../crates/fp-formats). All text parsers tolerate a leading UTF-8
BOM, CRLF line endings, and `;` comments, and follow a strict never-crash policy (warn-and-skip
malformed lines; only an unrecoverable condition returns an error).

| Format | Status | Notes |
|--------|--------|-------|
| **DEF** (`.def`) | **Done** | INI sections + `key=value`, case-insensitive lookup, quote stripping, DEF-relative path resolution. `def.rs`. |
| **AIR** (`.air`) | **Partial** | `[Begin Action N]`, frame lines `group,image,x,y,ticks[,flip[,blend]]`, `Loopstart`, `Clsn1`/`Clsn2` default + per-frame boxes, blend modes `A`/`A1`/`S`/`AS###D###`. **Missing:** extended frame params `scale`, `angle`, `Interpolate` blocks are silently dropped (KFM uses none — audit #39). Both plain `A` and `A1` map to Additive and the `D` (destination) alpha is ignored. `air.rs`. |
| **CMD** (`.cmd`) | **Done** | `[Defaults]` (`command.time`, `command.buffer.time`), `[Command]` blocks with default inheritance. `[Statedef -1]` / `[State -1]` AI sections are deliberately skipped here and handled by the CNS layer + the command→state bridge. `cmd.rs`. |
| **CNS** (`.cns`, `.cns`-in-`.def`) | **Done** | `[Statedef N]` with 13 dedicated header fields + an extras map; `[State N,label]` controllers with `triggerall` + numbered trigger groups; `ignorehitpause`/`persistent` routing. Raw expression strings are preserved (split only on the first `=`), so comparisons inside triggers survive intact. `cns.rs`. See [§4](#4-known-semantic-deviations) for the trigger-group-gap deviation and dropped Statedef headers. |
| **SND** (`.snd`) | **Done** | Elecbyte SND header + sound-directory walk; `(group,sample)` lookup returns the raw RIFF/WAVE payload. PCM decoding is `fp-audio`'s job, not the parser's. `snd.rs`. |
| **SFF** (container) | **Done** | Auto-detects v1 vs v2 from the major-version byte at offset 15. `sff/mod.rs`. |
| &nbsp;&nbsp;↳ SFF **v2** header + directory | **Done** | Real MUGEN 1.0 layout: 512-byte header, 28-byte sprite sub-headers, 16-byte palette sub-headers, LData/TData blocks, on-demand decode with link-following. `sff/header.rs`, `sff/mod.rs`. |
| &nbsp;&nbsp;↳ SFF v2 **Uncompressed (Raw)** | **Done** | Passthrough. |
| &nbsp;&nbsp;↳ SFF v2 **RLE8** | **Done** | Verified against real `kfm.sff`. `sff/compression.rs`. |
| &nbsp;&nbsp;↳ SFF v2 **RLE5** | **Done** | Faithful Elecbyte/Ikemen port; verified against a synthesized fixture (KFM does not use RLE5). `sff/compression.rs`. |
| &nbsp;&nbsp;↳ SFF v2 **LZ5** | **Done** | Faithful Elecbyte/Ikemen port; verified against real `kfm.sff` (every non-empty sprite decodes to exact `width*height`). `sff/compression.rs`. |
| &nbsp;&nbsp;↳ SFF v2 **PNG** (Png8/24/32) | **Missing** | `decompress_png` is an explicit `Unsupported` stub — modern HD PNG-packed sprites are invisible. KFM is RLE/LZ5 so unaffected (audit #35). `sff/compression.rs`. |
| &nbsp;&nbsp;↳ SFF **v1** (inline PCX) pixels | **Partial** | Container + linked-list walk + 8-bit RLE PCX index decode work. Only the common single-plane 8-bit PCX variant is supported. `sff/v1.rs`. |
| &nbsp;&nbsp;↳ SFF **v1** palette | **Missing** | The trailing 768-byte VGA palette is **never read**, so v1 art (intro/ending/motif sprites) decodes pixel indices with **no colors to look up** → renders invisible. This is audit **#25**. KFM's in-match `kfm.sff` is v2 and unaffected. `sff/v1.rs`, `sff/mod.rs`. |
| **FNT** (`.fnt` fonts) | **Missing** | No parser module exists at all (only a doc placeholder). The HUD is hand-rolled colored quads; no glyph/text rendering. Audit **#30**. |
| **ACT** (`.act` palettes) | **Missing** | No parser. KFM uses embedded SFF palettes, not `.act` files (audit #39). |

> **SFF format-tag caveat:** SFF v1 sprites are internally tagged `SpriteFormat::Png8` even
> though the data is RLE PCX. This is cosmetic — the v1 decode path is selected by version before
> the format tag is consulted, and no consumer outside `fp-formats` branches on `.format`.

---

## 2. State controllers

State controllers are dispatched in [`fp-character/src/executor.rs`](../crates/fp-character/src/executor.rs)
by a case-insensitive name match (`dispatch`, executor.rs:900-981). Every controller name **not**
in the table below falls through to a debug-logged **safe no-op** — your character still loads and
runs, the controller simply does nothing.

Controller *parameters* and *triggers* are compiled to an expression AST at load (and evaluated by a
tree-walk evaluator per tick — see the naming note below); a parse failure becomes a const-`0` fallback
with a warning (never a panic).

### Dispatched (Done)

| Controller | Notes |
|-----------|-------|
| `ChangeState` | Same-tick re-entry is capped at 512 transitions to prevent loops. |
| `SelfState` | Implemented as a self-`ChangeState`. The custom-state-detach distinction (entering an opponent's state graph mid-throw) is deferred — see [§4](#4-known-semantic-deviations). |
| `VelSet`, `VelAdd`, `VelMul` | |
| `PosSet`, `PosAdd` | |
| `CtrlSet` | |
| `ChangeAnim`, `ChangeAnim2` | `ChangeAnim2` currently **aliases** `ChangeAnim` (no distinct opponent anim table for custom-state throws). |
| `VarSet`, `VarAdd`, `VarRangeSet` | Integer/float/sys var banks. |
| `PowerAdd`, `PowerSet` | Super-meter mechanism; power carries across rounds within a match. |
| `AttackMulSet`, `DefenceMulSet` | Damage multipliers; applied in `resolve_attack` (audit #19). |
| `StateTypeSet` | |
| `Turn` | |
| `PlaySnd` | Deferred into `TickReport.sound_requests`; applied by the engine. `S`-prefix selects the character's own `.snd`. |
| `HitDef` | Builds the active `HitDef`: attr/hitflag/guardflag/damage/velocities/pausetime/hittimes/p1stateno/p2stateno/fall/priority/id/chainid. |
| `NotHitBy`, `HitBy` | Invulnerability windows (i-frames), consulted before a hit is applied (audit #9). |
| `TargetState`, `TargetBind`, `TargetLifeAdd`, `TargetFacing`, `TargetVelSet`, `TargetVelAdd`, `TargetPowerAdd` | The throw / target system. Deferred into `TickReport.target_ops`; applied to the opponent by the engine. No-ops when the attacker has no target (audit #8). |
| `Null` | Intentional no-op (matches MUGEN). |

### Missing (parsed/ignored or fall to no-op)

| Controller | Status | Notes / audit |
|-----------|--------|---------------|
| `Width` | Missing | Per-state push/collision width; only static `[Size]` exists. #10 |
| `AssertSpecial` | Missing | `NoWalk`/`NoAutoTurn`/`Intro` per-tick flags not asserted. #13 |
| `SprPriority` | Missing | Controller absent; the `sprpriority` Statedef header is also dropped at compile. No sprite layering. #16 |
| `Pause`, `SuperPause` | Missing | No global super-freeze. Hitpause is per-character only, not a whole-match freeze. #24 |
| `AfterImage`, `AfterImageTime`, `PalFX` | Missing | No render tint / after-image trail. #33 |
| `HitVelSet`, `HitFallSet`, `HitFallVel`, `HitFallDamage` | Missing | Get-hit velocity/fall re-translation controllers. **Note:** basic knockback already applies directly in `resolve_attack`, so struck characters *do* move — these only matter inside authored get-hit states. #23 |
| `LifeAdd`, `HitAdd` | Missing | Fall to no-op. |
| `Helper`, `Projectile` | Missing | No helper graph / projectile model. (Blocks `parent`/`helper`/`target` redirects — see [§3](#3-triggers--functions).) |
| `Explod`, `RemoveExplod`, `ModifyExplod` | Missing | No effect-entity system; blocks hit-spark rendering. #17 |
| `EnvShake`, `EnvColor` | Missing | |
| `BindToParent`, `BindToRoot`, `BindToTarget` | Missing | |
| `Offset`, `Trans`, `AngleDraw`, `RemapPal` | Missing | |
| `DisplayToClipboard`, `AppendToClipboard` | Missing | Debug-only. |
| `HitOverride` | Missing | Note: `NotHitBy`/`HitBy` i-frames **are** done; `HitOverride` is not. #9 |

> If a controller you rely on is in the *Missing* list, the character will still load and play —
> that controller's effect just won't happen. See [Known Issues](known-issues.md) for the live
> tracking of these gaps.

---

## 3. Triggers & functions

The expression engine is [`fp-vm`](../crates/fp-vm): a tolerant lexer, a Pratt parser covering the
full MUGEN operator-precedence table, and a tree-walk evaluator with never-panic numeric semantics
(every error path — div/mod by zero, `ln` of a non-positive, unresolved redirect, unknown trigger —
yields `0` rather than crashing). The *self-only* trigger answers come from `fp-character`'s
`EvalContext for Character`; *cross-entity* answers come from the `EvalCtx` wrapper that threads the
opponent + stage into the tick.

> **Naming note:** the crate is named `fp-vm` and some docs call it a "bytecode/stack VM," but the
> current implementation is a tree-walk AST evaluator, not a bytecode machine. Behavior is what this
> table documents; the name is historical.

### Language / operators (Done)

| Feature | Notes |
|---------|-------|
| Arithmetic `+ - * / % **` | `/` truncates for ints, `/0`→`0`; `%` is int-only (sign of dividend), `%0`→`0`; `**` right-associative, int saturates / float `powf`. |
| Comparisons `= != < <= > >=` | Return `1`/`0`. `==` is accepted as an alias of `=`. |
| Logical `&& || !` | `&&`/`||` short-circuit. |
| Bitwise `& | ^ ~` | |
| Range literals `[a,b]` `(a,b)` `[a,b)` `(a,b]` | As RHS of `=`/`!=`, mixed inclusive/exclusive bounds. |
| Functions `cond`, `ifelse`, `abs`, `floor`, `ceil`, `min`, `max`, `sin`, `cos`, `atan`, `exp`, `ln` | `cond`/`ifelse` evaluate only the taken branch. `min`/`max` are type-preserving (see [§4](#4-known-semantic-deviations)). |
| `random` | Reads through an entity-owned RNG seam for rollback determinism — **but no entity overrides it yet**, so `random` currently returns a fixed `0` (audit #28). |
| `var(n)`, `fvar(n)`, `sysvar(n)` | Typed variable banks. |
| `AnimElem = N, op M` | Two-parameter form: `reached(AnimElemTime(N) >= 0) AND (AnimElemTime(N) op M)`. |
| `command = "name"` | Routed through the command-recognition seam (either operand order, with `!=`). |
| Member-keyed `GetHitVar(member)`, `const(member)` | Bare-identifier argument passed verbatim. |

### Self triggers (Done)

`Time`/`StateTime`, `AnimTime`, `Anim`, `AnimElem`, `AnimElemNo`, `AnimElemTime(n)`, `StateNo`,
`PrevStateNo`, `StateType`, `MoveType`, `Physics`, `Ctrl`, `Life`, `LifeMax`, `Power`, `PowerMax`,
`Facing`, `Pos x/y`, `Vel x/y`, `Alive`, `MoveHit`/`MoveGuarded`/`MoveContact`, `HitShakeOver`,
`GetHitVar(member)`, `const(member)`.

`AnimElemTime(n)` works for **any** element via a per-element cumulative start-offset table (returns
negative for not-yet-reached elements) — audit #6.

### Cross-entity triggers (Done — the keystone)

These are answered by `EvalCtx`, which threads `opponent: Option<&EvalCtx>` + stage view + AIR action
set through every evaluation site.

| Trigger | Notes |
|---------|-------|
| `P2Dist x/y` | Facing-relative on X. |
| `P2BodyDist x/y` | Subtracts both characters' front widths on X. |
| `FrontEdgeDist`, `BackEdgeDist`, `FrontEdgeBodyDist`, `BackEdgeBodyDist` | Stage-edge distances (from static stage bounds, not a scrolling camera). |
| `ScreenPos x/y` | |
| `SelfAnimExist(n)` | Resolves against the loaded AIR action table (audit #22). On a bare self-only `Character` (no AIR in view) it returns `0` by design. |
| `P2Life`, `P2LifeMax`, `P2StateNo`, `P2MoveType`, `P2StateType` | Single-token aliases reading the opponent's own field. |
| `Const720p(n)`, `Const1280p(n)` | HD coordinate scaling by the localcoord **width** ratio (e.g. KFM 320-wide → `Const720p` ratio `0.25`). Audit #5. |

### Redirects (Partial)

| Redirect | Status | Resolves to |
|----------|--------|-------------|
| `p2`, `enemy`, `enemynear(_)` | **Done** | The single opponent (standard 1-v-1). |
| `root` | **Done** | Self (a non-helper entity's root is itself). |
| `parent`, `helper(id)`, `target(id)`, `partner`, `playerid(id)` | **Missing** | Resolve to `None` → the redirected sub-expression evaluates to `0`. No helper graph / teams / targeting is modeled yet. |

> Redirects bind looser than every operator and are resolved by evaluating the sub-expression
> against the target context. The opponent context is built **one level deep** (its own opponent is
> `None`), so nested `p2, p2, ...` redirects bottom out at `0`.

### Missing triggers (return safe default `0`)

| Trigger | Notes / audit |
|---------|---------------|
| `RoundState`, `RoundNo`, `RoundsExisted`, `MatchOver`, `GameTime` | Tracked by the engine but **not threaded into the trigger context** — they read `0` forever. Pinned by a regression test. #21 |
| `:=` (assignment operator) | The single deliberately-unsupported real-content form. Lexed but has no AST node → surfaces as a recoverable parse error (the controller line is skipped). |
| `Random` (effective) | Parses and evaluates, but the RNG seam returns a fixed `0` (no entity override). Probabilistic AI is silently deterministic. #28 |

---

## 4. Known semantic deviations

These are places where Fighters Paradise **parses your content but behaves differently** from
Elecbyte MUGEN 1.0. They matter most for authors porting AI logic and edge-case mechanics.

| # | Area | MUGEN behavior | Fighters Paradise behavior | Impact |
|---|------|----------------|---------------------------|--------|
| 1 | **CNS trigger-group gaps** | A numbering gap truncates: `trigger1, trigger2, trigger4` → `trigger4` is dead. | **All** numbered groups are kept, including post-gap ones. The contiguity rule is deferred to the trigger-compilation consumer (backlog CB6). | A controller MUGEN would silently never fire *may* fire here. Rare in clean content. |
| 2 | **Dropped Statedef headers** | `sprpriority`, `juggle`, `facep2`, `hitdefpersist`, `movehitpersist` affect layering, juggle limits, throw facing, and HitDef/MoveHit persistence. | Parsed by `fp-formats` but **dropped at compile** (`CompiledState::from_parsed` omits them) → never applied. | No sprite layering, no juggle cap, throw-facing relies on a simplified neutral-face heuristic. #16 |
| 3 | **`GetHitVar(type)` / `hitcount` / `isbound`** | Populated on every connecting hit. | `animtype`, `damage`, knockback, hittimes, `fall`, `guarded`, `chainid` **are** populated (12 of 15 fields); `type`, `hitcount`, `isbound` stay at `0`. | Combo counters and `GetHitVar(type)` branches read `0`. (`animtype` is correct — the common "every hit plays the Light reaction" bug is fixed.) |
| 4 | **Modulo on floats** | Ikemen coerces float operands to int for `%`. | `%` on any float operand returns `0` (follows the Elecbyte spec, diverges from Ikemen). | Float `%` expressions differ from Ikemen-tuned content. |
| 5 | **`min` / `max` typing** | Ikemen promotes to float. | Type-preserving: `min(int,int)` → int. | Edge-case differences when mixing int/float through `min`/`max`. |
| 6 | **`SelfState` custom-state detach** | `SelfState` vs. opponent-driven custom state are distinct entry paths. | `SelfState` is a plain self-`ChangeState`; the mid-throw detach distinction is deferred. | Custom-state throws work via `TargetState` from the engine, but `SelfState`'s detach nuance is simplified. |
| 7 | **Priority / trade clash** | Two simultaneous `HitDef`s compare priority and apply Hit/Miss/Dodge trade rules. | Combat runs strictly sequentially (P1→P2 then P2→P1) with **no clash arbitration** — both attacks can land. `priority` is parsed but never read. | Trades resolve as double-hits. #20 |
| 8 | **On-hit power gain** | A connecting hit grants `getpower`/`givepower` (damage-proportional by default). | `resolve_attack` does **not** add power on hit. Meter fills only via `PowerAdd`/`PowerSet`/`TargetPowerAdd` and the `poweradd` Statedef header (KFM's actual path). | Default damage-proportional meter gain absent; explicit `PowerAdd` content unaffected. #18 |
| 9 | **Guard pausetime / slidetime / ctrltime** | Distinct `guard.pausetime`, `*.slidetime`, `*.ctrltime` fields. | `HitDef` models no separate guard pausetime; guarded hits reuse `pausetime`. `slidetime`/`ctrltime` mirror the applicable hittime. | Frame-data-precise guard timing differs slightly. |
| 10 | **`AnimElemTail` family** | Per-family element resolution. | Family name is kept on the AST but the evaluator always resolves through `AnimElemTime`. | No observed KFM impact; future per-family logic. |
| 11 | **Input sampling under frame hitches** | (Engine-internal.) | The keyboard is sampled *inside* the catch-up tick loop, so a multi-tick frame re-reads the same snapshot per sub-tick, distorting press-vs-hold edges. | Command timing can be over-counted under frame drops. #27 |
| 12 | **Friction stop-floor** | `friction.threshold` snaps low velocity to zero. | The threshold is exposed via the `const()` seam (so `common1`'s own `abs(vel x) < Const(threshold)` check fires), but there is **no explicit engine-side snap-to-zero line**. | Functionally fine for KFM; mechanically incomplete. #12 |

### Presentation gaps (not deviations, just not-yet-implemented)

These don't change *logic* — your character runs — but the screen looks incomplete:

- **Stages** — [`fp-stage`](../crates/fp-stage) is a stub; matches render over a flat clear color. No `[BGDef]`/`[BG]`/`[Camera]`. #29
- **Lifebars / screenpack** — [`fp-ui`](../crates/fp-ui) is a stub; the HUD is hand-rolled colored quads (two life bars + a KO/win marker), not a `fight.def`/`fight.sff`. No power bar drawn yet. #26, #31
- **Text** — no FNT parser, so timer/round/KO render as colored quads, not glyphs. #30
- **Hit sparks** — the contact point is computed but no spark/Explod entity is spawned or rendered. #17
- **Intro / ending storyboards** — [`fp-storyboard`](../crates/fp-storyboard) parses but never ticks or renders; also blocked on SFF v1 palettes (#25). #32
- **After-image / PalFX tint** — no color-tint render uniform. #33

---

## How to predict whether *your* character runs

1. **Sprites:** SFF **v2** with RLE8/RLE5/LZ5/uncompressed → renders. SFF **v1** or **PNG-packed
   v2** → loads but renders **invisible** (no palette / no decode). See [§1](#1-file-formats).
2. **States:** if your moves use only controllers in the [Dispatched](#dispatched-done) list, they
   execute fully. Anything in [Missing](#missing-parsed-ignored-or-fall-to-no-op) is a silent no-op
   (the character still loads).
3. **AI / triggers:** self + the listed cross-entity triggers resolve. If your AI leans on
   `parent`/`helper`/`target` redirects, `RoundState`/`GameTime`, or `Random`, expect those reads to
   be `0`.
4. **Presentation:** expect no stage, real lifebars, fonts, or sparks yet — gameplay logic is ahead
   of presentation.

For step-by-step guidance on structuring content that runs well today, see the
[Content Guide](content-guide.md). For the live status of every gap above, see
[Known Issues](known-issues.md) and the [Roadmap](roadmap.md).

---

*Clean-room note: Fighters Paradise ships no Elecbyte/MUGEN engine source or copyrighted assets.
Kung Fu Man (CC BY-NC) is used locally for development only and is never tracked or distributed.*
