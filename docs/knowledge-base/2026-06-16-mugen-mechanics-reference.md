# MUGEN Mechanics Reference — Specification → Fighters Paradise synthesis

> **Date:** 2026-06-16 · **Scope:** how core MUGEN gameplay mechanics are *specified*
> in the published Elecbyte documentation, and how each one maps onto (or diverges
> from) Fighters Paradise's current implementation.
>
> **Clean-room provenance.** Everything below is drawn from **(1) public Elecbyte
> documentation and its community mirrors** (cited per-section) and **(2) our own
> repository** (`crates/`, `docs/knowledge-base/01–08`). **No third-party engine
> source** (Ikemen GO or any other implementation) was read or referenced. This is a
> study of the published *spec*, not of anyone's implementation.

## Sources (public, cited inline by `[n]`)

1. Elecbyte trigger reference (mirror): <https://github.com/fanyer/mugen/blob/master/docs/trigger.html>
2. Elecbyte state-controller reference (`sctrls.html`, mirror): `fanyer/mugen` `docs/sctrls.html`
3. Canonical `common1.cns` (mirror): <https://raw.githubusercontent.com/fanyer/mugen/master/data/common1.cns>
4. Canonical Kung Fu Man `kfm.cns` (mirror): `fanyer/mugen` `data/kfm.cns`
5. MUGEN command (`.cmd`) format — mugen-net wiki: <https://mugen-net.work/wiki/index.php/Command>
6. Elecbyte CNS format (`cns.html`, mirror): `fanyer/mugen` `docs/cns.html`
7. Random trigger range (0–999) — Elecbyte expression/trigger docs, corroborated via
   <https://www.angelfire.com/ego/heavens/mugen_triggers.htm> and `mugenarchive.com/docs/beta/exp.html`
8. Our repo: `docs/knowledge-base/08-faithfulness-audit.md`, `docs/known-issues.md`, `crates/`

> Note: `elecbyte.com/mugendocs/*` returns HTTP 403 to automated fetches; the
> *content* there is faithfully mirrored at the `fanyer/mugen` and `mugen-net`
> sources above, which were used instead.

---

## 1. Locomotion

### 1.1 The canonical common-state layout

MUGEN ships a `common1.cns` of **engine-default "common states"** that every
character inherits unless it overrides them. State numbers `0–199` and `5000–5999`
are **reserved** for these; custom states must avoid that range unless deliberately
overriding [3][6]. Confirmed canonical numbers from `common1.cns` [3]:

| Statedef | Meaning |
|---:|---|
| **0** | Stand |
| **10** | Stand → Crouch (transition) |
| **11** | Crouching (held) |
| **12** | Crouch → Stand (transition) |
| **20** | Walk (fwd/back) |
| **40** | Jump Start (jumpstart; air-jump bookkeeping at `time=0`) |
| **45** | Air-Jump Start |
| **50** | Jump Up / airborne |
| **51** | Jump Down (compat placeholder) |
| **52** | Jump Land |
| **100** | Run forward |
| **105 / 106** | Hop backwards / backwards-land |
| **120** | Guard (start) |
| **130 / 131 / 132** | Stand / Crouch / Air guard (holding) |
| **140** | Guard (end) |
| **150–155** | Guard-hit (shaking / knocked-back), stand/crouch/air |
| **170 / 175** | Lose / Draw (time over) |
| **190 / 191** | Pre-intro / Intro |
| **5000/5001** | Stand get-hit (shaking / knocked back) |
| **5010/5011** | Crouch get-hit |
| **5020/5030/5035** | Air get-hit (shake / knocked away / transition) |
| **5040/5050** | Air get-hit (recover-not-falling / falling) |
| **5070/5071** | Tripped |
| **5080–5101** | Downed get-hit chain (shake/knockback/hit-ground/bounce) |
| **5110/5120/5150** | Lying / getting-up / lying-defeated |
| **5200/5201/5210** | Fall-recovery (ground/air) |
| **5500** | Continue-screen animation |
| **5900** | Initialize at round start |

The defining subtlety: **the engine has hard-coded behaviours for *specific* common
state numbers** [3] — e.g. air-jump count is reset at `Statedef 40` `time=0`, defence
is restored at `5120` `time=0`, the lie-down state `5110` auto-advances to get-up
`5120`. A faithful engine must treat these numbers as semantically special, not as
arbitrary author states.

### 1.2 How held directions drive the locomotion graph

MUGEN does *not* hard-wire stand↔walk↔crouch↔jump in C; it expresses the transitions
as ordinary `ChangeState` controllers in `[Statedef -1]` (the command bridge, §2.2)
and in the stand/crouch/walk states, gated on the **`command` trigger** matching the
built-in hold commands `holdfwd`/`holdback`/`holdup`/`holddown` (defined in the `.cmd`
file's `[Command]` block, typically as `/$F`, `/$B`, `/$U`, `/$D` — i.e. *hold* +
direction-detect) [5][3]. Roughly:

- **Stand (0):** holding F/B → Walk (20); holding D → Stand→Crouch (10→11); holding U
  (or `command = "holdup"` and ctrl) → Jump Start (40).
- **Crouch (11):** releasing D → Crouch→Stand (12→0).
- **Jump Start (40):** at `time=0`, `VelSet` from the jump-velocity consts, then
  `ChangeState 50` (airborne). The horizontal jump velocity chosen depends on whether
  F/B is held (`jump.fwd`/`jump.back` vs `jump.neu`).
- **Air (50):** on landing (`Vel y > 0 && Pos y >= 0`) → Jump Land (52) → Stand (0).
- **Air-jump:** while airborne with air-jumps remaining, holding U re-enters 45 → 50.

### 1.3 The `[Velocity]` / `[Movement]` constants (Kung Fu Man, canonical) [4]

```
[Velocity]
walk.fwd      =  2.4
walk.back     = -2.2
run.fwd       =  4.6, 0
run.back      = -4.5,-3.8
jump.neu      =  0,-8.4
jump.back     = -2.55
jump.fwd      =  2.5
runjump.fwd   =  4,-8.1
airjump.neu   =  0,-8.1
airjump.back  = -2.55
airjump.fwd   =  2.5

[Movement]
airjump.num             = 1
airjump.height          = 35
yaccel                  = .44       ; gravity (downward accel per tick)
stand.friction          = .85
crouch.friction         = .82
stand.friction.threshold  = 2       ; |vel x| below this snaps to 0 (stop floor)
crouch.friction.threshold = .05
```

The common states *read these as constants* (`Const(velocity.walk.fwd)`,
`Const(velocity.jump.fwd.x)`, `Const(movement.yaccel)`, …) inside `VelSet`/`VelAdd`,
so locomotion speed is data-driven by each character's own `.cns` rather than
hard-coded [3][4]. `yaccel` is the per-tick gravity added to `Vel y` while airborne;
`*.friction` multiplies `Vel x` each grounded tick; `*.friction.threshold` is the
snap-to-zero floor that lets idle transitions actually fire.

### 1.4 Fighters Paradise: status & gaps

- **Implemented.** Canonical state numbers are recognised; the audit's #3/#4/#14/#15
  items closed `SelfState`, the full forward/back/run/runjump/airjump velocity const
  members, the air-jump entry built-in + `airjump.num` bookkeeping, and the
  ground-plane Y clamp + auto-land in `fp-engine` [8]. Because real content's
  stand/walk common states never `ChangeState` *into each other*, we **inject an
  engine built-in ground-locomotion `[Statedef -1]`** (`append_builtin_ground_locomotion`,
  `crates/fp-character/src/loader.rs:633`) that synthesizes stand↔walk↔crouch→jumpstart;
  it is appended *after* the character's own `[Statedef -1]` so authored specials/run/
  throws win first. Gravity is `0.44` (`fp-physics`), matching `yaccel`.
- **Gap — friction stop-floor.** `friction.threshold` is read and exposed via the
  `Const()` seam (so KFM's own `abs(Vel x) < Const(threshold)` idle guard fires), but
  there is **no engine-side snap-to-zero line** in `apply_physics` — functionally fine
  for KFM, mechanically incomplete (#12) [8].
- **Gap — vertical camera follow** is parsed but the camera scrolls only horizontally
  (#29) [8].

---

## 2. Special moves (`.cmd` command motions + the `[Statedef -1]` bridge)

### 2.1 `.cmd` command-motion syntax [5]

A move is a named input pattern in a `[Command]` block:

```
[Command]
name       = "QCF_x"
command    = ~D, DF, F, x
time       = 15          ; window (ticks) to complete the whole sequence
buffer.time = 1          ; how long the match stays "active" after completing
```

**Tokens** (exact meanings from [5]):

| Token | Meaning |
|---|---|
| `B F U D` + diagonals `DB DF UB UF` | 8-way direction elements |
| `a b c x y z s` | buttons (lowercase; `s` = start) |
| `~` | "the input that immediately follows must be **released**" (negative edge) |
| `/` | "the input that immediately follows must be **held**" |
| `+` | "two or more inputs must be **active at the same time**" (e.g. `a+b`) |
| `$` | direction-**detect** shorthand: `$D` matches D, DF, *or* DB (any of the relevant 4-way set) |
| `>` | the element must **immediately follow** the previous one — no other distinct input frame in between (strict succession) |

**Charge moves** use a *duration* on `~`: `~60$B, F, x` = "hold Back for 60 ticks,
release, then F + x" [5]. (MUGEN's known limitation: it tracks only one charge
variable, not one per direction [5].)

`[Defaults]` (per `.cmd`) sets `command.time` and `command.buffer.time` applied to
commands that omit their own.

### 2.2 The `[Statedef -1]` command→state bridge

`[Statedef -1]` is a special always-running state whose controllers are evaluated
**every tick, in every state**. It is where moves are dispatched [6][3]:

```
[State -1, Hadouken]
type      = ChangeState
value     = 1000
triggerall = command = "QCF_x"
trigger1  = statetype = S          ; only from a standing state
trigger1  = ctrl                   ; only if the character currently has control
```

Key points:
- The **`command` trigger** is true on the tick a named command completes [1][5].
- **`ctrl` gating** is the canonical "can I act?" guard — `trigger1 = ctrl` means the
  move only fires when the character has control (not mid-move, not in hitstun). When
  a move *starts*, its `ChangeState` carries `ctrl = 0` to remove control; the move's
  own states restore it (`ctrl = 1`) when recovery allows [1][2][6].
- `triggerall` must hold for *every* numbered trigger group; `trigger1`, `trigger2`,
  … are OR-ed groups of AND-ed lines [6].

### 2.3 Fighters Paradise: status & gaps

- **Implemented.** `compile_command` / `parse_single_token`
  (`crates/fp-input/src/command.rs`) parse `~` (Release), `/` (Hold), `$`
  (direction-detect), `>` (strict-immediate), and `+` (Simultaneous), plus reversed
  diagonal aliases (`FU`/`BU`/`FD`/`BD`). The matcher buffers `time`/`buffer.time` and
  scans the 60-frame ring backward. `[Statedef -1]` is merged from `.cmd` into the
  state graph (`merge_cmd_statedefs`, `loader.rs:1008`), the **`command` trigger** is
  wired, and **`ChangeState` honours `ctrl`** (`executor.rs:1711`, the `ctrl=` param;
  and the built-in locomotion `ChangeState` is gated on `&& self.ctrl`,
  `executor.rs:1104`).
- **Gap — charge `~NN` duration is NOT parsed.** `parse_single_token` strips the `~`
  but then tries to read the rest (`60$B`) as a button/direction → it would
  `Err("unknown command token: '60$B'")` and the whole command compiles to a const-0
  fallback. **Charge moves are unsupported** (`crates/fp-input/src/command.rs:470`).
  No `min_hold`/duration field exists on the `Hold`/`Release` modifier. This is the
  most actionable `.cmd` gap.
- **Note.** `:=` in-expression assignment now *is* supported (T036,
  `lib.rs:3486`), updating `docs/known-issues.md:378` which still lists it as
  unsupported — that doc line is stale.

---

## 3. Super moves + meter (Power, `PowerAdd`, gating, `SuperPause`)

### 3.1 How Power is specified

- **`Power` trigger** — "Returns the amount of power the player has" (int). The
  documented level idiom: `trigger1 = power >= 1000` is "level 1" [1]. The community
  convention is a **1000-per-level** gauge; max power is `[Data] power = N` in the
  `.cns` (commonly `3000`, i.e. 3 levels — the "3000-per-bar / 1000-per-level"
  idiom). (KFM's shipped `[Data]` sets `life/attack/defence`; `power` defaults when
  unset [4].)
- **Building meter:** `PowerAdd` (value = amount to add) and `PowerSet` (value = set
  absolute) state controllers [2]; attacks also grant meter via the HitDef
  `getpower`/`givepower` params and the `[Statedef]` `poweradd` header (KFM fills its
  bar via the statedef `poweradd`, e.g. `[Statedef 200] poweradd = 10`) [2][8].
- **Gating supers:** a super move's `[Statedef -1]` group adds a power threshold to
  its trigger set, e.g. `triggerall = power >= 1000`, and the move's start controller
  spends it with `PowerAdd = -1000` (or `PowerSet`) [1][2].

### 3.2 `SuperPause`

`SuperPause` freezes the game for a dramatic super-flash [2]. Parameters:
`time` (freeze ticks), `anim`, `sound`, `pos`, `movetime` (ticks the *triggerer* stays
unfrozen), `darken`, `p2defmul` (opponent defence multiplier during the pause),
`poweradd`, `unhittable` (triggerer invulnerable during the pause) [2]. `Pause` is the
plainer "freeze everyone for `time` ticks" controller [2].

### 3.3 Fighters Paradise: status & gaps

- **Implemented.** Power is tracked/clamped and persists across rounds; `power()` /
  `power_max()` accessors and a **blue power bar** in the HUD (#26); `PowerAdd`/
  `PowerSet`, the `[Statedef] poweradd` header, and HitDef `getpower`/`givepower`
  on-hit gain (#18) all land — so supers gate correctly on `power >= threshold` [8].
  `Pause`/`SuperPause` are wired to a **whole-match `Freeze` timer** in `fp-engine`
  (`lib.rs:590`): frozen players' sim *and* the round timer / `GameTime` are held,
  while only the `SuperPause` triggerer keeps ticking (`FreezeExempt`) (#24) [8].
- **Gap — `SuperPause` cosmetic/defence params.** The freeze itself works, but
  `p2defmul`, `darken`, `unhittable`, and the super-flash `anim`/`sound`/`pos` are not
  individually modeled as effects (the freeze is the mechanically important part) [8].
- **Note.** `power` max defaults rather than being read from `[Data] power`; verify
  the loader reads `[Data] power` for non-KFM characters that set a non-3000 gauge.

---

## 4. AI activation — the `AILevel` trigger and the self-AI bug class

### 4.1 What the spec says [1]

> **`AILevel`** — "Returns the difficulty level of the player's AI. If AI is enabled
> on the player, the value ranges from **1 (easiest) to 8 (most difficult)**. If AI is
> **not enabled** on the player, the return value is **0**."

This is the *only* correct switch for "am I engine-controlled?": **`AILevel = 0` ⟺ a
human is driving this player; `AILevel > 0` ⟺ the engine AI is driving it.** The engine
sets it; the character only *reads* it.

### 4.2 The community "cheap AI" idiom

Because old WinMUGEN had no `AILevel`, authors invented a self-detection hack: at round
start the character sets a flag var (e.g. `var(59) = 1`) **only via a path a human can't
trivially reproduce**, or watches for "impossible" input combos, then gates all
AI-only `[Statedef -1]` branches on that var. The pattern is:

```
; AI-only move: only the cheap-AI flag + a random roll lets it fire
triggerall = var(59)            ; cheap-AI flag is set
trigger1   = random <= 300      ; 30% chance this frame (Random ∈ [0,999]) [7]
trigger1   = p2bodydist X < 60
```

The modern, *correct* gate is simply `triggerall = AILevel` (true iff > 0). The two are
interchangeable in intent: **both must be FALSE for a human-controlled player**, so the
human's `[Statedef -1]` only ever fires *human* command branches.

### 4.3 WHY a human player must NOT run the character's self-AI — the evilken bug class

This is the bug we just hit. evilken gates its self-AI on something like
`trigger... = Var(30) = 59` (a cheap-AI flag). The failure mode:

1. **Our VM has no `AILevel` arm.** `grep` across `fp-vm` and `fp-character` finds
   **zero** `AILevel` references; an unrecognised trigger falls through to the
   **unknown-trigger default `Value::DEFAULT` (0)** (`crates/fp-character/src/lib.rs:3470`).
   So for the *modern* idiom, `AILevel` reads 0 → AI branches stay off → harmless for a
   human (but it also means a CPU player can never see a positive `AILevel`).
2. **The cheap-AI var idiom is the real trap.** If the engine (or a character init
   state `5900`) ever *sets* the cheap-AI flag var on a **human** player — or if the
   var defaults to the magic value — then the human's `[Statedef -1]` starts firing
   AI-only branches: the character spontaneously throws supers, walks itself, or locks
   into AI scripts the human never asked for. That is exactly the evilken `Var(30)=59`
   symptom: a *human* P1 running the character's *self-AI*.
3. **Root cause:** the character's self-AI branches must be **unreachable for a
   human-driven player.** In real MUGEN that is guaranteed because (a) `AILevel` is 0
   for humans and (b) the cheap-AI flag is set down a path the human's input can't take.
   An engine that (i) doesn't implement `AILevel` *and* (ii) lets the cheap-AI var
   reach its magic value on a human player will run the self-AI. The fix is to **never
   let a human player's cheap-AI gate evaluate true** — implement `AILevel` returning 0
   for humans / `1..8` for engine-AI players, and do **not** seed the cheap-AI flag var
   for human-controlled players.

### 4.4 Architectural distinction (important)

There are **two** unrelated "AIs":

- **The character's self-AI** — author-written `[Statedef -1]` branches gated on
  `AILevel`/cheap-AI var, running *inside* the character's own state machine. This is
  what must stay dark for humans.
- **Our engine's puppet AI** — `fp_input::CpuAi` (`crates/fp-input/src/ai.rs`), an
  *external* brain that emits an `InputState` (absolute directions + buttons) fed
  through the **same** command-matching path as a keyboard frame. It is deterministic
  (Park–Miller RNG, replay-safe), difficulty-tunable (Easy/Normal/Hard), and is wired
  into `fp-app` as P2 (`main.rs:3521`, `pick_p2_input → ai.decide(obs)`). This is the
  *right* architecture — it drives a character the same way a human does, so it never
  needs the character to detect "I am the AI."

### 4.5 Fighters Paradise: status & gaps — the most actionable AI items

- **GAP (highest-value, currently unimplemented): the `AILevel` trigger is not
  wired.** No arm in `fp-vm`/`fp-character`; it resolves to the unknown-trigger default
  0 for *every* player, human or CPU (`lib.rs:3470`). **Action:** add an `AILevel`
  trigger arm that returns 0 for a human-driven player and the engine AI's difficulty
  (1–8) for a CPU-driven one. This is what lets characters with *modern* AI scripts
  behave (CPU plays its scripted AI), and — combined with not seeding cheap-AI vars on
  humans — closes the self-AI bug class for the *modern* idiom.
- **GAP (the actual evilken trigger): cheap-AI var seeding.** Ensure the engine never
  initialises a human player's vars to a cheap-AI magic value, and confirm `var(n)`
  defaults to 0 at round start (`Statedef 5900`). evilken gates on `Var(30)=59`, so a
  human must start with `Var(30)=0`.
- **Present but incomplete:** `CpuAi` exists and is wired for P2; difficulty is a
  3-level engine knob, not yet plumbed to a `1..8` `AILevel` value. Team modes
  (`--simul`/`--turns`) added a live partner, so multiple AI-driven players are now
  reachable — making the `AILevel` gap more visible.

> Note: `docs/known-issues.md` (#39 row) still says "P2 receives `MatchInput::none()`
> … an idle dummy" — that is **stale**; `CpuAi` now drives P2. The CLAUDE.md banner is
> the current truth.

---

## 5. Extras (brief) — helpers, explods, projectiles, throws

### 5.1 Specification highlights [2]

- **Helper** — spawns a second controllable entity (extra hitboxes, minions, custom
  effects). Key params: `name`, `id`, `pos`, `postype` (p1/p2/front/back/left/right),
  `stateno` (starting state), `keyctrl` (does it read input?), `ownpal` (independent
  palette), `helpertype` (normal/player) [2]. Helpers run their own state machine and
  can `DestroySelf`.
- **Explod** — "Creates game animations such as sparks, dust and other visual
  effects." Params: `anim`, `id`, `pos`, `postype`, `ownpal`, `removetime`, `bindtime`,
  `sprpriority` [2]. Display-only entities (no game-logic effect).
- **Projectile** — `projid`, `projanim`, `projhitanim`, `velocity`, `projremove`,
  `projscale`, plus a HitDef [2]. A self-moving attack with its own contact resolution.
- **Throws** — built from `HitDef` (with `attr` including `T` for throw) + the
  **`Target*`** family: `TargetBind` (pin the caught opponent to a position relative to
  the thrower), `TargetState` (force the opponent into a state number), `TargetLifeAdd`,
  `TargetFacing`, and `p1stateno`/`p2stateno` on the HitDef to put *both* fighters into
  scripted throw states. `ChangeState`'s `ctrl` param (0 = no control) commits the
  thrower [2].

### 5.2 Fighters Paradise: status & gaps

- **Implemented.** The **Explod subsystem**, **helpers** (lifecycle + `DestroySelf`),
  **projectiles**, and the **throw system** (`TargetBind`/`TargetState`/
  `TargetLifeAdd`/`TargetFacing` + `p1stateno` + a targets list) all landed (#8) — KFM's
  signature throw (Statedef 800/810) works. `parent`/`helper`/`target`/`partner`/
  `playerid` redirects resolve in the cross-entity `EvalCtx` [8] (CLAUDE.md banner).
- **Gaps.**
  - **Bare no-id `Proj*` triggers** (`ProjHit` meaning "any of my projectiles", no id)
    evaluate to 0; only `ProjHit<id>` etc. are wired (`parse_proj_trigger`,
    `lib.rs`) [8].
  - **Cosmetic explods/projectiles are excluded from `MatchSnapshot`** by design, so a
    mid-match save-state would drop them (record/replay from round start is unaffected
    since it re-derives them) [8].
  - **Hit sparks:** own-spark infra works, but no `fightfx.sff` loader exists yet and
    the `S`-prefix own-spark id is flattened upstream, so conventional characters
    (incl. KFM) render **no visible spark** (#17) [8].

---

## Top actionable gaps for our engine (ranked)

1. **Implement the `AILevel` trigger** (`fp-vm`/`fp-character`): return **0 for human**
   players, **1–8 for engine-AI** players (plumb `CpuAi`'s difficulty through). This is
   the single missing piece behind the self-AI bug class for the *modern* AI idiom, and
   it's currently entirely unwired (resolves to the unknown-trigger default 0 for
   everyone) — §4.5.
2. **Guarantee human players never satisfy a cheap-AI var gate** — confirm `var(n)`
   defaults to 0 at round init (`Statedef 5900`) and the engine never seeds a magic
   cheap-AI value (e.g. evilken's `Var(30)=59`) for a human-driven player — §4.3.
3. **Parse charge `~NN` command motions** in `parse_single_token`
   (`crates/fp-input/src/command.rs:470`): add a hold-duration to the `Release` token
   so `~60$B, F, x` compiles instead of erroring to a const-0 fallback. Charge moves are
   currently unsupported — §2.3.
4. **Friction snap-to-zero** in `apply_physics` using `*.friction.threshold` (the const
   is read but the engine-side stop-floor line is missing) — §1.4 / #12.
5. **Refresh stale docs:** `docs/known-issues.md` still lists `:=` as unsupported (it
   isn't, §2.3) and P2 as an idle `MatchInput::none()` dummy (it's `CpuAi`-driven now,
   §4.4). The CLAUDE.md banner is the current source of truth; known-issues.md predates
   the AI/team-mode/`:=` work.
