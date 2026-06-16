# Player Expectations — Product Deep-Dive

> **What this is.** A product-strategy document that maps the *player* side of fighting games onto
> Fighters Paradise's *engine* side. It distills Patrick Miller's fighting-game primer **"From Masher to
> Master: The Educated Video Game Enthusiast's Fighting Game Primer" (Super Book Edition)**
> (Shoryuken.com, illustrations by Mariel Cartwright & Jonathan Kim) into the player motivations and
> needs it describes, defines the ideal Fighters Paradise user, honestly grades where the current 1.0 build
> serves those needs, and proposes a prioritized, PRD-shaped feature set to close the gap.
>
> **What this is NOT.** It is not a roadmap edit, not code, and not a commitment. The authoritative plan
> still lives in [roadmap.md](../roadmap.md); the honest current state in [known-issues.md](../known-issues.md);
> the format/controller ledger in [mugen-compatibility.md](../mugen-compatibility.md). This doc *proposes*
> against those; it does not change them.
>
> **Source.** Page references below cite the primer by its printed page number (the circled number at the
> foot of each page). The primer was read in full (Introduction through Conclusion + Recommended Reading,
> pp. 1–131). Where a quote is load-bearing it is reproduced verbatim. The primer is a *Street Fighter*
> teaching text, not a MUGEN/engine text — so its lessons are about **what players experience and need**,
> which is exactly the product lens we want.

---

## 1. Key takeaways from the primer

### 1.1 Why people play (player motivations)

Miller's core thesis is that fighting games are not button-mashers; they are *"speed-chess-poker-magic-the-
gathering-rock-paper-scissors-fighting"* (Introduction, p. 1) — a uniquely *physical* competitive medium
where you must both **understand** a situation and **physically execute** the right response in a fraction
of a second. The motivations he names, repeatedly, are:

- **Mastery of self, not just the game.** *"A great fighting game only gets more and more fun as you put
  more time into it… It is something that can teach you more about yourself."* (p. 3). The Conclusion
  (pp. 129–130) makes this the whole point: *Street Fighter* *"teaches us to scrutinize who we are as
  people… It teaches us to acquire new skills, and the value of practice and repetition and study."* The
  Ryu-as-eternal-challenger framing (p. 129) — Ryu walks off into the sunset because *"the point of the
  game isn't even to win first place… It's just the fight itself, and how it sharpens his soul"* — is the
  emotional spine of the document.
- **The 1-on-1 social rivalry.** *"fighting games are all about: Making new friends, and beating the
  virtual shit out of each other"* (Tip 11, p. 100). Improvement is explicitly social: *"you're only as good
  as the people you play with"* (p. 100). The acknowledgements and "support your local FGC" framing
  (Contributors page) reinforce that the *scene* is part of the product.
- **The deep decision-making loop.** A fight is *"a complex branching chain of attack-block-throw
  interactions"* (Ch. 3, p. 58) run at *"a very, very high speed"* like poker (Ch. 1, p. 29). The pleasure is
  in out-reading another mind in real time (yomi), not in raw reflexes.
- **Expression and style.** Players gravitate to characters and playstyles that fit them (Tip 2, "Play a
  character you like," p. 93; Ch. 5 on offense vs defense temperament). The game is a canvas for personal
  expression *and* a shared language.

### 1.2 The masher → master progression (and what each stage needs)

The book is literally structured as a learning ladder; the implied stages and their needs:

| Stage | Player state | What the primer says they need |
|-------|--------------|--------------------------------|
| **Masher** | Presses buttons "haphazardly"; thinks the game is random/cheap (Intro, p. 1; Ch. 6 scrub). | A reason to believe there *is* a game underneath. A guided on-ramp — *"Fighting games are not actually that hard to learn… But they're very hard to teach"* (p. 4). |
| **Literate** | Understands Attack-Block-Throw, footsies, fireball/DP, hitstun/blockstun, crossups (Ch. 1–3). | **A vocabulary + a mental model.** Ch. 3 explicitly reduces the game to Rock-Paper-Scissors so the player can "think and play much faster." Needs to *see* the structure (hitboxes, phases, frames). |
| **Executing** | Can perform fireball/DP/combos reliably (Ch. 2 & 4). | **A practice space.** "Just go ahead and hit training mode" (p. 35). Set goals, drill "five DPs in a row each side," chunk combos, identify "pain points" (Ch. 4, p. 87). Needs *feedback* — Miller turns game sound off and listens to the stick's "click-click-click" rhythm (p. 39); recommends a "combo trainer" that plays a sound cue (p. 39). |
| **Competent** | Applies the model in real matches; manages resources/meter (Ch. 5, Ch. 7). | **Real opponents + diagnosis tools.** "Act with intention" (Tip 4), "look at the big picture / how did I end up in that situation" (Tip 8), "spend more time in training mode" with **recording functions** to rehearse specific setups (Tip 9, p. 98). |
| **Master / "Warrior"** | Reads opponents, controls neutral, wins conservatively (Tip 5; Conclusion). | **A scene and a ladder of stronger opponents.** "Find new people to play with — and destroy them… you're only as good as the people you play with" (Tip 11, p. 100). Netplay is explicitly part of this (Tip 10, GGPO/Xbox Live/PSN, p. 99). |

The recurring teaching method matters as much as the content: **break a big skill into smaller bite-sized
ideas, introduce them gradually, and give immediate feedback** (Conclusion, p. 130; Ch. 4 "chunking,"
p. 87). The book's *form* is a product spec for onboarding.

### 1.3 What makes matches feel good / rewarding

- **Legible cause and effect.** The single most repeated visual aid is the **hitbox/hurtbox display** —
  HDR's "view mode that shows hitboxes and hurtboxes" (Ch. 1, p. 11), plus the **startup / active /
  recovery** frame diagram of a move (Ch. 1, p. 12). Miller's whole pedagogy depends on the game being
  able to *show* its internal state. "While you may be seeing punches and kicks, the game itself is just
  seeing different kinds of boxes moving around" (p. 11).
- **Momentum and the comeback.** A match is a momentum war (Phase 0 "neutral game," p. 26). Good games
  are designed with deliberate **comeback potential** so neither the first knockdown nor an early lead
  decides everything (Ch. 7, "#3 The Comeback Problem," pp. 121–123; SF4's Ultra meter as the textbook
  example). The *life bar as a poker chip stack* metaphor (p. 122) is how a good build should feel.
- **Risk/reward clarity.** The fireball-vs-DP exchange (Ch. 1) is satisfying *because* each option's payoff
  and punish are knowable: the DP is "high risk, high reward" (p. 17), the fireball trades present safety for
  space. The feel comes from readable trade-offs, not flashy art — *"the coolest-looking moves are the
  ones you want to use sparingly"* (p. 13).
- **Inputs that resolve cleanly.** A huge fraction of "feel" is whether your intended motion comes out:
  *"the difference between… walk-forward-fireball and Dragon Punch is a matter of hitting some very small
  switches very close to each other in just the right time"* (p. 8). The primer teaches *diagnosing* botched
  inputs (Ch. 2, pp. 37–38) — which presumes the engine has consistent, learnable input recognition and,
  ideally, an **input display** to show what it read.
- **A precise, reliable clock.** Everything is measured in 60fps frames ("frame advantage," p. 21; "one
  frame (1/60th of a second)" reversal window, p. 46). Determinism and frame-accuracy are table stakes for
  the experience to even exist.

### 1.4 Common frustrations / barriers that drive players away

- **The teaching gap / barrier to entry.** *"the relatively high barrier to entry intimidates them… Fighting
  games are not actually that hard to learn… But they're very hard to teach."* (p. 4). Learning alone "by
  getting your asses beat for years" is "stupidly hard" (p. 4). This is the #1 churn driver in the book.
- **Execution walls.** Players who can't reliably throw a fireball or DP get *"demoralized"* (Ch. 2, p. 36)
  and quit before they ever reach the actual game. Miller wrote a whole extra section for his friend who
  *"couldn't hit 'em — by stick or by pad, by left side or by right side"* (p. 36).
- **The "scrub mentality" / "it's cheap."** Ch. 6 ("Don't Want No Scrubs") names the mindset that calls
  tactics "cheap" and refuses to learn — *"Everything you think is 'cheap' is really just 'stuff you haven't
  figured out how to beat'"* (Tip 7, p. 97; Ch. 6, p. 104). It's framed as ego + naivety; the cure is *losing
  the ego* and exposing your game to stronger players (pp. 105–107).
- **Opaque mechanics that read as randomness.** A masher experiences crossups, meaties, and tick-throws
  as unfairness because the game never *showed* them the structure (Ch. 6, p. 103: "you're pressing
  buttons" not "playing Street Fighter"). The fix is legibility (see 1.3).
- **Design pitfalls the genre itself has fought.** Ch. 7 names four "problems" classic SF had to solve:
  the **Fireball Problem** (zoning too oppressive for casuals — "many casual players will get frustrated
  long before they're able to acquire it," p. 118), the **Footsies Problem** (looks like "spamming" to
  outsiders; also brutally hard to *balance* across a big roster, p. 120), the **Comeback Problem**
  (pp. 121–123), and the **Super/resource Problem** (pp. 124–126). These are *design* frustrations — the
  engine's job is to make the *content author's* tuning of them possible and visible.
- **No one to play / online lag.** Without opponents you can't improve (Tip 11). Online is essential but lag
  "can make a drastic difference in… how you execute your moves and react" and breeds bad habits (Tip 10,
  p. 99) — so quality netcode (the book name-drops GGPO, p. 99) matters.

---

## 2. The ideal user for Fighters Paradise

Fighters Paradise's positioning ("a *completely customizable* fighting-game engine: bring your own
characters in MUGEN format") plus the primer's player model imply **three nested personas**. They are
nested, not separate — the same person can move outward over time, and the engine should let them.

### Persona A — "The Curator / Author" (primary, today)
The MUGEN tinkerer who has a `chars/` folder full of `.def`s and wants them to *run* and *render*
faithfully. This is who the engine already serves and the only one who can exist without a content
ecosystem. From the primer they inherit the **author-side** of every "Problem" in Ch. 7: they need to be
able to tune fireballs, footsies range, meter, and comeback mechanics *and see the result*. They are not
necessarily a strong player; they are a builder. **The validator, directory discovery, and never-crash
robustness are for them.**

### Persona B — "The Learner" (the primer's whole audience; our biggest growth gap)
The "Educated Video Game Enthusiast" — smart about games, intimidated by the barrier to entry, wants to
become *literate then competent*. Everything in §1.2–§1.4 is their need list: a guided on-ramp, a training
mode with **hitbox/input/frame display**, drill goals, recording/playback of setups, and a CPU that
teaches. Today Fighters Paradise gives this persona a fight loop but almost none of the *learning scaffold*
the primer says they require. **This is the single largest unserved persona and the biggest strategic
opportunity** if the goal is "a complete, playable game," not just an engine.

### Persona C — "The Competitor / Warrior" (aspirational; gated on B + infra)
The player who wants rivals, ladders, replays to study, and netplay (Tip 10/11; Conclusion). Serving them
requires the determinism/replay/netplay infrastructure that the roadmap already files under M4 — but
*also* the social layer (matchmaking, lobbies) the primer treats as the endgame ("find new people to play
with"). This persona is real but should be sequenced last; without B there is no pipeline of players to
become C.

> **Strategic read.** The clean-room "bring your own everything" engine is genuinely differentiated for
> Persona A and is largely *done*. But the primer is overwhelmingly a Persona-B document, and Persona B is
> where a "playable game, not just an engine" lives or dies. The product question is whether Fighters
> Paradise wants to be *the best MUGEN runtime* (A) or *the best place to learn and play a customizable
> fighter* (A→B→C). The proposals in §4 assume the latter, because that is what the primer optimizes for
> and what turns a tech achievement into a product.

---

## 3. Gap analysis — primer expectations vs the current build

Status key mirrors the project's own: **Strong** (the build serves this well), **Partial** (works for the
common case, real gaps), **Gap** (essentially unserved). Cross-references are to
[known-issues.md](../known-issues.md), [roadmap.md](../roadmap.md),
[mugen-compatibility.md](../mugen-compatibility.md), and the
[faithfulness audit](08-faithfulness-audit.md).

| # | Primer expectation (player need) | FP status | Evidence / notes |
|---|----------------------------------|-----------|------------------|
| 1 | **Guided on-ramp / onboarding** ("hard to teach," p. 4) | **Gap** | The build has a Title→Select→Stage→Fight flow and a Setup/Options screen (CLAUDE.md banner), but **no tutorial, no trials, no explanatory layer**. Nothing teaches Attack-Block-Throw or how to throw a fireball. |
| 2 | **Hitbox / hurtbox display** (p. 11) | **Partial→Strong-ish** | The **F1 Clsn debug overlay** exists and draws Clsn1/Clsn2 boxes (known-issues "#34 done"; render debug-box primitive). It is a *developer* overlay, not a player-facing, labeled, color-coded "training view" — but the hard rendering part is done. Big leverage to productize. |
| 3 | **Move phase display: startup / active / recovery** (p. 12) | **Gap** | Frames exist internally (fixed 60Hz tick, AIR frame data) but there is **no surfaced frame data** — no per-move startup/active/recovery readout, no on-block advantage. Core to the learning model. |
| 4 | **Input display** ("diagnose your botched fireball," pp. 37–38) | **Gap** | `fp-input` has a 60-frame ring buffer + command recognition with `~ / $ > +` tokens — the *data* is right there — but nothing renders the recent input history on screen. Cheap to add, high pedagogical value. |
| 5 | **Training mode** (drill DPs, set CPU dummy behavior, record/playback setups — Tip 9, p. 98; Ch. 2/4 drills) | **Gap** | There is no training mode. P2 is either a "baseline CPU AI" or human (CLAUDE.md), with **no dummy controls** (block all / record / playback / reset position / infinite meter+life). The primer's drills (pp. 54–55) are literally a training-mode spec we don't fulfill. |
| 6 | **Reliable, consistent inputs** (fireball vs DP, p. 8) | **Strong** | `fp-input` command recognition is implemented + tested (103 tests), handles the full symbol set. Recognition quality vs real MUGEN content is the open question, but the mechanism is real. |
| 7 | **A precise 60fps clock / determinism** (frame advantage, 1-frame reversals, pp. 21/46) | **Partial** | Fixed 60Hz tick is a design keystone (architecture §2.1). But **whole-match determinism/replay is not built** (#38 Missing): no `to_bytes`/`from_bytes`, no record/replay harness beyond per-tick input snapshots. Frame-accuracy *exists*; *reproducibility tooling* doesn't. |
| 8 | **Game-feel: hit feedback / sparks / juice** (readable impact) | **Partial** | Hitpause, knockback, impact sounds, PalFX tint, and an AfterImage smear all work. But **hit sparks render only for own-sparks**, and KFM/conventional content shows **no visible spark** because there's no `fightfx.sff` loader and the `S`-prefix is flattened (#17 Partial). The single most-noticeable "feel" gap on real content. |
| 9 | **Comeback mechanics / meter feel** (Ch. 7 #3/#4) | **Partial** | A power/super meter exists and is drawn (blue bar, #26 done); meter is carried across rounds. But the *author's* ability to express SF4-style Ultra/Revenge comeback systems depends on controllers like `Helper`/`Projectile`/`Explod` and full PalFX — several of which are no-ops or approximations. The engine can run KFM-class meter; richer comeback design is content-blocked. |
| 10 | **Footsies/neutral readability** (range, sweeps, c.MK space control) | **Strong (mechanics) / Partial (presentation)** | Distance triggers (P2Dist/P2BodyDist), push/bounds, facing, hit detection all work via the cross-entity eval keystone. The *neutral game* the primer describes is fully expressible. Presentation gaps (sparks, true afterimage, stage tiling) dull it but don't break it. |
| 11 | **The fireball game / projectiles** (Ch. 1, Ch. 7 #1) | **Gap (engine) — critical** | The whole primer is built on Ryu's fireball, yet **`Projectile` (and `Helper`) controllers are no-ops** (architecture §2.4; audit). MUGEN fireballs are usually `Projectile`/`Helper`-driven. **A character whose identity is a fireball will not throw one.** This is the most primer-central mechanical gap in the build. (Bare no-id `Proj*` triggers also return 0 — known-issues v1.0 follow-up.) |
| 12 | **Crossups / high-low/throw mixups, meaties, tick-throws** (Ch. 1, Ch. 6) | **Partial** | Throws (`Target*` + `p1stateno`) work; facing-relative input and block direction are handled. Get-hit/guard resolution ladder is faithful. Crossup-correct blocking depends on facing flips mid-jump (the `EvalCtx` facing math exists). Likely workable for KFM-class content; not validated against the adversarial mixups the primer describes. |
| 13 | **Pick/learn characters; tier/style expression** (Tips 2/3) | **Partial** | Directory discovery auto-populates a roster from `chars/<name>/`; character-select + stage-select exist. But there's **no per-character info** (movelist, properties, tier hints) surfaced — the "learn the top tier / play who you like" loop has no in-app support. Movelists are author content we don't display. |
| 14 | **Netplay** (Tip 10; GGPO named, p. 99) | **Gap** | No netcode. Roadmap files this under M4 as *groundwork* gated on determinism (#38). Correctly sequenced as far-future. |
| 15 | **Real opponents / CPU that teaches** (Tip 11; learning needs a foil) | **Partial** | A "baseline CPU AI" P2 exists, and the RNG-in-state seam (#28) enables probabilistic AI. But there is no *difficulty ladder*, no *teaching* CPU, and no dummy-recording foil for solo learners — the primer's "you're only as good as the people you play with" has no single-player substitute today. |
| 16 | **Never punish the player for bad content** (the masher's "it's cheap" is often the engine's fault) | **Strong** | The never-crash discipline is real and pervasive (architecture §2.7): warn-and-skip parsing, const-0 fallbacks, invisible missing sprites, test-pattern fallback, ~2621 tests + clippy/fmt CI gates. This is a genuine moat for Persona A and indirectly protects B. |

### Honest summary of strengths vs gaps

**Where we are strong:** the *simulation core* the primer presupposes — a deterministic 60Hz tick, faithful
Attack-Block-Throw/guard resolution, distance/neutral mechanics, throws, meter, hitpause, i-frames, and an
exceptional never-crash robustness story with a real CI safety net. The hard part of "the game underneath"
mostly exists. The **F1 hitbox overlay** and the **input ring buffer** mean two of the primer's three core
teaching visuals are *one productization step away*.

**Where the gaps are concentrated:** everything the primer spends 130 pages on — **the learning scaffold**
(onboarding, training mode, frame data, input display, teaching CPU) — and two mechanically central items:
**projectiles/helpers** (`Projectile`/`Helper` no-ops, which breaks the fireball game the entire primer is
built around) and **hit-spark feedback** for conventional content. Plus the forward-looking
**determinism/replay → netplay** chain that gates the Competitor persona.

The blunt version: **we built the engine the primer assumes, and almost none of the teacher the primer
*is*.**

---

## 4. Proposed specs & milestones (PRD-shaped, not a roadmap edit)

These are **proposals** framed so they could later seed a fakoli-state PRD: each group has a goal, the
persona it serves, and acceptance-style bullets. They are sequenced by *impact-per-effort toward the ideal
user*, and they explicitly avoid re-specifying work the roadmap already owns (M4 replay/teams/netplay) —
instead they note where they *depend on* or *complement* it.

> **Sequencing principle (from the primer).** Legibility before depth before competition. You cannot teach
> footsies to someone who can't see hitboxes; you can't run a ladder for players who never onboarded. So
> the order is: **make the fight legible → make it teachable → make the fireball game actually work →
> then make it competitive/social.**

### FG-1 — "See the Fight": player-facing training/legibility layer  *(Persona B; highest impact-per-effort)*
**Goal.** Turn the existing developer overlays into a real, player-facing **Training/Lab mode** that makes
the simulation legible — the primer's three core visuals (hitboxes, move phases, inputs) plus dummy
control. Most of the *hard* substrate already exists (F1 Clsn overlay, input ring buffer, fixed tick), so
this is largely surfacing + UI.
- A selectable **Training mode** from the menu (not just CLI), distinct from a normal match.
- **Hitbox/hurtbox view** as a labeled, color-coded, toggleable *player* feature (red=hitbox/Clsn1,
  blue=hurtbox/Clsn2), built on the existing F1 debug primitive. *(Complements #34.)*
- **Input display**: render the last N inputs (directions + buttons + recognized command) from the
  `fp-input` ring buffer along the screen edge. *(Data already exists.)*
- **Move-phase / frame readout**: surface startup / active / recovery and on-block advantage for the
  currently executing move, derived from AIR frame counts + Clsn windows. *(New surfacing of existing data.)*
- **Dummy controls**: stand / crouch / jump / block-all / block-after-first-hit / CPU; toggles for
  infinite life + infinite meter; reset-to-position. *(Needs a P2 control source abstraction.)*
- **Acceptance feel:** "A learner can load a character, see exactly which box hit them, read why their
  fireball came out as a crouch punch, and drill a setup against a controllable dummy — without leaving the
  app." Directly fulfills Ch. 1 (p. 11–12), Ch. 2 (pp. 37–38), Tip 9 (p. 98), and the drill lists (pp. 54–55).

### FG-2 — "Make the Fireball Real": projectile & helper subsystem  *(Persona A+B; highest mechanical impact)*
**Goal.** Implement the `Helper`/`Projectile`/`Explod` entity subsystem so the genre-defining fireball game
the *entire primer* is built on actually works for conventional content.
- A child-entity model (slot-map of helpers/projectiles) — the architecture doc explicitly flags this as
  "not yet modeled" (§2.2), so it is net-new infrastructure, *large effort*, and a prerequisite for many
  community characters.
- `Projectile` controller spawns/ticks/expires projectiles; `ProjHit`/`ProjContact`/`ProjGuarded` triggers
  resolve (including the bare no-id form currently returning 0 — known-issues follow-up).
- `Helper` lifecycle + `parent`/`helper`/`target`/`root` redirects resolve to real entities (they return
  `None` today — architecture §2.5).
- `Explod` + the `fightfx.sff`/`fightfx.air` loader so **hit sparks render for KFM/conventional content**
  (closes the visible half of #17 — the most-noticeable feel gap).
- **Acceptance feel:** "Ryu throws a Hadouken, it travels, it trades with another fireball, it hits and
  shows a spark." This unblocks Persona A's catalog *and* makes the primer's Ch. 1/Ch. 7 lessons
  demonstrable. *(Note: large; sequence the spark/`fightfx` slice early since it has independent value.)*

### FG-3 — "Game Feel" polish pass  *(Persona A+B; medium effort, high perceived quality)*
**Goal.** Close the readable-impact and presentation gaps that make hits feel weightless or stages feel
flat — the difference between "tech demo" and "game."
- True hit-spark rendering for common sparks (depends on FG-2's `fightfx` loader; the `S`-prefix flatten
  in `parse_resource_id` is the other half of #17).
- True AfterImage frame-history ghost ring (current is a motion-smear approximation, #33) and the unmodeled
  PalFX fields (`sinadd`/`PalBright`/`PalContrast`/`Trans`).
- Stage fidelity: `tile`/`velocity`/`mask`/`type=anim` rendering + camera vertical follow (#29 Partial).
- Screenpack fidelity: `[Combo]` counter drawn, `[Face]` portraits, layered bg (#31 Partial) — the combo
  counter is *directly* what the primer's combo chapter (Ch. 4) wants to reward.
- **Acceptance feel:** "Hits land with weight, supers flash, combos count up on screen, stages scroll
  properly." Serves the primer's "matches feel good" criteria (§1.3).

### FG-4 — "Learn to Play": onboarding + content presentation  *(Persona B; the primer-as-product)*
**Goal.** Be the teacher the primer says the genre lacks. This is the strategic bet that turns a runtime
into a product.
- An **interactive tutorial / trials** flow teaching the primer's literacy ladder: blocking high/low,
  Attack-Block-Throw, fireball/DP, crossups, basic combos — built on FG-1's Training mode.
- **Sound-cue practice** option (the primer's stick-rhythm and "combo trainer" insight, p. 39): an optional
  audio metronome/cue for combo timing.
- **In-app movelist / character info** screen (Tips 2/3): surface a character's normals/specials and
  authored properties so a learner can "play who they like" and "learn the top tier."
- A **teaching/difficulty-laddered CPU** (building on the RNG-in-state seam #28) so solo learners have a
  foil — the single-player substitute for "you're only as good as the people you play with."
- **Acceptance feel:** "A smart newcomer who has never thrown a fireball can install Fighters Paradise and,
  in-app, learn to be *literate* — the exact arc the primer teaches." This is the highest-ceiling,
  highest-effort group; it depends on FG-1 and is the clearest path to the "playable game, not just engine"
  vision.

### FG-5 — "Study & Compete": replay, then netplay  *(Persona C; complements roadmap M4, sequence last)*
**Goal.** Give the Warrior persona the study and competition tools the Conclusion + Tips 10/11 demand —
explicitly building on the roadmap's existing M4 items rather than duplicating them.
- **Whole-match record/replay** (roadmap #38): `to_bytes`/`from_bytes`, a record/replay harness, a
  determinism test. *Prerequisite for everything below.* (Training-mode rewind in FG-1 also benefits.)
- **Replay study tools**: load a recorded match, scrub frames, toggle the FG-1 hitbox/input overlays on
  playback — "study yourself / study the top players."
- **Team/Turns/Tag generalization** (roadmap #39) + `.act` runtime palette consumption — the modes real
  rosters expect.
- **Netplay** (roadmap netplay groundwork; primer Tip 10 names GGPO): rollback/lockstep on top of
  determinism. Plus the social layer the primer treats as the endgame (lobbies/matchmaking) — *the* thing
  that creates Persona C from Persona B.
- **Acceptance feel:** "Record a match, study it frame-by-frame with hitboxes on, then play a ranked set
  online." This is the long pole and is correctly the *last* sequence.

### Suggested milestone sequencing (by impact toward the ideal user)

1. **FG-1 (See the Fight)** — cheapest path to serving Persona B; mostly surfacing work on existing
   substrate. *Do first.*
2. **FG-2 (Make the Fireball Real)** — highest mechanical leverage; unblocks Persona A's catalog and the
   primer's central lesson. *Pull the `fightfx`/spark slice forward — it has standalone value.*
3. **FG-3 (Game Feel)** — perceived-quality multiplier; partly gated on FG-2's spark loader.
4. **FG-4 (Learn to Play)** — the strategic product bet; gated on FG-1.
5. **FG-5 (Study & Compete)** — long-horizon, complements roadmap M4/M5; gated on determinism (#38).

---

## 5. Appendix — primer citations index (for traceability)

- Why play / self-mastery: pp. 1–4, 129–130 (Intro; Conclusion "The Warrior's Path").
- Teaching gap / barrier to entry: p. 4 ("very hard to teach"); p. 36 (execution demoralization).
- Hitbox/hurtbox view: p. 11. Startup/active/recovery frame diagram: p. 12.
- Fireball vs Dragon Punch (the core exchange): pp. 14–17, 25–28.
- Footsies / neutral game: pp. 26–30 (Phase 0; "Word Alert — Footsies").
- Rock-Paper-Scissors mental model / Attack-Block-Throw: pp. 58–60.
- Input diagnosis / "Teach Me How To Douken": pp. 36–39 (incl. sound-cue practice, p. 39).
- Execution drills / training-mode goals: pp. 35, 44–46, 54–55.
- Combos / chunking / pain points: pp. 75–90 (esp. p. 87 chunking; pp. 83–84 BnB checklist).
- Eleven tips (intention, play safe, do what works, throw a lot, big picture, training mode, online,
  find new people): pp. 92–100.
- Scrub mentality / losing the ego / "it's cheap": pp. 97 (Tip 7), 103–107 (Ch. 6).
- SF4 / character skeleton / the four "Problems" (Fireball/Footsies/Comeback/Super) + comeback design:
  pp. 109–127.
- Netplay / GGPO: p. 99 (Tip 10).
- Recommended reading (Sirlin's *Playing to Win*, Killian's *Domination 101*, footsies handbook): p. 131.

*Note on readability: the PDF was read via per-page image extraction; all 130 numbered pages plus front
matter and the recommended-reading appendix rendered legibly. No section was unreadable. The PSN-inbox
screenshot on p. 106 contains profanity that is incidental to the (cited) "losing the ego" point and is not
reproduced here.*

---

*Author: product-strategy research pass, 2026-06-16. Cross-references: [roadmap.md](../roadmap.md) ·
[known-issues.md](../known-issues.md) · [mugen-compatibility.md](../mugen-compatibility.md) ·
[architecture.md](../architecture.md) · [faithfulness audit](08-faithfulness-audit.md). This document
proposes; it does not amend the roadmap or any other doc.*
