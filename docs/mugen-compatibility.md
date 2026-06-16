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
> - **Stub** — the owning crate is an empty placeholder. *(No format/feature below is a Stub anymore — `fp-stage`, `fp-ui`, and `fp-storyboard` have all graduated.)*

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
hit reactions, and damage scaling all work. The presentation has since caught up substantially:
**SFF v1 palettes + SFF v2 PNG sprites decode**, a **parallax stage** and a **fight.def
screenpack** render, **intro/ending storyboards** play, a **power bar** is drawn, and the
PalFX/AfterImage/Pause/SuperPause/Width/AssertSpecial/SprPriority/get-hit-vel controllers plus
the RoundState/GameTime/MatchOver triggers all execute. What remains is mostly **fidelity
sub-features** on those newly-landed paths (stage tiling/animation, screenpack combo/face,
true afterimage ghosting) and the **forward-looking modes** (team/turns/tag, replay/rollback).

---

## 1. File formats

Parsers live in [`fp-formats`](../crates/fp-formats). All text parsers tolerate a leading UTF-8
BOM, CRLF line endings, and `;` comments, and follow a strict never-crash policy (warn-and-skip
malformed lines; only an unrecoverable condition returns an error).

| Format | Status | Notes |
|--------|--------|-------|
| **DEF** (`.def`) | **Done** | INI sections + `key=value`, case-insensitive lookup, quote stripping, DEF-relative path resolution. `def.rs`. |
| **AIR** (`.air`) | **Partial** | `[Begin Action N]`, frame lines `group,image,x,y,ticks[,flip[,blend]]`, `Loopstart`, `Clsn1`/`Clsn2` default + per-frame boxes, blend modes `A`/`A1`/`S`/`AS###D###`. Extended per-frame `scale`, `angle`, and `Interpolate Offset/Scale/Angle/Blend` lines now **parse** into the typed `Frame` (`scale`/`angle`/`interpolate` fields) — but the renderer does **not yet apply** them (audit #39a, parser-side only; KFM uses none). Both plain `A` and `A1` map to Additive and the `D` (destination) alpha is ignored. `air.rs`. |
| **CMD** (`.cmd`) | **Done** | `[Defaults]` (`command.time`, `command.buffer.time`), `[Command]` blocks with default inheritance. `[Statedef -1]` / `[State -1]` AI sections are deliberately skipped here and handled by the CNS layer + the command→state bridge. `cmd.rs`. |
| **CNS** (`.cns`, `.cns`-in-`.def`) | **Done** | `[Statedef N]` with 13 dedicated header fields + an extras map; `[State N,label]` controllers with `triggerall` + numbered trigger groups; `ignorehitpause`/`persistent` routing. Raw expression strings are preserved (split only on the first `=`), so comparisons inside triggers survive intact. `cns.rs`. See [§4](#4-known-semantic-deviations) for the trigger-group-gap deviation and dropped Statedef headers. |
| **SND** (`.snd`) | **Done** | Elecbyte SND header + sound-directory walk; `(group,sample)` lookup returns the raw payload. The container is codec-agnostic; `SndEntry::format()` / `sniff_sound_format` sniff the payload codec (WAV vs ADX vs unknown — ADX via the `0x80` sync byte or the `(c)CRI` copyright marker) so a consumer can skip an undecodable blob, and `SoundFormat::is_decodable()` is the predicate. **Only WAV/PCM decodes** (in `fp-audio`); **ADX is recognised-but-unsupported** — `Sound::decode` rejects an ADX payload up front with `FpError::Unsupported` and `fp-app` warns-once-and-skips it (never a panic, never a per-tick flood). `snd.rs`, `fp-audio/sound.rs`. |
| **SFF** (container) | **Done** | Auto-detects v1 vs v2 from the major-version byte at offset 15. `sff/mod.rs`. |
| &nbsp;&nbsp;↳ SFF **v2** header + directory | **Done** | Real MUGEN 1.0 layout: 512-byte header, 28-byte sprite sub-headers, 16-byte palette sub-headers, LData/TData blocks, on-demand decode with link-following. `sff/header.rs`, `sff/mod.rs`. |
| &nbsp;&nbsp;↳ SFF v2 **Uncompressed (Raw)** | **Done** | Passthrough. |
| &nbsp;&nbsp;↳ SFF v2 **RLE8** | **Done** | Verified against real `kfm.sff`. `sff/compression.rs`. |
| &nbsp;&nbsp;↳ SFF v2 **RLE5** | **Done** | Faithful Elecbyte/Ikemen port; verified against a synthesized fixture (KFM does not use RLE5). `sff/compression.rs`. |
| &nbsp;&nbsp;↳ SFF v2 **LZ5** | **Done** | Faithful Elecbyte/Ikemen port; verified against real `kfm.sff` (every non-empty sprite decodes to exact `width*height`). `sff/compression.rs`. |
| &nbsp;&nbsp;↳ SFF v2 **PNG** (Png8/24/32) | **Done** | `decode_png` decodes the embedded PNG datastream via the `png` crate: PNG8 → palette indices + PLTE through the indexed path; PNG24/PNG32 → flat RGBA (via `decode_sprite_rgba`), with a `MAX_PNG_PIXELS` decompression-bomb guard. Audit **#35**. `sff/compression.rs`. |
| &nbsp;&nbsp;↳ SFF **v1** (inline PCX) pixels | **Partial** | Container + linked-list walk + 8-bit RLE PCX index decode work. Only the common single-plane 8-bit PCX variant is supported. `sff/v1.rs`. |
| &nbsp;&nbsp;↳ SFF **v1** palette | **Done** | Each 8-bit sprite's trailing 768-byte VGA palette (`0x0C` marker + 256 RGB triplets) is now extracted into the `SffPalette` table; every data-owning sprite contributes its own palette (linked/short sprites reuse the most recent real one). v1 intro/ending/motif art renders with color. Audit **#25**. `sff/v1.rs`, `sff/palette.rs`. |
| **FNT** (`.fnt` fonts) | **Partial** | An FNT v1 parser now exists (`fnt.rs`) and `fp-render` has a `draw_text`/glyph path consumed by the screenpack HUD. **Caveats:** asset-blocked (no real `.fnt` fixture — synthetic-tested), and the *legacy quad HUD* is not yet wired to it. Audit **#30**. |
| **ACT** (`.act` palettes) | **Partial** | A parser exists (`act.rs`, `ActPalette::load`/`from_bytes`) and `SffFile::decode_sprite_rgba_with_palette` now re-tints indexed sprites through an external (ACT/alt-color) palette at the format layer. Runtime *selection* of `pal1`..`pal12` is still the consumer's job — only in-SFF palettes drive the default render path. Audit **#39a**. |

> **SFF format-tag caveat:** SFF v1 sprites are internally tagged `SpriteFormat::Png8` even
> though the data is RLE PCX. This is cosmetic — the v1 decode path is selected by version before
> the format tag is consulted, and no consumer outside `fp-formats` branches on `.format`.

### 1a. Asset-type × engine-variant compatibility matrix

The table above is the *parser*-level view. This matrix is the **per-variant** view that answers
"my content was made for engine X / codec Y — does it work?" It surfaces the variant axis the
file-format table folds away (WinMUGEN vs MUGEN 1.0 vs 1.1 vs IKEMEN GO; the specific SFF/SND/FNT
codecs). It was assembled from a survey of the MUGEN asset taxonomy (community Characters dominate,
plus Stages, Screenpacks/Lifebars, add-on fonts/effects, and Full Games) and grounded against the
`fp-formats` source + tests. Clean-room note: only the *taxonomy* was referenced; every test uses
synthetic fixtures authored from scratch.

> **Variant legend:** **Supported** = decodes/parses and is exercised by tests. **Partial** = the
> common case works, listed sub-features are simplified or parsed-not-applied. **Unsupported** =
> recognised (detected, then skipped/flagged) but not decoded — never a crash, always a safe default.
> **Out of scope** = explicitly not a goal of this engine.

| Asset / variant | Status | Notes |
|-----------------|--------|-------|
| **SFF v1.01** (WinMUGEN, inline PCX) | **Partial** | Container + linked-list walk + 8-bit RLE PCX decode + trailing-palette extraction. Only the common single-plane 8-bit PCX variant; renders in color. `sff/v1.rs`. |
| **SFF v2.00** (MUGEN 1.0) | **Supported** | Full RLE8/RLE5/LZ5/Raw + PNG8/24/32 + RGBA palette path. |
| **SFF v2.01** (MUGEN 1.1) | **Supported** | **Decode parity with v2.00** — the two minor revisions share an identical container/decode layout; only the minor-version byte differs (`parse_header` validates the *major* byte only). Locked by `sff_v2_minor_versions_decode_identically`. |
| SFF v2 codecs: **Raw / RLE8 / RLE5 / LZ5** | **Supported** | RLE8/LZ5 verified against real KFM; RLE5 against a synthetic fixture. `sff/compression.rs`. |
| SFF v2 codecs: **PNG8 / PNG24 / PNG32** | **Supported** | PNG8 → indices + embedded PLTE; PNG24/32 → flat RGBA, with a decompression-bomb guard. |
| SFF **per-sprite / shared palettes** | **Supported** | v1 trailing-PCX palettes and v2 RGBA palette sub-headers (incl. sub-256-color palettes) both resolve; linked palettes follow their link. |
| SFF **external / alt-color palette** (`pal1..pal12` `.act`) | **Partial** | `decode_sprite_rgba_with_palette` re-tints any **indexed** sprite (Raw/RLE8/RLE5/LZ5, v1 PCX, PNG8) through a caller-supplied RGBA palette — the format-layer hook for palette swaps. Truecolor PNG24/32 ignore it (they carry their own color). Runtime *selection* of which `.act` to apply is the consumer's job (audit #39a). |
| **AIR** base (WinMUGEN) | **Supported** | Frames, loop, flip, default + per-frame Clsn, blend `A`/`A1`/`S`/`AS###D###`. |
| **AIR 1.1** extensions (`scale`, `angle`, `Interpolate`) | **Partial** | Per-frame `xscale,yscale,angle` columns and `Interpolate Offset/Scale/Angle/Blend` lines **parse** into the typed `AnimFrame`; the renderer does not yet *apply* them. `air.rs`. |
| **CNS / CMD** state + command files | **Supported** (with deviations) | Statedef/controller/command parsing is complete; see [§2](#2-state-controllers) for unhandled controllers and [§4](#4-known-semantic-deviations) for trigger-group/semantic deviations. Version-specific controllers fall to a safe no-op. |
| **DEF** config | **Supported** | INI sections + case-insensitive `key=value` + DEF-relative paths. |
| **SND** payload: **WAV / PCM** | **Supported** | Decoded + played by `fp-audio`. `SndEntry::format()` reports `Wav` (and `is_decodable()` is `true`). |
| **SND** payload: **ADX** (CRI, classic-MUGEN ports) | **Unsupported** | Detected via the `0x80` sync byte **or** the `(c)CRI` copyright marker (`sniff_sound_format` → `SoundFormat::Adx`). The decode path enforces the skip: `fp_audio::Sound::decode` rejects an ADX payload up front with `FpError::Unsupported`, and `fp-app` warns-once-and-continues (cached as `None`, so no per-tick flood). Never decoded, never a crash. Audit **#T029**. `snd.rs`, `fp-audio/sound.rs`. |
| **ACT** palette (768 / 772-byte) | **Supported** (parse) | Reverse-ordered VGA palette → RGBA, index-0 transparent; both the bare 768-byte and the 772-byte-trailer variants parse. Runtime multi-palette *selection* is the consumer's job. `act.rs`. |
| **FNT v1** bitmap font (WinMUGEN) | **Partial** | Embedded PCX glyph strip + `[Def]`/`[Map]` parse; fed to `fp-render`'s `draw_text`. Asset-blocked (synthetic-tested). `fnt.rs`. |
| **FNT v2** sprite/TTF font (MUGEN 1.0+) | **Unsupported** | **Detected** by its version byte and skipped with a warning (`FntFont::from_bytes` returns `FpError::Unsupported`) — never a crash. `fnt.rs`. |
| **MUGEN 1.1 high-res `localcoord`** | **Partial** | The `localcoord`-width ratio drives `Const720p`/`Const1280p` (see [§3](#cross-entity-triggers-done-the-keystone)); full HD-coordinate stage/sprite scaling is not modeled end-to-end. |
| **Stage** `.def` (`[BGdef]`/`[BG]`/`[Camera]`) | **Partial** | Parsed + horizontal parallax; `tile`/`velocity`/`mask`/`type=anim`/vertical-follow parsed-not-rendered. `fp-stage`. |
| **Screenpack / Lifebar** (`system.def`/`fight.def`) | **Partial** | Typed model + HUD render; `[Combo]`/`[Face]` parsed-not-drawn, single bg layer. `fp-ui`. |
| **IKEMEN GO** extensions (Lua/zss, engine-specific `.cns`) | **Out of scope** | Not a goal of this engine; IKEMEN-only scripting is neither parsed nor executed. |

---

## 2. State controllers

State controllers are dispatched in [`fp-character/src/executor.rs`](../crates/fp-character/src/executor.rs)
by a case-insensitive name match (`dispatch`); the chain now handles ~40 controllers (up from ~30).
Every controller name **not** in the table below falls through to a debug-logged **safe no-op** — your
character still loads and runs, the controller simply does nothing.

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
| `HitOverride` | 8-slot get-hit override (audit #9b). |
| `TargetState`, `TargetBind`, `TargetLifeAdd`, `TargetFacing`, `TargetVelSet`, `TargetVelAdd`, `TargetPowerAdd` | The throw / target system. Deferred into `TickReport.target_ops`; applied to the opponent by the engine. No-ops when the attacker has no target (audit #8). |
| `Width` | Per-state push/collision width override on top of the static `[Size]` widths (audit #10). |
| `AssertSpecial` | Per-tick asserted flags (`NoWalk`/`NoAutoTurn`/`Intro`, …), consulted for the tick and cleared each frame (audit #13). |
| `SprPriority` | Sets sprite draw priority; combined with the now-kept `sprpriority` Statedef header it drives draw-order layering (audit #16). |
| `HitVelSet`, `HitFallSet`, `HitFallVel`, `HitFallDamage` | Get-hit velocity/fall re-translation; the HitDef also carries `fall.damage`/`fall.xvelocity` (audit #23). |
| `Pause`, `SuperPause` | Whole-match freeze — `fp-engine` holds the frozen players + round timer/`GameTime`; only the `SuperPause` triggerer keeps ticking (audit #24). |
| `PalFX` | Drives a color-tint render uniform in the palette shader (audit #33). Caveat: `sinadd`/`PalBright`/`PalContrast`/`Trans` not modeled. |
| `AfterImage`, `AfterImageTime` | Drives a fading motion-smear trail behind the fighter (audit #33). Caveat: it re-uses the current frame stepped back by facing, **not** a true frame-history ghost ring; `TimeGap`/`FrameGap` unmodeled. |
| `Null` | Intentional no-op (matches MUGEN). |

### Missing (parsed/ignored or fall to no-op)

| Controller | Status | Notes / audit |
|-----------|--------|---------------|
| `LifeAdd`, `HitAdd` | Missing | Fall to no-op. |
| `Helper`, `Projectile` | Missing | No helper graph / projectile model. (Blocks `parent`/`helper`/`target` redirects — see [§3](#3-triggers--functions).) |
| `Explod`, `RemoveExplod`, `ModifyExplod` | Missing | No general effect-entity controller. (`fp-engine` *does* have a hit-spark effect entity, but only the attacker-own-spark path is wired and KFM shows no spark — see [§Presentation gaps](#presentation-gaps-not-deviations-just-not-yet-implemented). #17) |
| `EnvShake`, `EnvColor` | Missing | |
| `BindToParent`, `BindToRoot`, `BindToTarget` | Missing | |
| `Offset`, `Trans`, `AngleDraw`, `RemapPal` | Missing | |
| `DisplayToClipboard`, `AppendToClipboard` | Missing | Debug-only. |

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
| `random` | Reads through an entity-owned Park–Miller RNG seam (seeded, re-seedable, serializable for future rollback) and now returns a real value in `[0, 999]` (audit #28). |
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

### Round / match triggers (Done)

| Trigger | Notes / audit |
|---------|---------------|
| `RoundState`, `GameTime`, `MatchOver` | Now threaded into the trigger context: the coordinator pushes a `RoundView` (`round_state`/`game_time`/`match_over`) onto each character every tick, so round-phase gates (intro/win poses) react. #21 |
| `RoundNo`, `RoundsExisted` | Still **Missing** — the round counters are not yet surfaced through `RoundView`; they read `0`. |

### Missing triggers (return safe default `0`)

| Trigger | Notes / audit |
|---------|---------------|
| `:=` (assignment operator) | The single deliberately-unsupported real-content form. Lexed but has no AST node → surfaces as a recoverable parse error (the controller line is skipped). |

---

## 4. Known semantic deviations

These are places where Fighters Paradise **parses your content but behaves differently** from
Elecbyte MUGEN 1.0. They matter most for authors porting AI logic and edge-case mechanics.

> **Resolved since this table was written:** dropped Statedef headers (#16), priority/trade clash
> (#20), on-hit power gain (#18), and per-frame input sampling (#27) have all landed — they are no
> longer deviations and were removed from the rows below (see [Known Issues](known-issues.md) for the
> closed entries). The rows that remain are the deviations that still stand.

| # | Area | MUGEN behavior | Fighters Paradise behavior | Impact |
|---|------|----------------|---------------------------|--------|
| 1 | **CNS trigger-group gaps** | A numbering gap truncates: `trigger1, trigger2, trigger4` → `trigger4` is dead. | **Matches MUGEN** (CB6 done). The CNS parser still keeps all groups, but the trigger consumer applies the contiguity rule at evaluation time (`fp_vm::triggers::active_group_indices`, used by `fp-character`'s executor): groups are walked from `trigger1` and truncated at the first gap. | None — post-gap groups no longer fire. |
| 2 | **`GetHitVar(type)` / `hitcount` / `isbound`** | Populated on every connecting hit. | `animtype`, `damage`, knockback, hittimes, `fall`, `guarded`, `chainid` **are** populated (12 of 15 fields); `type`, `hitcount`, `isbound` stay at `0`. | Combo counters and `GetHitVar(type)` branches read `0`. (`animtype` is correct — the common "every hit plays the Light reaction" bug is fixed.) |
| 3 | **Modulo on floats** | Ikemen coerces float operands to int for `%`. | `%` on any float operand returns `0` (follows the Elecbyte spec, diverges from Ikemen). | Float `%` expressions differ from Ikemen-tuned content. |
| 4 | **`min` / `max` typing** | Ikemen promotes to float. | Type-preserving: `min(int,int)` → int. | Edge-case differences when mixing int/float through `min`/`max`. |
| 5 | **`SelfState` custom-state detach** | `SelfState` vs. opponent-driven custom state are distinct entry paths. | `SelfState` is a plain self-`ChangeState`; the mid-throw detach distinction is deferred. | Custom-state throws work via `TargetState` from the engine, but `SelfState`'s detach nuance is simplified. |
| 6 | **Guard pausetime / slidetime / ctrltime** | Distinct `guard.pausetime`, `*.slidetime`, `*.ctrltime` fields. | `HitDef` models no separate guard pausetime; guarded hits reuse `pausetime`. `slidetime`/`ctrltime` mirror the applicable hittime. | Frame-data-precise guard timing differs slightly. |
| 7 | **`AnimElemTail` family** | Per-family element resolution. | Family name is kept on the AST but the evaluator always resolves through `AnimElemTime`. | No observed KFM impact; future per-family logic. |
| 8 | **Friction stop-floor** | `friction.threshold` snaps low velocity to zero. | The threshold is exposed via the `const()` seam (so `common1`'s own `abs(vel x) < Const(threshold)` check fires), but there is **no explicit engine-side snap-to-zero line**. | Functionally fine for KFM; mechanically incomplete. #12 |

### Presentation gaps (not deviations, just partial)

These don't change *logic* — your character runs — and most now render; what's left are
fidelity sub-features on the newly-landed paths:

- **Stages** — [`fp-stage`](../crates/fp-stage) has graduated from a stub: typed `[StageInfo]`/`[BGDef]`/`[BG]`/`[Camera]` parsing + a horizontal-parallax camera in `fp-app`. **Deferred:** `tile`, per-layer scroll `velocity`, `mask`, `type = anim`, and camera vertical-follow are parsed but not yet rendered; no real Elecbyte stage fixture. #29
- **Lifebars / screenpack** — [`fp-ui`](../crates/fp-ui) has graduated: a typed `fight.def` model + a `ScreenpackHud` renderer (life bars, power bars, names, round announcer, timer) loaded when a `fight.def` is present, with fallback to the hand-rolled quad HUD. A **power bar is now drawn**. **Deferred:** `[Combo]`/`[Face]` parsed-not-drawn, single `bg0` layer, no `fight.def`/`fight.sff` fixture (synthetic-tested). #26, #31
- **Text** — FNT v1 parser + `fp-render` `draw_text` exist and feed the screenpack HUD; asset-blocked (no real `.fnt` fixture) and not yet wired to the legacy quad HUD. #30
- **Hit sparks** — `fp-engine` has a real hit-spark effect entity (spawn/tick/expire/render), but only the **attacker-own** spark path is wired; KFM authors *common* `fightfx` sparks and **no `fightfx.sff` loader exists yet**, and `parse_resource_id` flattens the `S`-prefix own-spark form, so **KFM renders no visible spark** (the hit still lands). #17
- **Intro / ending storyboards** — [`fp-storyboard`](../crates/fp-storyboard) has graduated from parser-only to a `StoryboardPlayer` that ticks scenes, overlaid by `fp-app` during Intro/ending. **Deferred:** per-scene fadein/fadeout, per-scene `clearcolor`, and BGM are not applied, and the intro's fixed-60-frame timer is not tied to storyboard length. #32
- **After-image / PalFX tint** — a PalFX color-tint render uniform (`palette.wgsl`) + a fading AfterImage trail now draw. The trail is a **motion-smear approximation** (no true frame-history ghost ring); `sinadd`/`TimeGap`/`FrameGap`/`Trans`/`PalBright`/`PalContrast` unmodeled. #33

---

## How to predict whether *your* character runs

1. **Sprites:** SFF **v2** (RLE8/RLE5/LZ5/uncompressed **and** PNG8/24/32) and SFF **v1** (inline-PCX
   with its trailing palette) all decode and render in color now. See [§1](#1-file-formats).
2. **States:** if your moves use only controllers in the [Dispatched](#dispatched-done) list, they
   execute fully. Anything in [Missing](#missing-parsed-ignored-or-fall-to-no-op) is a silent no-op
   (the character still loads).
3. **AI / triggers:** self + the listed cross-entity triggers resolve, and `Random`,
   `RoundState`/`GameTime`/`MatchOver` now return real values. If your AI leans on
   `parent`/`helper`/`target` redirects or `RoundNo`/`RoundsExisted`, expect those reads to be `0`.
4. **Presentation:** stage, screenpack lifebars, power bar, fonts, and intro/ending storyboards now
   render, but with fidelity gaps (stage tiling/animation, screenpack combo/face, true afterimage
   ghosting, and KFM's common-`fightfx` hit sparks are not there yet — see [§Presentation gaps](#presentation-gaps-not-deviations-just-partial)).

For step-by-step guidance on structuring content that runs well today, see the
[Content Guide](content-guide.md). For the live status of every gap above, see
[Known Issues](known-issues.md) and the [Roadmap](roadmap.md).

---

*Clean-room note: Fighters Paradise ships no Elecbyte/MUGEN engine source or copyrighted assets.
Kung Fu Man (CC BY-NC) is used locally for development only and is never tracked or distributed.*
