# 01 — M.U.G.E.N: What It Is & Its History

## In one sentence

**M.U.G.E.N is a data-driven 2D fighting-game *engine* — not a game** — created by the anonymous
group **Elecbyte**. It ships essentially empty (one sample character) and runs whatever
characters, stages, screenpacks, and sounds the community supplies as drop-in data files.
([Wikipedia](https://en.wikipedia.org/wiki/Mugen_(game_engine)))

The name derives from Japanese **無限** ("limitless" / "infinity"); the original acronym
expansion is "lost to time." ([SaltyTrack](https://saltytrack.com/insights/what-is-mugen))

## Who built it

Elecbyte was a small group — commonly cited as three — of **Electrical Engineering & Computer
Science students at the University of Michigan** (Ann Arbor), working under the aliases *Akito*,
*Geki*, and *Admin*. They stayed anonymous for nearly two decades and largely vanished from the
internet around 2002–2003. The engine was written in **C**, originally on the **Allegro**
graphics library, later migrating toward **SDL**.
([SuperCombo](https://supercombo.gg/2022/05/20/fighting-game-mysteries-elecbyte/),
[U-M CSE](https://cse.engin.umich.edu/stories/u-m-origins-of-legendary-gaming-mystery-revealed),
[Wikipedia](https://en.wikipedia.org/wiki/Mugen_(game_engine)))

A piece of trivia that explains the engine's DNA: MUGEN drew early inspiration from a Korean
pirated Street Fighter II clone, **SF2IBM**, and was originally distributed under a
"giftware/swapware" model. ([SuperCombo](https://supercombo.gg/2022/05/20/fighting-game-mysteries-elecbyte/))

## The three eras

| Era | Timeline & key facts |
|-----|----------------------|
| **DOS-MUGEN** | First public beta **July 27, 1999** for MS-DOS. Development moved to Linux in **November 2001**, ending DOS support. Allegro-based. |
| **WinMUGEN** | Elecbyte solicited donations for a Windows build, then **discontinued the project in 2003** and took the site down. A public Windows beta circulated anyway; third-party **"no-limit"** hacks (~2007) removed the character-count cap and added higher-res support. This era's `.sff` v1 content is a huge legacy library. |
| **1.0 / 1.1** | A re-emerged Elecbyte shipped release candidates in 2009–2010; **MUGEN 1.0 official: January 10, 2011**. **MUGEN 1.1 beta 1: August 2013** added an OpenGL renderer, stage zoom, and 720p `.sff` v2 assets. **1.1 beta is the last official build.** |

Sources: [List of MUGEN versions](https://mugen.fandom.com/wiki/List_of_M.U.G.E.N_versions),
[Wikipedia](https://en.wikipedia.org/wiki/Mugen_(game_engine)).

### The two practical compatibility eras

For a reimplementation, the version history collapses into **two format generations you must read**:

1. **WinMUGEN era** — `.sff` **v1** sprites (PCX-encoded), older font format. The largest legacy
   content library lives here.
2. **MUGEN 1.0 / 1.1 era** — `.sff` **v2** sprites (RLE8/RLE5/LZ5, plus PNG in 1.1), higher color
   depth, stage zoom, text-based fonts. This is the modern baseline.

**Ikemen GO targets MUGEN 1.1 beta compatibility**, and so should we. (See below.)

## Closed source — and why that matters

Elecbyte **never released the engine's source code**; MUGEN remains closed-source to this day.
That single fact is the reason the entire reimplementation scene exists: the community had a
*frozen, well-documented spec* (the Elecbyte docs + the sample character) but no code, so people
re-implemented the engine from the outside.
([Wikipedia](https://en.wikipedia.org/wiki/Mugen_(game_engine)),
[SuperCombo](https://supercombo.gg/2022/05/20/fighting-game-mysteries-elecbyte/))

> **For us:** "closed source" is a *feature* of the clean-room path. The Elecbyte **docs** describe
> behavior; they are not Elecbyte's code. We implement from the docs and observed behavior. We do
> **not** decompile MUGEN.

## Ikemen GO — the open-source successor (our north star)

- **Ikemen** is an open reimplementation that added online play.
- **Ikemen GO** is a community rewrite **in Go**, **MIT-licensed**, explicitly aiming for
  **backwards compatibility with MUGEN 1.1 beta** — it loads MUGEN characters, stages, and
  screenpacks and adds rollback netcode. Its bundled screenpack assets are CC-licensed.
  ([Ikemen GO on GitHub](https://github.com/ikemen-engine/Ikemen-GO),
  [MUGEN DB: Ikemen](https://mugen.fandom.com/wiki/Ikemen))

**Why it's our most valuable reference:** Ikemen GO is *itself* a clean-room, format-compatible
reimplementation. It demonstrates that a format-compatible engine is legally viable, and its source
is a legitimate reference for *how the formats behave* (it is not Elecbyte code). When the Elecbyte
docs are ambiguous, Ikemen GO's behavior is the practical tie-breaker.

## Why it became a cult engine and endured 20+ years

1. **Near-zero barrier.** Free + drop-in modularity: even a programming amateur can assemble a
   roster of hundreds of characters.
2. **A stable, knowable spec.** A 1999-era character still runs; Elecbyte's disappearance left the
   format *frozen*, which the community kept building on and eventually re-implemented.
3. **The crossover premise has infinite fuel.** "You can put literally anyone in MUGEN" — anime vs.
   cartoon vs. video-game vs. real celebrity. New IP and memes constantly feed it.
4. **A spectator layer.** AI-vs-AI battles and especially **SaltyBet** (24/7 Twitch betting on
   AI-controlled MUGEN matches) gave it a self-renewing streaming-era audience.
   ([Know Your Meme](https://knowyourmeme.com/memes/subcultures/mugen-mugen),
   [SaltyTrack](https://saltytrack.com/insights/what-is-mugen))

Geek.com even named MUGEN its **"Game of the Year for 2017"** — 18 years after release.
([Wikipedia](https://en.wikipedia.org/wiki/Mugen_(game_engine)))

The character/culture side is covered in [02-character-ecosystem.md](02-character-ecosystem.md);
the technical internals in [03-engine-architecture.md](03-engine-architecture.md).

## Sources

- Wikipedia — *Mugen (game engine)*: https://en.wikipedia.org/wiki/Mugen_(game_engine)
- Simple Wikipedia — *M.U.G.E.N.*: https://simple.wikipedia.org/wiki/M.U.G.E.N.
- MUGEN Database — *List of M.U.G.E.N versions*: https://mugen.fandom.com/wiki/List_of_M.U.G.E.N_versions
- SuperCombo — *Fighting Game Mysteries: Elecbyte*: https://supercombo.gg/2022/05/20/fighting-game-mysteries-elecbyte/
- U-M CSE — *Origins of a legendary gaming mystery revealed*: https://cse.engin.umich.edu/stories/u-m-origins-of-legendary-gaming-mystery-revealed
- Ikemen GO — GitHub: https://github.com/ikemen-engine/Ikemen-GO
- MUGEN Database — *Ikemen*: https://mugen.fandom.com/wiki/Ikemen
- Know Your Meme — *Mugen / M.U.G.E.N*: https://knowyourmeme.com/memes/subcultures/mugen-mugen
- SaltyTrack — *What Is MUGEN?*: https://saltytrack.com/insights/what-is-mugen
