# Fighters Paradise — Knowledge Base

A research-backed knowledge base on **M.U.G.E.N** (the engine we are reimplementing),
the community/content ecosystem it produced, and where the **Fighters Paradise** codebase
stands today. The purpose is to ground a clean-room, format-compatible Rust reimplementation
in verified facts rather than guesswork.

> **What this directory is — research & planning history, not the current spec.**
> `docs/knowledge-base/` captures the MUGEN research, the build planning, and a point-in-time
> codebase review. Some of it is intentionally a **historical snapshot** (notably
> [04-codebase-review.md](04-codebase-review.md), taken at an early commit). For the **authoritative,
> current** picture of what Fighters Paradise does today, read the top-level docs instead:
>
> | Authoritative doc | What it covers |
> |-------------------|----------------|
> | [../architecture.md](../architecture.md) | Design overview, dependency graph, the keystone decisions. |
> | [../mugen-compatibility.md](../mugen-compatibility.md) | The support matrix — which formats parse, which controllers/triggers run, where we diverge from MUGEN 1.0. |
> | [../content-guide.md](../content-guide.md) | How to structure bring-your-own-character MUGEN content. |
> | [../known-issues.md](../known-issues.md) | Ranked fidelity gaps (the live mirror of [doc 08](08-faithfulness-audit.md)). |
> | [../roadmap.md](../roadmap.md) | What's planned next. |
> | [../development.md](../development.md) | Build / test / lint, worktree + clean-room rules. |
> | [../format-specs/sff-v2.md](../format-specs/sff-v2.md) | SFF v2 binary layout. |
> | Root [../../README.md](../../README.md) · [../../CLAUDE.md](../../CLAUDE.md) | Public overview · maintainer/agent guide. |

> **Clean-room stance (read first):** We reimplement from *documented formats and observed
> behavior*, never from Elecbyte's (closed) source. We ship **no** Elecbyte assets and **no**
> copyrighted character content. See [05-reimplementation-roadmap.md](05-reimplementation-roadmap.md#legal--clean-room-guidance).

## Contents

| Doc | What's inside |
|-----|---------------|
| [01-mugen-overview.md](01-mugen-overview.md) | What M.U.G.E.N is, Elecbyte's history, the DOS → WinMUGEN → 1.0/1.1 eras, the Ikemen GO open-source successor, and why it endured 20+ years. |
| [02-character-ecosystem.md](02-character-ecosystem.md) | Kung Fu Man (the reference fixture), what "a character" is, quality tiers (accurate/cheap/joke/god-tier), notable memes, the AI-vs-AI / SaltyBet scene, the IP reality, and community infrastructure. |
| [03-engine-architecture.md](03-engine-architecture.md) | The technical heart: coordinate system, the character file set (.def/.cns/.cmd/.air/.sff/.snd/.fnt), the state machine (Statedef + ~100 state controllers), the trigger/expression system, the combat model (HitDef/Clsn/juggle/guard), helpers/projectiles/explods, and common1.cns. |
| [04-codebase-review.md](04-codebase-review.md) | Codebase review of the workspace **as of commit `52db1f7` (2026-06-13)**: what was implemented/stubbed, dependency graph, parser completeness, the game loop, technical debt. ⚠️ **Historical snapshot — now stale** (it predates the playable two-character match; many crates it calls "stub" are implemented). Use [../architecture.md](../architecture.md) and [../known-issues.md](../known-issues.md) for the current state. |
| [05-reimplementation-roadmap.md](05-reimplementation-roadmap.md) | Gap analysis, a phased build plan that maps engine concepts → crates, the legal/clean-room guidance, and what a "KFM-equivalent" conformance fixture should be. (Forward planning continues in [../roadmap.md](../roadmap.md).) |
| [06-execution-plan.md](06-execution-plan.md) | The operational build plan — the reviewed agent build-loop and the **live task ledger** (DONE/TODO with commit hashes), kept current through Phase 5.6. |
| [07-evaluator-semantics.md](07-evaluator-semantics.md) | A faithful spec of MUGEN trigger-expression numeric/evaluation semantics — the contract the `fp-vm` evaluator implements (Value model, coercions, never-crash bottom→0). |
| [08-faithfulness-audit.md](08-faithfulness-audit.md) | 39 ranked MUGEN-fidelity items (the source the authoritative [../known-issues.md](../known-issues.md) mirrors). Note: a few inline ✅DONE markers can lag the code — verify done-status against source. |

## How to use this

- **Implementing a parser?** Start with the format's section in [03](03-engine-architecture.md), then check the spec under [../format-specs/](../format-specs/).
- **Picking the next phase?** See the gap analysis in [05](05-reimplementation-roadmap.md), the live ledger in [06](06-execution-plan.md), and the current [../roadmap.md](../roadmap.md).
- **Implementing the expression evaluator?** [07](07-evaluator-semantics.md) is the semantic contract.
- **Tracking fidelity gaps?** [08](08-faithfulness-audit.md) (research) ↔ [../known-issues.md](../known-issues.md) (authoritative).
- **Wondering whether something is legal to ship?** [05 § Legal](05-reimplementation-roadmap.md#legal--clean-room-guidance).

## Source quality

Engine/format facts are anchored in the Elecbyte documentation and corroborated by independent
parsers (e.g. `bitcraft/mugen-tools`) and the MIT-licensed **Ikemen GO** reimplementation.
Cultural/scale facts come from community wikis and retrospectives — reliable for *consensus and
vocabulary*, but exact counts are estimates. Inline citations appear throughout; each doc ends
with a sources list.

_Compiled 2026-06-13 from a multi-agent research + codebase-verification sweep._
