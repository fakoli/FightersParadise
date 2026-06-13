# 02 — The Character & Content Ecosystem

> The single most important fact for a compatibility-focused reimplementation: **the content is
> the ecosystem.** The engine ships nearly empty; everything interesting is drop-in community
> files. ([Wikipedia](https://en.wikipedia.org/wiki/Mugen_(game_engine)))

## 1. Kung Fu Man (KFM) — the reference fixture

**Kung Fu Man** (universally abbreviated **KFM**) is the default character bundled with every copy
of the engine. Per the MUGEN Database he is *"the very first M.U.G.E.N character ever made,"*
conceived by Elecbyte *"as a sample character that shows how things are done"* and *"a base for
other creators to build their creations upon."*
([MUGEN DB: Kung Fu Man](https://mugen.fandom.com/wiki/Kung_Fu_Man))

Elecbyte's official tutorial makes his role explicit: *"You can start by using our example
character Kung Fu Man (KFM). KFM's character directory is located in `chars/kfm/`."*
([Elecbyte Tutorial 1](https://www.elecbyte.com/mugendocs/tutorial1.html))

> ⚠️ **KFM is an *original* Elecbyte creation — NOT SNK's "K'".** The two are unrelated; "KFM" is
> just the abbreviation of *Kung Fu Man*. His sprites and sounds are original, which is exactly why
> Elecbyte could legally ship him with the engine. Do not conflate them.

**Why KFM is our conformance fixture:**
- He is the worked example in the official docs, so essentially every creator learns the format by
  reading KFM's files (`kfm.def`, `kfm.sff`, `kfm.air`, `kfm.cmd`, `kfm.cns`, `kfm.snd`).
- He is the universal "hello world" sanity check. **If an engine can't load and run stock KFM, it
  loads nothing.** (Fighters Paradise demoing with `kfm.sff`/`kfm.air` is following this convention.)
- The community even maintains a wiki of **"Elecbyte Kung Fu Man Edits"** — countless creators cut
  their teeth re-skinning or rebalancing him.
  ([MFG KFM wiki](https://network.mugenguild.com/guild/mugenwiki_kfm.html))

> **Caveat for us:** KFM himself is an Elecbyte asset. We can *target compatibility with* his file
> set, but we **cannot redistribute KFM**. We need our own original equivalent fixture
> (see [05 § conformance fixture](05-reimplementation-roadmap.md#a-kfm-equivalent-conformance-fixture)).

## 2. What "a MUGEN character" actually is

A character is a **self-contained folder** dropped into the engine's `chars/` directory. Elecbyte's
tutorial enumerates the six core files every character needs:

| File | Role |
|------|------|
| `.def` | **Definition / "ID card."** Points the engine at all other files + metadata. Required. |
| `.sff` | **Sprite file** — all graphics, packed in MUGEN's Sprite File Format. |
| `.air` | **Animation data** — sprite sequencing, timing, transparency, flips, and the **Clsn hit/collision boxes**. Without it the character doesn't appear on screen. |
| `.cns` | **Constants + states** — the state machine: moves, gravity, hit logic, **and the AI**. |
| `.cmd` | **Command file** — maps input sequences to the states that trigger moves. |
| `.snd` | **Sound file** — voice and SFX. |

Optional/related: `.act` (palette files), `.fnt` (fonts), plus the config files used by stages,
screenpacks, and lifebars.
([Elecbyte Tutorial 1](https://www.elecbyte.com/mugendocs/tutorial1.html),
[MUGEN DB: file formats](https://mugen.fandom.com/wiki/List_of_M.U.G.E.N_file_formats))

The technical detail of each format is in [03-engine-architecture.md](03-engine-architecture.md).

### Quality tiers (community vocabulary)

The community sorts characters along an informal quality/power axis. You'll need to know these terms
because they describe the *adversarial space your parser/VM must survive*:

- **Accurate / source-accurate ("faithful")** — meticulously reproduce a fighter from its origin
  game (correct sprites, frame data, movelist). The realistic *upper bar* for compatibility.
- **Cheap** — *"a variant capable of easily defeating an opponent."* Broad term; some are blatantly
  imbalanced, some are *"unintentionally cheap due to oversights or programming errors."* Hallmarks:
  overpowered moves, infinite combos, one-hit KOs.
  ([MUGEN DB: Cheap character](https://mugen.fandom.com/wiki/Cheap_character))
- **Cheap boss / god-tier / "cheapie"** — the high end of cheap; boss characters often live here.
- **Joke / troll / satire** — made for laughs. The canonical early example is a Homer Simpson joke
  character built from *Barney's Hide & Seek* sprites as a glorified sandbag.
  ([MUGEN DB: Cheap character](https://mugen.fandom.com/wiki/Cheap_character))

A telling community maxim: *"a beautifully drawn character with terrible AI will lose to an ugly
sprite with smart AI every single time."* ([SaltyTrack](https://saltytrack.com/insights/what-is-mugen))
The AI lives in `.cns`/`.cmd` — so **running the embedded AI correctly is non-negotiable**, not a
nice-to-have.

## 3. Notable characters, memes, and the AI-battle scene

### The "cheap boss" arms race

There's a documented lineage of escalating overpowered edits. The seminal entry is **Rare Akuma**
(a cheapie joke edit of Shin Akuma by *Phantom.of.the.Server*) — once considered among the most
powerful MUGEN characters ever, since surpassed by edits like **Chuck Norris, N-Alice, Orochi G3-X,
and Dark Ronald**. The lineage runs roughly: commercial "secret boss" (Shin Akuma) → Rare Akuma →
ever-cheaper "god" edits.
([Joke Battles: Rare Akuma](https://joke-battles.fandom.com/wiki/Rare_Akuma_(M.U.G.E.N)),
[MUGEN DB: Cheap character](https://mugen.fandom.com/wiki/Cheap_character))

### Famous joke / meme characters

Recurring internet-famous content includes **Rare Akuma, Ultimate Chimera, the F1 Button, Omega
Tom Hanks, A-Bomb**, and the SaltyBet legend **Veku** ("king of P-tier"). The meme stock also
absorbed broader internet artifacts — **Weegee, Rick Astley, "Peanut Butter Jelly Time."**
([MUGEN Cheap Wiki](https://mugen-cheap.fandom.com/wiki/General_Ranking),
[Know Your Meme](https://knowyourmeme.com/memes/subcultures/mugen-mugen))

### AI-vs-AI culture & SaltyBet

Because each character carries its own AI, an entire spectator subculture grew around **AI vs AI**
"Watch mode" battles on YouTube and Nico Nico Douga. The crown jewel is **SaltyBet** — a 24/7 Twitch
betting site on AI-controlled MUGEN matches, cycling *over 10,000 unique fighters.*
([MUGEN DB: SaltyBet](https://mugen.fandom.com/wiki/SaltyBet),
[SaltyTrack](https://saltytrack.com/insights/what-is-mugen))

SaltyBet codified a public tier system (weakest → strongest): **P – B – A – S – X.** Characters
auto-promote/demote on **15 consecutive wins or losses**, except **X-tier is assigned manually**.
([MUGEN DB: SaltyBet](https://mugen.fandom.com/wiki/SaltyBet))

> **Why this matters technically:** SaltyBet runs thousands of arbitrary community characters
> head-to-head 24/7. It is, in effect, a **massive ongoing compatibility/robustness test bed** for
> the format. The cheap/god/joke characters are the *adversarial fuzz tests* — custom AI,
> projectiles/helpers, unusual state controllers, extreme scaling. An engine that survives the
> long tail of weird characters is a genuinely compatible engine.

### Oversized roster compilations

A whole content genre is the **mega-roster compilation** — pre-built games stuffed with hundreds of
characters (often anime crossovers), e.g. "Anime Crossover MUGEN" builds advertising **500–540+
characters**, and Ikemen-based compilations bundling massive rosters into one playable build.
([YouTube example](https://www.youtube.com/watch?v=VPPoBZtVL_o))

## 4. The IP / copyright reality

The **overwhelming majority** of MUGEN characters are **unauthorized rips of commercial
fighting-game sprites and other IP** — Street Fighter, King of Fighters, Sonic, Dragon Ball,
Pokémon, Marvel/DC — alongside genuine original creations (OCs). The community even publishes
tutorials on ripping SNK/Capcom sprites from arcade emulators.
([Wikipedia](https://en.wikipedia.org/wiki/Mugen_(game_engine)),
[MFFA ripping tutorial](https://mugenfreeforall.com/topic/30238-how-to-rip-snk-capcom-sprites-from-winkawaks-and-nebula-emulators/))

The legal posture, in practice:
- The engine license permits free **non-commercial** use; IP holders have **generally turned a blind
  eye** to MUGEN content as long as it stays non-commercial.
- **SNK Playmore** initially opposed all third-party IP use, then **reversed to permit MUGEN works
  for private, non-commercial use** (no resale).
  ([SuperCombo](https://supercombo.gg/2022/05/20/fighting-game-mysteries-elecbyte/))
- **Enforcement precedent points at the engine, not the content.** Elecbyte's most notable action
  was a **2009 copyright-infringement notice against a site monetizing the engine**, and
  **BrokenMUGEN** (an unauthorized WinMUGEN mod) was discontinued after a cease-and-desist from
  Elecbyte. So Elecbyte historically defended the *engine binary*, not the data ecosystem.
  ([MUGEN DB: BrokenMUGEN](https://mugen.fandom.com/wiki/BrokenMUGEN_(engine)),
  [Wikipedia](https://en.wikipedia.org/wiki/Mugen_(game_engine)))

> **Takeaway for Fighters Paradise:** the engine is a neutral tool; risk concentrates in
> *distributing it bundled with ripped content*. Keep engine and content **strictly separable**
> (as Elecbyte and Ikemen GO both do). Ship only original/CC assets. See
> [05 § Legal](05-reimplementation-roadmap.md#legal--clean-room-guidance).

## 5. Other content categories

Beyond individual characters, all community-distributed:

- **Full games** — standalone, themed, pre-assembled rosters shipped as complete experiences.
- **Screenpacks** — the **UI/menu skins**: title screen, character-select grid, menus, fonts.
  Distributed by resolution and slot count (e.g. "1280×720, 214 character slots, 125 stages").
- **Stages** — backgrounds/arenas, their own format with parallax + camera config.
- **Lifebars** — the health/power-bar HUD overlay, distributed independently of screenpacks.

([AK1 categories](https://www.andersonkenya1.net/files/category/4-screenpacks/),
[MUGEN Download DB](https://www.mugendb.com/))

These map to our stubbed crates: `fp-ui` (screenpacks, lifebars, select screen), `fp-stage`
(stages), `fp-engine` (full-game/round flow).

## 6. Community infrastructure & scale

| Hub | Role |
|-----|------|
| **MUGEN Database** (`mugen.fandom.com`) | The wiki — canonical reference for lore, definitions, taxonomy. |
| **Mugen Fighters Guild** (`mugenguild.com`) | Long-running creator/discussion forum; hosts char wikis. |
| **Mugen Free For All** (`mugenfreeforall.com`) | Major releases/requests/tutorials forum. |
| **AK1 / Anderson Kenya 1** (`andersonkenya1.net`) | Large file repo organized by category. |
| **The MUGEN ARCHIVE** (`mugenarchive.com`) | *"Largest MUGEN warehouse online,"* claims **50,000+ creations** — but controversial for restrictive policies. |

**Scale (estimates, not audited):** tens of thousands of pieces of content. SaltyBet cycles 10,000+
fighters; SaltyTrack documents ~4,400 well-known characters; the Archive claims 50,000+ creations.
Honest summary: **"tens of thousands of characters, likely 50,000+ pieces of content overall."**
([MUGEN DB: MUGEN ARCHIVE](https://mugen.fandom.com/wiki/MUGEN_ARCHIVE),
[SaltyTrack](https://saltytrack.com/insights/what-is-mugen))

## What a creator expects a compatible engine to do (today)

1. **Load the `.def`-anchored folder** dropped into `chars/`, resolving `.sff` / `.air` (+ Clsn) /
   `.cns` / `.cmd` / `.snd`, plus `.act` palettes and `.fnt` fonts where referenced.
2. **Run the embedded AI** in `.cns`/`.cmd` — mandatory, since AI quality is the whole basis of the
   watch-mode/SaltyBet culture.
3. **Target MUGEN 1.1 beta** as the baseline (what Ikemen GO targets) **while still reading legacy
   `.sff` v1** WinMUGEN content.

### Recommended compatibility test ladder

1. **Stock KFM** (or our original equivalent) — core pipeline smoke test.
2. **Faithful "source-accurate" characters** — stress correct frame data, hitboxes, complex states.
3. **Cheap / god-tier / joke characters** — adversarial fuzz: custom AI, projectiles/helpers, odd
   state controllers, extreme scaling.
4. **Ikemen GO's documented support matrix** — the practical reference for "does my engine load what
   real creators ship."

## Sources

- Wikipedia — *Mugen (game engine)*: https://en.wikipedia.org/wiki/Mugen_(game_engine)
- Elecbyte — *Tutorial 1*: https://www.elecbyte.com/mugendocs/tutorial1.html
- MUGEN Database — *Kung Fu Man*: https://mugen.fandom.com/wiki/Kung_Fu_Man
- Mugen Fighters Guild — *Kung Fu Man Edits* wiki: https://network.mugenguild.com/guild/mugenwiki_kfm.html
- MUGEN Database — *List of file formats*: https://mugen.fandom.com/wiki/List_of_M.U.G.E.N_file_formats
- MUGEN Database — *Cheap character*: https://mugen.fandom.com/wiki/Cheap_character
- MUGEN Cheap Wiki — *General Ranking*: https://mugen-cheap.fandom.com/wiki/General_Ranking
- Joke Battles Wikia — *Rare Akuma (M.U.G.E.N)*: https://joke-battles.fandom.com/wiki/Rare_Akuma_(M.U.G.E.N)
- MUGEN Database — *SaltyBet*: https://mugen.fandom.com/wiki/SaltyBet
- SaltyTrack — *What Is MUGEN?* / *SaltyBet Glossary*: https://saltytrack.com/insights/what-is-mugen , https://saltytrack.com/insights/saltybet-glossary
- Know Your Meme — *Mugen*: https://knowyourmeme.com/memes/subcultures/mugen-mugen
- SuperCombo — *Fighting Game Mysteries: Elecbyte*: https://supercombo.gg/2022/05/20/fighting-game-mysteries-elecbyte/
- MFFA — *How to rip SNK & Capcom sprites*: https://mugenfreeforall.com/topic/30238-how-to-rip-snk-capcom-sprites-from-winkawaks-and-nebula-emulators/
- MUGEN Database — *MUGEN ARCHIVE*: https://mugen.fandom.com/wiki/MUGEN_ARCHIVE
- MUGEN Database — *BrokenMUGEN*: https://mugen.fandom.com/wiki/BrokenMUGEN_(engine)
- AK1 — content categories: https://www.andersonkenya1.net/files/category/4-screenpacks/
