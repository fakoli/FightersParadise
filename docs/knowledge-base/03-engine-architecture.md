# 03 — M.U.G.E.N Engine Architecture (for reimplementers)

A developer-oriented reference for the parts of MUGEN you must reimplement. Field names, value
ranges, and section names are quoted from the Elecbyte docs where possible, corroborated by
independent parsers (`bitcraft/mugen-tools`) and the Ikemen GO reimplementation.

## 0. The mental model

Everything is a **declarative interpreter**:

```
A character = text config (.def/.cns/.cmd/.air) + binary assets (.sff/.snd/.fnt/.act)
The engine   = a fixed-step loop that, each tick, runs every player's state machine,
               evaluates trigger expressions, executes state controllers, resolves
               collisions/hits, and renders.
```

There is **no compiled code per character.** The "hard part" of the engine is not graphics — it is
a correct **expression VM** + **state-machine interpreter** + **combat resolver**.

## 1. Coordinate system & timing

- **Fixed timestep — 60 ticks/second.** `1 tick = 1/60 s`. **Distances in pixels, velocities in
  pixels/tick, accelerations in pixels/tick².** Each tick the engine runs the state machine once
  per player. ([Elecbyte trigger.html](https://www.elecbyte.com/mugendocs/trigger.html))
- **Float semantics matter.** Positions/velocities are floating point. For bit-exact replays /
  rollback netcode, a port must mirror MUGEN's float evaluation order and rounding.
- **`localcoord`** — each character declares its authoring resolution via `localcoord = W, H` in
  `[Info]`; stages use `localcoord` in `[StageInfo]`. The engine has one **game coordinate space**
  (`GameWidth`/`GameHeight` in `mugen.cfg`). Assets authored in one space are **scaled by the ratio
  of target-width to source-width** — e.g. a 320×240 character scaled ×4 into 1280×720.
  ([Elecbyte coordspace.html](https://www.elecbyte.com/mugendocs/coordspace.html))

**1.0 vs 1.1 scaling differences:**
- **1.0:** the `localcoord` *height* is not used; a 4:3 stage at 16:9 is **zoomed in** to fill width.
- **1.1:** OpenGL renderer, native high-res (720p via `.sff` v2), a `StageFit` option (`1` = shrink
  stage to fit; `0` = 1.0 crop-to-width behavior), and explicit `zoomout`/`zoomin` in the stage
  `[Camera]` group for dynamic zoom.

> ⚠️ **Resolution-scaled defaults trap.** Some HitDef defaults are resolution-dependent:
> `yaccel` defaults to `.35 / .7 / 1.4` and `fall.yvelocity` to `-4.5 / -9 / -18` for
> 240p / 480p / 720p. A port must apply these **per the active `localcoord`**, not as constants.

## 2. The character file set

| Ext | Type | Role |
|-----|------|------|
| `.def` | text/INI | Manifest: metadata + pointers to all other files. The only extension-mandatory file. |
| `.cns` | text/INI | Constants + the **state machine** (Statedefs + state controllers). The heart. |
| `.cmd` | text/INI | Command (input) definitions + `[Statedef -1]` command→state logic. |
| `.air` | text/INI | Animations: Actions, frame elements, **Clsn1/Clsn2** boxes. |
| `.sff` | binary | Sprite File Format — images + palettes. v1 and v2. |
| `.snd` | binary | Sound File Format — WAV/PCM by group/index. |
| `.fnt` | binary (v1) / text+SFF/TTF (v2) | Fonts. |
| `.act` | binary | 256-color palette files, referenced by `pal1..pal12`. |

The engine reads any extension as unformatted text *except* `.def`, which must keep its extension.
([MUGEN DB: file formats](https://mugen.fandom.com/wiki/List_of_M.U.G.E.N_file_formats))

### `.def` — manifest
- **`[Info]`**: `name` (unique id), `displayname`, `versiondate`, `mugenversion`, `author`,
  `pal.defaults`, `localcoord`.
- **`[Files]`**: `cmd`, `cns`, `st`/`st0..st9` (state files), `stcommon` (loaded **last** to fill in
  missing common states), `sprite` (SFF), `anim` (AIR), `sound` (SND), `ai`, `pal1..pal12`.
- **`[Arcade]`**: `intro.storyboard`, `ending.storyboard`.

([mugen-net wiki: Character Definitions](https://www.mugen-net.work/wiki/index.php/Character_Definitions))

### `.cmd` — commands
`[Command]` blocks define named inputs. Notation:
- **Directions** (relative to facing): `F B U D UF UB DF DB`.
- **Buttons:** `a b c x y z` + `s` (start).
- **Symbols:** `~` = release, `/` = hold, `+` = simultaneous, `>` = strict immediate sequence,
  `$` = direction-detect (any held). `[Defaults]` sets `command.time` & `command.buffer.time`.
- Commands are consumed by **`[Statedef -1]`** controllers firing `ChangeState` when
  `Command = "..."` matches.

([Tutorial 3](https://mugenarchive.com/docs/beta/tutorial/tutorial3.html))

### `.air` — animations
- **`[Begin Action n]`** defines action number `n`.
- **Frame element:** `group, image, x-offset, y-offset, display-time [, flip] [, alpha]`
  - `group,image` index into the SFF; `display-time` in ticks (`-1` = display indefinitely).
  - Flip (6th field): `H`, `V`, or `VH`. Alpha (7th): `A` (add), `S` (subtract), or `AS<src>D<dst>`
    (values 0–256).
- **`Loopstart`** sets the loop point (without it the action loops from element 1).
- **Collision boxes** (declared *before* the frames they apply to):
  - **Clsn2 = hurt boxes; Clsn1 = attack boxes.** Per-frame: `Clsn1: N` then
    `Clsn1[0] = x1,y1,x2,y2`. Defaults applying to all following frames: `Clsn2Default: N` /
    `Clsn1Default: N`. Box = rectangle `x1,y1,x2,y2`.

([Elecbyte air.html](https://www.elecbyte.com/mugendocs/air.html))

### `.sff` — sprite file (v1 vs v2)
Signature **`ElecbyteSpr`** + version bytes. Sprites indexed by **(group number, image number)**.
256-color **indexed** palettes; a sprite can **link/share** another sprite's palette (and in v1, its
image) via an index field.

**SFFv1** — per-sprite subheader: `next_subfile`(u32), `length`(u32), `axisx`(u16), `axisy`(u16),
`groupno`(u16), `imageno`(u16), `index`(u16, linked sprite), `palette`(u8). **Pixel data is PCX.**

**SFFv2** — header: `signature`, version, `sprite_offset/total`, `palette_offset/total`,
`ldata_offset/length`, `tdata_offset/length`. Sprite node: `groupno`, `itemno`, `width`, `height`,
`axisx`, `axisy`, `index` (linked), `format`, `colordepth`, `data_offset`, `data_length`,
`palette_index`, `flags`. Palette node: `groupno`, `itemno`, `numcols`, `index`, `data_offset`,
`data_length`.
- **Compression `format` values: `0`=raw, `2`=RLE8, `3`=RLE5, `4`=LZ5** (1.1 adds `10`=PNG8,
  `11`=PNG24, `12`=PNG32). RLE8 control bytes have bits 7=0,6=1; RLE5 compresses 5-bit (32-color)
  data; LZ5 is a lossless 5-bit codec (slow compress, fast decompress, good ratio).

> Note: PNG formats (10/11/12) are documented in the ARCHIVE wiki but were absent from the bitcraft
> parser's enum (raw/RLE8/RLE5/LZ5 only) — confirm against a real 1.1 SFF before relying on PNG.

See also the existing `docs/format-specs/sff-v2.md`. Sources:
[bitcraft sff.py](https://github.com/bitcraft/mugen-tools/blob/master/libmugen/sff.py),
[ARCHIVE: SFFv2](https://mugenarchive.com/wiki/MUGEN:SFFv2).

### `.snd` — sounds
Signature **`ElecbyteSnd`**; sounds organized by **(group, index)** holding **WAV/PCM**. `PlaySnd`
references them as `group, index`.

### `.fnt` — fonts
- **v1 (pre-1.0):** single binary `.fnt`, sprite-based bitmap font, ASCII only. System fonts
  `f-4x6.fnt`, `f-6x9.fnt`.
- **v2 (1.0+):** text `.def`-style file. `[Def]` with `type = bitmap | truetype`, `size`, `spacing`,
  `offset`, `file`, `blend` (TTF). Bitmap fonts need an accompanying **SFF** (one glyph per char,
  mapped `0,<ASCII>`, printable 32–126). Width `Fixed` or `Variable`. TrueType supports UTF-8.

([Elecbyte 1.1b1 fonts.html](https://www.elecbyte.com/mugendocs-11b1/fonts.html))

## 3. The state machine (the heart)

A character's behavior is a set of **states**, each a `[Statedef N]` followed by one or more
`[State N, id]` **controller** blocks. ([Elecbyte cns.html](https://www.elecbyte.com/mugendocs/cns.html))

### `[Statedef N]` parameters

| Param | Values | Default | Meaning |
|-------|--------|---------|---------|
| `type` | `S`/`C`/`A`/`L`/`U` | `S` | Stance: standing / crouching / air / lying / unchanged |
| `movetype` | `A`/`I`/`H`/`U` | `I` | Attack / idle / being-hit / unchanged |
| `physics` | `S`/`C`/`A`/`N`/`U` | `N` | Stand-friction / crouch-friction / air (gravity+landing) / none / unchanged |
| `anim` | action no. | — | Animation to switch to on entry |
| `velset` | `x, y` | unchanged | Initial velocity on entry |
| `ctrl` | 0/1 | unchanged | Player control flag |
| `poweradd` | int | — | Add to power bar on entry |
| `juggle` | int | inherit | Juggle points the move costs (required on fresh attacks) |
| `facep2` | 0/1 | 0 | Turn to face opponent on entry |
| `hitdefpersist` | 0/1 | 0 | Keep active HitDef across the transition |
| `movehitpersist` | 0/1 | 0 | Carry move hit/guard/miss status |
| `hitcountpersist` | 0/1 | 0 | Carry combo hit counter |
| `sprpriority` | int | — | Sprite draw layer |

### Controller block structure

```ini
[State N, id]
type = <ControllerName>
triggerall = <expr>      ; optional, AND-combined gate (ALL must be true)
trigger1 = <expr>        ; OR of AND-groups: trigger1, trigger2, ... (each group ANDs internally)
ignorehitpause = 0|1     ; universal: also evaluate during hitpause (default 0)
persistent = 0|1|n       ; universal: 1=every qualifying tick (default), 0=once per entry, n=every nth
<controller-specific params>
```

`id` is arbitrary (used for error reporting). **At least one `trigger1` is required.**

### Per-tick execution order & special states

Processing order each tick: **−3 → −2 → −1 → current state number**, evaluating each block's
controllers top-to-bottom.
- **`[Statedef -1]`** — command/input → state-transition rules (the bridge to `.cmd`).
- **`[Statedef -2]`** — runs every tick (early); runs even when using another player's state data.
- **`[Statedef -3]`** — runs every tick **except** when temporarily using another player's state
  data (e.g. mid-throw custom state).
- Helpers normally lack −1/−2/−3 unless input is enabled.

### State controllers (~100+), by category
([Elecbyte sctrls.html](https://www.elecbyte.com/mugendocs/sctrls.html))

- **State flow:** `ChangeState`, `SelfState`, `StateTypeSet`, `CtrlSet`, `Null`.
- **Animation/display:** `ChangeAnim`, `ChangeAnim2`, `AngleDraw`, `AngleSet/Add/Mul`, `Offset`,
  `SprPriority`, `Trans`, `Width`.
- **Movement/physics:** `PosSet`, `PosAdd`, `PosFreeze`, `VelSet`, `VelAdd`, `VelMul`, `Gravity`,
  `Turn`, `PlayerPush`, `ScreenBound`.
- **Attack/defense:** `HitDef`, `HitAdd`, `HitBy`, `NotHitBy`, `HitOverride`, `HitFallDamage`,
  `HitFallSet`, `HitFallVel`, `ReversalDef`, `AttackDist`, `AttackMulSet`, `DefenceMulSet`.
- **Targets:** `TargetState`, `TargetBind`, `TargetDrop`, `TargetFacing`, `TargetLifeAdd`,
  `TargetPowerAdd`, `TargetVelAdd`, `TargetVelSet`.
- **Health/power:** `LifeAdd`, `LifeSet`, `PowerAdd`, `PowerSet`.
- **Palette/FX:** `PalFX`, `AllPalFX`, `BGPalFX`, `RemapPal`, `AfterImage`, `AfterImageTime`.
- **Explods/projectiles:** `Explod`, `ModifyExplod`, `RemoveExplod`, `ExplodBindTime`, `Projectile`.
- **Helpers/binding:** `Helper`, `DestroySelf`, `BindToParent`, `BindToRoot`, `BindToTarget`.
- **Variables:** `VarSet`, `VarAdd`, `VarRandom`, `VarRangeSet`, `ParentVarSet`, `ParentVarAdd`.
- **Audio:** `PlaySnd`, `StopSnd`, `SndPan`.
- **Game/timing/FX:** `Pause`, `SuperPause`, `EnvShake`, `FallEnvShake`, `EnvColor`, `MoveHitReset`,
  `AssertSpecial`.
- **Debug:** `DisplayToClipboard`, `AppendToClipboard`, `ClearClipboard`.

### Trigger gating (per tick, per controller)

Evaluate **all `triggerall`** first — if any is 0, skip the controller. Then evaluate
`trigger1, trigger2, …` as an **OR of AND-groups** (within a numbered group all lines AND; the
controller fires if any group is fully true). **Skipping a number** (trigger1, trigger2, trigger4
with no trigger3) causes later triggers to be ignored. `persistent` controls re-firing across ticks.

## 4. The trigger / expression system

**Operators** ([trigger.html](https://www.elecbyte.com/mugendocs/trigger.html)):
- Relational: `= != < > <= >=`
- Logical: `&& || !` (bitwise `& | ^ ~` also exist)
- Arithmetic: `+ - * / % **` (exponent)
- Range: `[a,b]` / `(a,b)` inclusive/exclusive, used with `=`/`!=`.
- Functions: `cond(c,t,f)`, `ifelse`, `abs`, `floor`, `ceil`, `sin`, `cos`, `atan`, `exp`, `ln`,
  `min`, `max`, `random`.
- **Types:** int, float, boolean-int (0 = false, nonzero = true).

**Operator precedence (lowest → highest binding)** — validated against Elecbyte `trigger.html` and
the Ikemen GO evaluator; note bitwise binds *looser* than relational (unlike C):

1. `||`  (logical OR)
2. `&&`  (logical AND)
3. `|`  `^`  `&`  (bitwise OR / XOR / AND — left-assoc)
4. `=` `==` `!=` `<` `<=` `>` `>=`  (relational; chains left-assoc, so `a < b < c` ⇒ `(a < b) < c`)
5. `+`  `-`  (additive)
6. `*`  `/`  `%`  (multiplicative; `/` is truncating integer division when both operands are int)
7. `!`  `-`  `~`  (unary prefix)
8. `**`  (exponent — **right-associative**)
9. atoms: literals, identifiers/triggers, `func(args)`, parenthesized exprs, ranges `[a,b]`/`(a,b)`

See [07-evaluator-semantics.md](07-evaluator-semantics.md) for the full numeric semantics (int/float
promotion, 32-bit saturation, `%`/`**` typing, short-circuiting, range/`!=` rules, the Park–Miller
`random` LCG) that the evaluator (task 4.4) must implement.

**Redirections** (query another entity): `parent`, `root`, `helper(id)`, `target(id)`, `partner`,
`enemy`, `enemynear(n)`, `playerid(id)` — written `enemy, P2BodyDist X` / `root, var(0)`.

**Standard triggers (selected):**
- State/anim/time: `StateNo`, `PrevStateNo`, `StateType`, `MoveType`, `Time`, `AnimTime`, `Anim`,
  `AnimElem`, `AnimElemTime`, `AnimElemNo`.
  - ⚠️ `AnimElem = 2` is true **only on the first tick** the anim reaches element 2 (does not
    re-trigger on loops). **The first element is element 1, not 0.**
- Position/distance: `Pos`, `ScreenPos`, `Vel`, `P2Dist`, `P2BodyDist`, `ParentDist`, `RootDist`,
  `FrontEdgeDist`, `BackEdgeDist`.
- Combat feedback: `MoveContact`, `MoveGuarded`, `MoveHit`, `MoveReversed`, `HitCount`,
  `UniqHitCount`, `GetHitVar(...)`.
- Resources/AI: `Life`, `LifeMax`, `Power`, `PowerMax`, `Ctrl`, `AILevel`.
- Game state: `RoundNo`, `RoundState`, `MatchNo`, `MatchOver`, `GameTime`, `GameWidth`, `GameHeight`.
- Input: `Command` (matches a `.cmd` command name).
- Counts: `NumEnemy`, `NumHelper`, `NumPartner`, `NumProj`, `NumTarget`, `NumExplod`.
- Projectiles: `ProjContact`, `ProjHit`, `ProjGuarded`, `Proj*Time`.
- Variables: `var(n)` (int), `fvar(n)` (float), `sysvar`, `sysfvar`.

> **Reimplementation note:** this is exactly what the stubbed **`fp-vm`** must evaluate. The CLAUDE.md
> plan — compile expressions to bytecode at load, run a stack interpreter at runtime — is the right
> shape. Redirections mean the VM needs an *evaluation context* that can resolve "which entity am I
> querying" before reading a trigger.

## 5. The combat model

### `HitDef` — the attack definition

Required **`attr`** = `<state-class>, <attack-string>` where class ∈ `S`/`C`/`A` and the 2-char
attack-string is `{N|S|H}` (normal/special/hyper) + `{A|T|P}` (attack/throw/projectile) — e.g.
`attr = S, NA` (standing normal attack).

Key optional fields (all from [sctrls.html](https://www.elecbyte.com/mugendocs/sctrls.html)):
- **`hitflag`** — combo of `H`/`L`/`A`/`M`(=HL)/`F`(fall, allows juggle)/`D`(hits downed); default `"MAF"`.
- **`guardflag`** — combo of `H`/`L`/`A`/`M`; **empty = unblockable**.
- **`affectteam`** — `B`/`E`/`F`; default `E`.
- **`animtype`** / `air.animtype` / `fall.animtype` — `light`/`medium`/`hard`/`back`/`up`/`diagup`.
- **`priority`** — `prior(1–7, default 4), type(Hit|Miss|Dodge)`.
- **`damage`** = `hit_damage, guard_damage` (default 0,0).
- **`pausetime`** = `p1_pausetime, p2_shaketime` (ticks; default 0,0).
- **`sparkno`** / `guard.sparkno` — spark action; prefix `S` = character's AIR (else common.fx).
  `sparkxy` = `x, y`.
- **`hitsound`** / `guardsound` = `grp, item` (prefix `S` = character SND, else common.snd).
- **`ground.type`** = `High`/`Low`/`Trip`/`None`; `air.type` defaults to ground.type.
- Timing: `ground.slidetime`, `ground.hittime` (0), `air.hittime` (20), `guard.hittime`,
  `guard.ctrltime`, `guard.slidetime`, `airguard.ctrltime`.
- Velocities: `ground.velocity` = `x,y`; `air.velocity` = `x,y`; `guard.velocity` = `x`;
  `airguard.velocity` defaults `(air.x×1.5, air.y/2)`; `yaccel` resolution-scaled (see §1).
- Corner push: `ground.cornerpush.veloff` (default `1.3×guard.velocity`) + air/down/guard/airguard.
- Knockdown/fall: **`fall`** (0/1), `air.fall`, `fall.xvelocity`, `fall.yvelocity` (resolution-scaled),
  `fall.recover` (1), `fall.recovertime` (4), `fall.damage`, `down.velocity`, `down.hittime`,
  `down.bounce`, `forcestand`, `forcenofall`.
- P2 positioning: `mindist`, `maxdist`, `snap`, `p1/p2 sprpriority` (1/0), `p1facing`, `p2facing`,
  `p1getp2facing`.
- State takeover: **`p1stateno`** / **`p2stateno`** (default −1 = no change), `p2getp1state`.
- Chain/juggle/IDs: `id`(≥1), `chainID`, `nochainID`, `hitonce`, `numhits` (1), **`air.juggle`**.
- Resources: `getpower`/`givepower`; `kill`/`guard.kill`/`fall.kill` (default 1).
- FX: `palfx.*`, `envshake.*`, `fall.envshake.*`.

### Hit/hurt boxes (Clsn)

Collision is **per animation frame**, defined in `.air`: **Clsn1 = attack boxes**, **Clsn2 = hurt
boxes**. When a HitDef is active, a hit occurs if **any of the attacker's Clsn1 boxes overlap any of
the defender's Clsn2 boxes.** ([Tutorial 4](https://www.elecbyte.com/mugendocs-11b1/tutorial4.html))

### Guarding

The defender guards if their stance matches the HitDef's `guardflag` (`H` standing / `L` crouching /
`A` air / `M` both ground heights) **and** they're holding back; otherwise the hit lands. Guard
timing from `guard.hittime`/`ctrltime`/`slidetime`; guard damage from `damage`'s second value.

### GetHitVars (defender's read of the incoming hit)

Used inside custom gethit states. Keys include: `xvel`, `yvel`, `yaccel`; `type` (0 none/1 high/2
low/3 trip), `animtype`, `airtype`, `groundtype`; `damage`, `hitcount`, `fallcount`, `hitid`,
`chainid`, `guarded`, `isbound`; `hitshaketime`, `hittime`, `slidetime`, `ctrltime`, `recovertime`;
and a full `fall.*` set.

### Juggle system

Each attack declares a juggle cost (`juggle` in Statedef / `air.juggle` in HitDef); a character has
a per-combo juggle budget (`airjuggle` constant). An air hit only connects if the move's cost ≤
remaining points — preventing infinite air combos.

> **Reimplementation note:** this entire section is the stubbed **`fp-combat`** crate. The hardest
> subtleties are (a) priority/trade resolution when both Clsn1s overlap, (b) the p1stateno/p2stateno
> *state takeover* mechanic (the attacker can force the defender into a specific state/anim), and
> (c) faithful juggle accounting.

## 6. Helpers, projectiles, explods & the object model

**Object model:** every entity is a "player"-like object. **Roots** are the real characters;
**helpers** are spawned sub-objects running their own state machine. Relationships: `root` (origin),
`parent` (immediate spawner), `target` (an opponent you're hitting/holding), `partner`, `enemy` /
`enemynear`. Each helper has an **ID** addressable by triggers (`NumHelper`, `helper(id),...`).

- **`Helper`** params: `helpertype` (`normal`|`player`), `name`, `ID`, `pos`, `postype`
  (`p1`/`p2`/`front`/`back`/`left`/`right`), `facing`, `stateno`, `keyctrl`, `ownpal`,
  `supermovetime`, `pausemovetime`, and a full `size.*` set.
- **`DestroySelf`** removes a helper (`recursive=1` also destroys descendants).
- **`BindTo{Parent,Root,Target}`** glue an object to another's axis.
- **`Projectile`** inherits *all HitDef params* plus: `ProjID`, `projanim`, `projhitanim`,
  `projremanim`, `projcancelanim`, `projscale`, `projremove`, `projremovetime`, `velocity`, `accel`,
  `velmul`, `projhits`, `projmisstime`, `projpriority`, `projsprpriority`, `proj*bound`, `offset`,
  `postype`, `projshadow`, `afterimage.*`.
- **`Explod`** — pure visual effect (sparks, dust, auras). Required `anim`; managed via
  `ModifyExplod`, `RemoveExplod` (by ID), `ExplodBindTime`.

> **Reimplementation note:** helpers are why a struct-based entity store needs a generational arena
> (the `slotmap` dep already in the workspace is the right tool). Helpers can spawn helpers and be
> destroyed mid-tick, so the engine needs stable IDs + safe iteration.

## 7. Common / standard states (`common1.cns`)

`stcommon` (default `data/common1.cns`) is **loaded last** and supplies any common state a character
doesn't override. State-number ranges **0–199** and **5000–5999** are reserved for these.

| State | Meaning | State | Meaning |
|-------|---------|-------|---------|
| 0 | Stand (idle) | 130/131/132 | Stand/Crouch/Air guard (holding) |
| 10/11/12 | Stand→Crouch / Crouch / Crouch→Stand | 140 | Guard end |
| 20 | Walk | 150–155 | Guard-hit shake/knockback |
| 40/45 | Jump start / Air-jump start | 170/175 | Lose (time over) / Draw |
| 50/51/52 | Jump up / down / land | 190/191 | Intro |
| 100 | Run forward | 5000–5002 | Stand get-hit |
| 105/106 | Hop back / land | 5010–5022 | Crouch / air get-hit |
| 120 | Guard start | 5050 | Air get-hit falling |
| | | 5070/5071 | Tripped |
| | | 5080–5101 | Downed get-hit / bounce |
| | | 5110/5120 | Lying down / getting up |
| | | 5150 | Defeated (KO) |
| | | 5200–5210 | Fall recovery |
| | | 5900 | Round init |

([1.1b1 common1.cns](https://github.com/Tunlan/mugen-1.1b1/blob/master/data/common1.cns))

> ⚠️ **Clean-room requirement:** a compatible engine must ship an **equivalent common-state set** so
> characters that don't override these behave correctly — but **we must author our own original
> `common1`-style file**, not copy Elecbyte's. See [05 § Legal](05-reimplementation-roadmap.md#legal--clean-room-guidance).

## Sources

- Elecbyte docs — *CNS*: https://www.elecbyte.com/mugendocs/cns.html
- Elecbyte docs — *State Controller Reference*: https://www.elecbyte.com/mugendocs/sctrls.html
- Elecbyte docs — *Trigger Reference*: https://www.elecbyte.com/mugendocs/trigger.html
- Elecbyte docs — *AIR animation format*: https://www.elecbyte.com/mugendocs/air.html
- Elecbyte docs — *Coordinate Space Notes*: https://www.elecbyte.com/mugendocs/coordspace.html
- Elecbyte docs (1.1b1) — *Fonts* / *Tutorial 4 (HitDef/Clsn)*: https://www.elecbyte.com/mugendocs-11b1/fonts.html , https://www.elecbyte.com/mugendocs-11b1/tutorial4.html
- Tutorial 3 (ticks/attack): https://mugenarchive.com/docs/beta/tutorial/tutorial3.html
- mugen-net wiki — *Character Definitions*: https://www.mugen-net.work/wiki/index.php/Character_Definitions
- ARCHIVE wiki — *SFFv2*: https://mugenarchive.com/wiki/MUGEN:SFFv2
- bitcraft/mugen-tools — *sff.py*: https://github.com/bitcraft/mugen-tools/blob/master/libmugen/sff.py
- Tunlan/mugen-1.1b1 — *common1.cns* / *mugen.cfg*: https://github.com/Tunlan/mugen-1.1b1/blob/master/data/common1.cns
- Ikemen GO: https://github.com/ikemen-engine/Ikemen-GO
