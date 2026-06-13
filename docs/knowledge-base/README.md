# Fighters Paradise — Knowledge Base

A research-backed knowledge base on **M.U.G.E.N** (the engine we are reimplementing),
the community/content ecosystem it produced, and where the **Fighters Paradise** codebase
stands today. The purpose is to ground a clean-room, format-compatible Rust reimplementation
in verified facts rather than guesswork.

> **Clean-room stance (read first):** We reimplement from *documented formats and observed
> behavior*, never from Elecbyte's (closed) source. We ship **no** Elecbyte assets and **no**
> copyrighted character content. See [05-reimplementation-roadmap.md](05-reimplementation-roadmap.md#legal--clean-room-guidance).

## Contents

| Doc | What's inside |
|-----|---------------|
| [01-mugen-overview.md](01-mugen-overview.md) | What M.U.G.E.N is, Elecbyte's history, the DOS → WinMUGEN → 1.0/1.1 eras, the Ikemen GO open-source successor, and why it endured 20+ years. |
| [02-character-ecosystem.md](02-character-ecosystem.md) | Kung Fu Man (the reference fixture), what "a character" is, quality tiers (accurate/cheap/joke/god-tier), notable memes, the AI-vs-AI / SaltyBet scene, the IP reality, and community infrastructure. |
| [03-engine-architecture.md](03-engine-architecture.md) | The technical heart: coordinate system, the character file set (.def/.cns/.cmd/.air/.sff/.snd/.fnt), the state machine (Statedef + ~100 state controllers), the trigger/expression system, the combat model (HitDef/Clsn/juggle/guard), helpers/projectiles/explods, and common1.cns. |
| [04-codebase-review.md](04-codebase-review.md) | Verified ground-truth review of the Fighters Paradise workspace: what's implemented, what's stubbed, dependency graph, parser completeness, the game loop, and technical debt. |
| [05-reimplementation-roadmap.md](05-reimplementation-roadmap.md) | Gap analysis, a phased build plan that maps engine concepts → crates, the legal/clean-room guidance, and what a "KFM-equivalent" conformance fixture should be. |

## How to use this

- **Implementing a parser?** Start with the format's section in [03](03-engine-architecture.md), then check the spec under `docs/format-specs/`.
- **Picking the next phase?** See the gap analysis in [05](05-reimplementation-roadmap.md).
- **Wondering whether something is legal to ship?** [05 § Legal](05-reimplementation-roadmap.md#legal--clean-room-guidance).

## Source quality

Engine/format facts are anchored in the Elecbyte documentation and corroborated by independent
parsers (e.g. `bitcraft/mugen-tools`) and the MIT-licensed **Ikemen GO** reimplementation.
Cultural/scale facts come from community wikis and retrospectives — reliable for *consensus and
vocabulary*, but exact counts are estimates. Inline citations appear throughout; each doc ends
with a sources list.

_Compiled 2026-06-13 from a multi-agent research + codebase-verification sweep._
