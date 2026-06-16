# Game Engines for 2D Fighting Games — paper notes & synthesis

> **Date:** 2026-06-16 · **Scope:** a close reading and synthesis of one academic
> paper on building a 2D side-scrolling fighting game on top of a general-purpose
> game engine (Godot), plus a "relevance to Fighters Paradise" mapping. The goal is
> to grow our subject-matter knowledge base on *how 2D fighting games are built when
> you sit on an engine that already gives you a scene graph, a tilemap editor, a
> physics body, an animation player, and a built-in finite-state-machine pattern* —
> the exact opposite end of the spectrum from our from-scratch MUGEN reimplementation.
>
> **Provenance.** All findings below come from the single source paper named in
> "Source" (read in full, all 8 pages). Where I relate a finding to our codebase I
> cite our own repo (`docs/architecture.md`, `CLAUDE.md`, `crates/`). I have **not**
> fabricated content; where the paper is thin or its method is weak I say so plainly.

## Source

- **Zhongyu Chang, "Exploring the Application of Game Engine in Creating 2D Fighting
  Game."** *Proceedings of CONF-MLA 2024 Workshop: Securing the Future: Empowering
  Cyber Defense with Machine Learning and Deep Learning.* DOI:
  10.54254/2755-2721/110/2024MELB0110. Pages 71–78 (8 pp.). CC BY 4.0.
  Local copy: `~/Downloads/Exploring_the_Application_of_Game_Engine_in_Creati.pdf`.
- Author affiliation: KangChiao Xi'an Qu Jiang, Xi'an, China. Single-author,
  workshop-track paper.

Citations below are to the paper's own section numbers (e.g. §2.3) and printed page
numbers (p.71–78). Its internal references `[1]–[9]` are listed in §"What the paper
cites" at the end.

---

## Key takeaways (read this first)

1. **This is a build log, not an engine-design paper.** It documents how *one
   developer* assembled a Godot 2D side-scroller with light combat ("fighting game"
   here means a Metroidvania/brawler-ish side-scroller with melee combos and a boar
   enemy — **not** a MUGEN-style versus fighter). Its value to us is as a
   *contrast case*: it shows what you get "for free" from a mature general engine,
   which is precisely the set of subsystems we hand-rolled. (§2, p.72–76)
2. **Finite-State Automaton (FSA) is the central character-control abstraction.**
   The author explicitly reaches for a state machine to add character states
   (wall-slide, wall-jump, attack tiers, hurt, death). This is the same control
   model MUGEN encodes as numbered `Statedef`s and that we run in our per-tick
   executor — strong cross-validation that **state machines are the right spine for
   2D fighter character logic.** (§2.2–2.3, p.74)
3. **Engine built-ins replace whole subsystems we wrote by hand:** sprite-sheet
   animation editor, `TileMap` with auto-tiling + random decoration, `ParallaxBackground`
   for scrolling/infinite backdrops, a 2D `Camera` with follow smoothing, an
   `is_on_floor()` physics body, collision *layers* + *masks* for typed collision,
   and a built-in progress-bar control for health. (§2.1–2.5, p.72–76)
4. **Collision layers/masks are how the paper distinguishes "wall vs cliff vs player."**
   Rather than one AABB test, Godot's bitmask-per-collision-object lets the enemy AI
   ask "did I hit a wall, a cliff edge, or the player?" cheaply. This is a more
   expressive collision model than our single `Clsn1×Clsn2` overlap, and worth noting
   as a technique. (§2.4, p.75–76)
5. **Attack combos are driven by a per-attack timing window + an `is_combo_requested`
   flag inside the attack state.** Press attack again inside the window → advance to
   the next attack tier (1→2→3), each with its own cooldown and damage. This is a
   concrete, reusable combo-state recipe. (§2.4, p.76)
6. **Input/jump feel is treated as a first-class tuning surface:** acceleration/
   deceleration on ground and in air, a jump *buffer* (queue the jump press),
   variable jump *height* keyed to how long/short the jump button is held, and a
   `jump_request_timer`. These are standard "game-feel" techniques the paper applies
   deliberately. (§2.2, p.74)
7. **Evaluation method is weak.** "Results" = 4 testers scoring the game on 6 ad-hoc
   axes out of 90. No baseline, no comparison engine actually measured, n=4. Treat
   the quantitative conclusions as anecdote, not evidence. (§3, p.77)
8. **The thesis of the paper — "engines make fighting games easy" — is asserted, not
   demonstrated against alternatives.** It never benchmarks Godot vs Unity/Unreal
   despite framing itself as a "technical assessment of mainstream fighting game
   engines." The intro promises comparison; the body delivers a single Godot tutorial.
   (§1 vs §2, p.71–76)

---

## 1. What problem the paper claims to address

Framing (§1, p.71–72): the mobile games market is growing; 2D side-scrolling
fighting games are popular ("easy to pick up, hard to master") but — the author
argues — Chinese domestic titles in the genre lag international ones on quality,
technical sophistication, UX, and originality. Two root causes are named:

- developers lack strong technical foundations, especially in **selecting and using
  game engines effectively** (Unity/Unreal support 2D but their potential goes
  under-exploited); and
- content is conservative and under-innovates.

Stated goal: give developers a **systematic guidance framework** — a development
process plus key technical points — for building 2D side-scrolling fighting games on
a game engine. Stated method: literature review + case study + empirical research,
"evaluating current mainstream fighting game engines."

> **Critical note.** The abstract/intro promise an *assessment/comparison of engines*.
> The actual body is a single-engine (Godot) build walkthrough with a 4-person
> playtest. The "comparison of engines" never materializes as data. (Contrast §1
> p.71–72 with §2 p.72–76 and §3 p.77.)

---

## 2. The engine-assisted build process (the paper's real content)

This is the useful core: a step-by-step recipe for standing up a 2D action/fighter in
Godot. I've grouped it by subsystem and kept the concrete technique in each.

### 2.1 Basic project + character sprite setup (§2.1, p.72–73)

- Pixel-art style. Author down-scales the viewport to **1/3 of original size** and sets
  stretch mode to **canvas items** (i.e. integer-ish pixel scaling for crisp pixels).
- Adds a **tile map + tile set** for the level.
- Character authoring loop: pick animation frames from a sprite sheet (Figure 1 shows
  a full action sheet), add a **`Sprite2D`** node, build a **standing animation**, tune
  playback speed to imply movement. Instances the *player scene* and binds buttons →
  animations. **Horizontal flip** of the sprite handles facing left vs right.
  - *Cross-reference to us:* facing-via-horizontal-flip is exactly our `place_clsn`
    facing mirror and `flip_h` UV swap (`docs/architecture.md` §3, §5). Universal 2D
    technique.

### 2.2 Scene, scrolling background, camera, and jump feel (§2.1–2.2, p.73–74)

- **TileMap built-ins:** select an area in the pixel grid, use the engine's "quick
  draw map" function, set a **probability refresh** on tiles to avoid obviously
  repeating patterns, and use the engine's **random function to scatter decorations**
  on a top layer. (§2.2, p.73)
- **ParallaxBackground** with separate inner/outer layers for depth; foreground
  vegetation drawn *in front of* the character (Figure 3). Infinite scrolling is
  achieved by toggling the parallax layer's **mirroring attribute ("MIR")** and
  repeating background images so trees etc. loop seamlessly (Figure 4). (§2.2, p.73–74)
  - *Cross-reference to us:* this is the same parallax model our `fp-stage`
    `[BG]`/`[Camera]` parser targets; tiling/velocity/mirroring are parsed-but-not-
    fully-rendered on our side (`CLAUDE.md` `fp-stage` row). The paper gets all of it
    from one engine node.
- **Camera:** attach the player scene to a 2D camera; write small functions to tune
  follow **speed** and **angle/amplitude**; enable horizontal+vertical follow in
  module settings. (§2.2, p.74)
- **Movement & jump feel** — treated as explicit tuning (§2.2, p.74):
  - tune movement speed and jump height;
  - add **acceleration and deceleration** (ground and air separately) and refine
    turning;
  - add a **countdown timer**, **buffer the jump button press**, and **control jump
    height** by how long/short the jump key is held;
  - ground detection: compare the **`is_on_floor`** property *before and after* the
    move to decide airborne/landed;
  - jump = "as soon as the character touches ground, allow jump" gated through a
    **`jump_request_timer`** node + conditions.
  - *Technique worth keeping:* **variable jump height keyed to button-hold duration**
    and **jump-press buffering** are standard platformer game-feel tricks; both are
    knobs MUGEN exposes via per-state velocity controllers but which we don't
    currently buffer at the input layer.

### 2.3 Finite-State Automaton for character control (§2.2–2.3, p.74)

The pivotal design statement (§2.2, p.74): *"If the player wishes to implement control
functions more easily, they can set up a Finite-State Automaton (FSA). An FSA is a
mathematical model representing finite states and transitions and actions between
those states. Creating this model helps developers add character states more readily,
such as the wall-slide mechanic."*

**Wall-slide** (§2.3, p.74–75):
1. Author a "Wall Sliding" animation in the animation player with keyframes for the
   slide pose; flipping the node's **X-axis scale** orients the slide to the wall on
   either side.
2. Add a **"Wall Sliding" state** to the state machine with logic: while sliding,
   **falling speed is lower than normal fall speed** (friction against the wall), and
   the slide **direction is derived from the contact-surface normal** so the character
   tracks the wall without drifting. (Figure 5.)

**Wall-jump** (§2.3, p.75):
1. Define a "wall jump" state with initial speed parameters.
2. In the state machine's **`GetNextState`** function, detect a jump-key press → switch
   into the wall-jump state.
3. In **`TransitionState`**, set the character's speed/direction so it leaves the wall
   correctly and orients in air.
4. Polish details: optional **slow-motion** to read the move; tune the **facing time
   before the jump** and the **acceleration during the jump** to avoid an unnatural
   S-shaped trajectory; on reaching the opposite wall, **immediately enter the falling
   state** rather than waiting to land — makes the chain feel coherent (Figure 6).

> **Named FSA hook functions** to remember: `GetNextState` (transition selection) and
> `TransitionState` (on-enter effects). This is a clean two-function state-machine
> contract: one function decides the next state, one applies the transition.
> *Cross-reference to us:* maps onto our executor's `ChangeState`/`SelfState`
> dispatch (transition) and trigger-gated controller groups (transition selection) —
> `docs/architecture.md` §2.4. Same shape, different surface.

### 2.4 Enemies, collision layers, and the combo attack module (§2.4, p.75–76)

**Boar enemy** (§2.4, p.75–76):
- Define enemy scene with attributes: **health, attack power, movement speed**, plus
  direction/speed/acceleration logic so it reacts to the environment.
- Add **wall- and cliff-detection nodes** so the enemy turns around / stops at
  boundaries instead of walking through walls or off ledges.
- Use **collision layers + collision masks** to classify what was hit: by assigning
  layers/masks, the enemy can tell **wall vs cliff vs player** apart and respond
  correctly.
  - *Technique worth keeping:* **bitmask-typed collision** (each body advertises a
    layer bitmask; each detector advertises a mask of layers it cares about) is a
    more expressive collision model than a single overlap predicate. It cheaply
    answers "what *kind* of thing did I touch?" — useful for AI edge/wall avoidance
    and for separating attack boxes from terrain.

**Three-tier combo attack** (§2.4, p.76) — concrete recipe:
- Implement three attack actions (1st/2nd/3rd), each with its own animation, its own
  **attack window**, and a bound attack key.
- Different **cooldown times between attacks** for continuity/realism.
- In the attack animation, **set a time window**; the player must press attack again
  **within that window**. If the input system detects the press, **enter the attack
  state and run the attack logic** (play animation, judge the hit, compute damage).
- Attack-state transition logic: judge the attack key → play animation → handle state
  transition; use variables to record attack conditions and an **`is_combo_requested`
  flag** to switch between attack tiers. Consecutive presses → combo.
- All three attack states **uniformly handle the physical logic** of the three-tiered
  attack so each correctly interacts with the enemy's collision volume and applies
  damage by attack type/power.
  - *Cross-reference to us:* this is a hard-coded version of MUGEN's data-driven
    chain/`chainid` + per-`HitDef` `attack`/`damage`/hit windows. We carry `chainid`
    in `HitDef` and GetHitVars (`docs/architecture.md` §3). The paper inlines combo
    routing as imperative state code; MUGEN/us externalize it as data.

### 2.5 Injury, death, HUD, slide-tackle, interactables (§2.5–2.6, p.76–77)

- **Damage/death** (§2.5, p.76): record + clamp enemy HP (value range for the "blood
  volume"); make injury and death animations; on `health == 0`, **delete the enemy**;
  switch between injured/death states by modifying properties; color-change + keyframes
  mark the death (Figure 7 shows the boar dissolving to a white silhouette).
- **HUD** (§2.5, p.76): a status panel with **player avatars + health bars**. Built
  from a container node that rows child controls; avatar via an Atlas texture sized to
  a frame; the **health bar uses one of the engine's two built-in progress-bar forms**,
  styled by adjusting the progress + border textures; a **custom signal** updates the
  bar when health changes.
  - *Cross-reference to us:* we hand-roll the life/power bars from 1×1 indexed quads
    (`Hud`) and have a `fight.def` screenpack path (`docs/architecture.md` §5). The
    paper gets a health bar from a stock control + a signal.
- **Slide tackle** (§2.6, p.76): three sub-phases — *lying down → sliding shovel →
  standing up* — each with an animation; tune speed + entry conditions; **extend the
  animation playback time** before entering the slide state to nail the timing.
- **Interactable objects** (§2.6, p.76–77): doors, stone tablets, vegetables/mushrooms.
  Add a rectangular collision area + an **`INTERACTABLE`** node; adjust collision
  layer/mask; add key-prompt animation + interaction logic. The "interacting" variable
  was changed from a scalar to an **array** with handlers so the player can interact
  with **multiple objects simultaneously**. **Dead-state handling:** when the state
  machine is in the dead state, a `clear` method empties the interactable array so a
  dead player can't interact. Optimization example: a boar hit knocks the player down,
  after which interaction is disabled.
  - *Technique worth keeping:* the **interactable-array + clear-on-death** pattern is a
    tidy way to model "what can I touch right now," and the explicit dead-state gate is
    a good defensive habit (don't let inputs fire in invalid states) — analogous to our
    `RoundState::Fight` gate that clears the command source outside the fight
    (`docs/architecture.md` §4 step 1).

---

## 3. Results / evaluation (§3, p.77) — and why to discount it

Method: **4 testers** of "different ages" rate the game on **6 axes** — Appearance,
Detail, Gameplay, Vulnerability feedback, First impression, Like comparison — each out
of 15, total out of 90. Scoring rubric is three bands: **Excellent = 15**, **Moderate
= 10**, **Poor = 5**.

Reported scores (Table 1, p.77):

| Tester | Appearance | Detail | Gameplay | Vuln. feedback | First impression | Like comparison | Total /90 |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | 15 | 15 | 15 | 10 | 10 | 10 | **75** |
| 2 | 15 | 15 | 10 | 15 | 10 | 15 | **80** |
| 3 | 10 | 15 | 15 | 15 | 10 | 15 | **80** |
| 4 | 10 | 15 | 10 | 15 | 10 | 10 | **70** |

Author's reading: **First impression scored lowest** (everyone gave 10) and **Detail
scored highest** (everyone gave 15); Godot-made games tend to have high gameplay
**detail**, but **players need time to get used to the game's logic and graphics**.

> **Critical note.** n=4, no control/baseline, no second engine measured, bands are
> coarse (only 5/10/15 possible), and the axes ("Like comparison", "Vulnerability
> feedback") are undefined. This supports *anecdote* ("the prototype is playable and
> looks finished") but **not** the paper's headline claim that engines make fighters
> measurably better/easier. The most defensible takeaway is qualitative: *a solo dev
> shipped a polished-looking, playable 2D action prototype on Godot.*

---

## 4. Conclusions the paper draws (§4, p.78)

- Demonstrated using **Godot** to build a 2D horizontal fighting game end-to-end, with
  real cases/problems from production; result is a "relatively complete game."
- Predicts Godot's **community platform + plug-in modules** will mature and meet
  growing dev needs; expects **more advanced graphics/physics, cross-platform support,
  and VR/AR** as future directions.
- Optimistic thesis: *"in the future, making a fighting game will become very easy;
  people can play their own games."*
  - *Resonance with us:* "people can play their own games" is squarely our mission —
    **bring-your-own MUGEN content** (`CLAUDE.md`). The paper reaches that goal via a
    fat general engine; we reach it via a faithful content runtime. Same end, opposite
    architecture.

---

## 5. Relevance to Fighters Paradise

### 5.1 The headline contrast: fat engine vs. faithful runtime

The paper's whole method is **"lean on the engine's built-ins"**: TileMap editor,
ParallaxBackground, Camera2D, animation player, physics body (`is_on_floor`),
collision layers/masks, progress-bar control, signals. Fighters Paradise is the
**inverse**: we hand-build each of these in dedicated crates because our contract is to
faithfully *run unmodified MUGEN content*, where behavior must come **from the data
files, not from engine defaults** (`docs/architecture.md` §2.7, `CLAUDE.md`). The paper
is therefore most useful to us as a **catalog of subsystems a mature 2D engine
provides** — a checklist of "what a general engine gives you for free" against which we
can sanity-check our own coverage. Mapping:

| Paper's Godot built-in | Our equivalent | Notes |
|---|---|---|
| FSA / state machine for character control (§2.2–2.3) | Per-tick executor + numbered `Statedef` dispatch (`fp-character`, arch §2.4) | **Strong validation**: both converge on state machines. Their `GetNextState`/`TransitionState` ≈ our trigger-gated `ChangeState`. |
| Sprite2D + animation player (§2.1) | AIR animation + `fp-render` sprite path (arch §5) | We parse `.air`; Godot edits sheets in-IDE. |
| TileMap + auto-tile + random decoration (§2.2) | *(no equivalent — MUGEN stages aren't tilemaps)* | Our stages are `[BG]` layers, not tiles. |
| ParallaxBackground + mirror-for-infinite-scroll (§2.2) | `fp-stage` `[BG]`/`[Camera]` parallax (partial) | We *parse* tiling/velocity/mirror; not all rendered yet (`CLAUDE.md`). Paper gets it from one node. |
| Camera2D follow w/ tuned speed/angle (§2.2) | `fp-app` parallax camera (arch §1, fp-stage) | Ours is stage-driven, not a free follow cam. |
| `is_on_floor` physics body (§2.2) | `GROUND_Y=0` clamp in executor (arch §4) | We use a flat ground plane, not a physics body. |
| Collision **layers + masks** (§2.4) | Single `Clsn1×Clsn2` AABB overlap (`fp-combat`, arch §3) | **Their model is more expressive** — typed collision (wall/cliff/player). MUGEN uses Clsn1=attack/Clsn2=hurt only; terrain isn't collided. |
| Progress-bar control + signal for HP (§2.5) | Hand-rolled quad life/power bars + `fight.def` HUD (arch §5) | We draw bars from indexed quads. |
| `is_combo_requested` + per-attack window (§2.4) | `HitDef` `chainid` + GetHitVars (arch §3) | Theirs is imperative; ours is data-driven. |

### 5.2 Techniques worth importing or remembering

These are genuinely portable ideas (none require adopting an engine):

1. **Jump-press buffering + variable jump height by hold duration** (§2.2). Our input
   layer is a 60-frame ring buffer + command matcher (`fp-input`); we do **not** buffer
   a jump press or modulate height by hold. MUGEN content normally encodes jump
   velocity per-state, but if we ever add engine-default locomotion polish (we already
   inject `BUILTIN_GROUND_LOCOMOTION_CNS`, arch §2.4), buffered/variable jump is a known
   good feel-tuning recipe.
2. **Bitmask-typed collision (layers/masks)** (§2.4) as a mental model if we ever add
   **terrain/wall collision** for stages (today only player-vs-player push + stage
   bounds exist, arch §4). Typed collision cleanly separates "hit a wall" from "hit the
   opponent."
3. **Two-function state-machine contract** `GetNextState` / `TransitionState` (§2.3) as
   a clean naming/shape reference — it isolates *which state next* from *what happens on
   enter*. Our executor blends both into trigger-gated controllers; the separation is a
   useful lens when reasoning about transition bugs.
4. **Interactable-array + clear-on-death gate** (§2.6) — defensive state-gating so
   inputs can't fire in invalid states. We already do the round-flow version of this
   (clearing commands outside `RoundState::Fight`, arch §4); the pattern generalizes.
5. **Slide/dissolve death via color-change keyframes** (§2.5, Fig.7) — a cheap,
   data-authored death effect; relevant to how community characters author KO/death
   animations we must render.

### 5.3 What the paper does *not* help with (our hard problems)

The paper's "fighting game" is a **PvE side-scroller**, so it is silent on everything
that makes a *versus fighter* (and a faithful MUGEN engine) hard:

- **No deterministic fixed-timestep discussion.** It never mentions a 60 Hz tick,
  accumulator loops, or determinism — central to us (arch §2.1) and to any competitive
  fighter.
- **No netcode** (rollback/delay-based) at all.
- **No frame-data model** (startup/active/recovery, hit/block advantage) — the
  competitive heart of versus fighters and a known gap area for us.
- **No data-driven content pipeline.** Everything is authored in-engine (scenes,
  scripts, nodes). The entire premise of MUGEN/Fighters Paradise — *run arbitrary
  third-party character files unmodified* — is absent. The paper's characters are
  hand-coded; ours are interpreted from `.cns`/`.cmd`/`.air`.
- **No two-player versus**, no rounds/best-of-N, no guard/block stance system, no
  super meter, no throws/helpers/projectiles. Its combat is one player vs. AI mobs.
- **No expression/trigger language.** Behavior is GDScript; there is nothing like our
  CNS trigger compile→eval pipeline (arch §2.3).

So for our actual roadmap (frame data, netcode/rollback, helper/projectile entities,
team modes), this paper offers **no technical guidance** — only the FSA-as-spine
validation and the game-feel/collision techniques above.

---

## 6. One-line synthesis

A solo-dev Godot build log for a 2D PvE action side-scroller; its durable contributions
are **(a) confirming the finite-state-machine as the correct character-control spine for
2D fighters**, **(b) a tidy three-tier combo recipe (`is_combo_requested` + per-attack
timing windows)**, **(c) game-feel tuning knobs (jump buffering, variable jump height,
accel/decel)**, and **(d) bitmask-typed collision (layers/masks) for distinguishing
wall/cliff/player**. It is *not* an engine comparison (despite its framing) and is
silent on determinism, netcode, frame data, and data-driven content — i.e. on the parts
that actually make Fighters Paradise hard.

---

## What the paper cites (its references `[1]–[9]`)

For provenance — these are the *paper's* sources, not ours:

- [3] **Gregory, J. (2018). _Game Engine Architecture._ A K Peters/CRC Press.** — the
  one substantive engine-architecture text it leans on (the canonical reference; worth
  noting we could mine it directly for our own foundations).
- [4] Hocking, J. (2018). _Unity in Action: Multiplatform game development in C#._
  Manning.
- [5] Bond, J. G. (2020). _Introduction to game design, prototyping and development_
  (Unity/C#). Addison-Wesley.
- [1] "Initial detection of inductance." (2019). *Amusement Equipment Engineering.*
- [2] Wang, X. (2020). Preliminary exploration of play induction function (Master's
  thesis, University of Chinese Academy of Sciences).
- [6]–[9] are **art/audio asset packs** (itch.io pixel sprites, generic character asset,
  Sonatina RPG music pack, Kenney assets) — used for the prototype's content, not
  technical sources.

> Observation: only `[3]` (Gregory) and arguably `[4]`/`[5]` are technical engine
> references, and none are fighting-game-specific. The paper's empirical base is thin,
> which is consistent with its build-log-not-research character.
