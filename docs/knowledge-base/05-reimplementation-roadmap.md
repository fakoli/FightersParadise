# 05 — Reimplementation Roadmap & Strategy

Synthesizes the engine reference ([03](03-engine-architecture.md)) and the verified codebase review
([04](04-codebase-review.md)) into a build plan, a clean-room legal posture, and a conformance
strategy.

## Where we are vs. what MUGEN is

| MUGEN concept ([03](03-engine-architecture.md)) | Fighters Paradise crate | State |
|---|---|---|
| Asset formats (SFF/AIR/CMD/DEF) | `fp-formats` | ✅ v2 + AIR/CMD/DEF done; **SFF v1, CNS, SND, FNT missing** |
| Rendering (palette-indexed, blend) | `fp-render` | ✅ Done |
| Input buffer + command matching | `fp-input` | ✅ Done |
| Physics (gravity/friction/ground) | `fp-physics` | ✅ Done (AABB pending) |
| **Expression VM** (triggers) | `fp-vm` | ❌ Stub — **the keystone** |
| **State machine** (Statedef + ~100 controllers) | `fp-character` + CNS parser | ❌ Stub |
| **Combat** (HitDef/Clsn/juggle/guard) | `fp-combat` | ❌ Stub |
| Stage + camera | `fp-stage` | ❌ Stub |
| Audio | `fp-audio` | ❌ Stub |
| UI (lifebars/menus/screenpacks) | `fp-ui` | ❌ Stub |
| Round flow / match coordination | `fp-engine` | ❌ Stub |
| Storyboards/cutscenes | `fp-storyboard` | ❌ Stub |

**The critical path is the interpreter triad: `fp-vm` → CNS parser → `fp-character`.** Until those
exist, every higher-level system has nothing data-driven to act on. The existing roadmap (Phases
4–11 in the README) already sequences this correctly.

## The keystone: why `fp-vm` comes first

A MUGEN character is *zero compiled code* — all behavior is **trigger expressions** evaluated every
tick ([03 § 4](03-engine-architecture.md#4-the-trigger--expression-system)). Nothing data-driven
works until you can evaluate `trigger1 = AnimElem = 6 && P2BodyDist X < 30`. So `fp-vm` is the
literal foundation of Phases 5–11.

Design guidance (consistent with CLAUDE.md):
1. **Compile at load, interpret at runtime.** Parse each trigger expression once into bytecode; run
   a small stack VM per evaluation. Thousands of evals/tick × many entities → don't re-parse strings.
2. **Evaluation context with redirections.** The VM must resolve `enemy,...` / `root,...` /
   `helper(id),...` *before* reading a trigger — i.e. the interpreter takes a "current entity +
   redirection target" context, not just a flat variable table.
3. **MUGEN quirks are correctness, not bugs.** `AnimElem` is 1-indexed and edge-triggered; integer
   vs. float coercion follows MUGEN's rules; operator set includes `**`, `:=`, ranges `[a,b]`. Bake
   these into the VM's test suite from day one.
4. **Determinism.** Mirror MUGEN's float evaluation order if you ever want replays/rollback netcode.

> This is the highest-leverage, highest-subtlety crate in the project. It deserves the most tests
> and the most careful design — it's where a "works for KFM but breaks on real characters" engine is
> made or avoided.

## Suggested build order

Reordered slightly from the README to surface compatibility-critical and de-risking work earlier.

### Phase 4 — Expression VM + CNS parser  *(unblocks everything)*
- `fp-vm`: lexer → Pratt/precedence parser → bytecode → stack interpreter. Cover the full operator
  set, `cond/ifelse`, ranges, and the standard triggers in [03 § 4](03-engine-architecture.md#4-the-trigger--expression-system).
- `fp-formats`: CNS parser → `Statedef` + `[State N, id]` controller blocks with raw trigger
  expressions attached (compiled by `fp-vm`).
- **Done when:** you can load `kfm.cns` (or our fixture) and evaluate its triggers against a mock
  entity state.

### Phase 5 — Data-driven state machine  *(`fp-character`)*
- Character struct (the struct-based entity from CLAUDE.md), the per-tick `-3 → -2 → -1 → current`
  execution order, `persistent`/`ignorehitpause` semantics, and the core state controllers
  (ChangeState, VelSet/Add, ChangeAnim, VarSet, PlaySnd, CtrlSet, PosAdd, …).
- **Delete the hardcoded SM in fp-app**; drive movement from CNS instead.
- **Done when:** the fixture character walks/jumps/crouches/attacks entirely from its CNS, with no
  hardcoded constants.

### Phase 6 — Combat  *(`fp-combat`)*
- HitDef application, Clsn1×Clsn2 overlap detection, priority/trade resolution, damage, guard, the
  **p1stateno/p2stateno state-takeover** mechanic, GetHitVars, and juggle accounting.
- Needs AABB collision in `fp-physics` (currently pending) + character-vs-character push.
- **Done when:** two fixture characters can hit, guard, combo, and KO each other.

### Phase 7 — Round flow  *(`fp-engine`)*
- Move the game loop out of `fp-app` into `fp-engine`; coordinate P1/P2, round states (intro →
  fight → KO → win), timer, win conditions. `fp-app` becomes a thin shell.

### Phase 8–11 — Stage, audio, UI, storyboard
- `fp-stage` (backgrounds/parallax/camera), `fp-audio` + SND parser, `fp-ui` + FNT parser (lifebars,
  select screen, screenpacks), `fp-storyboard`. These are largely independent and parallelizable
  once the engine core exists.

### Cross-cutting (schedule opportunistically)
- **SFF v1 parser** — required to load the huge *legacy* WinMUGEN content library, which is most of
  what exists. High *content-compatibility* value even though it's not on the linear roadmap.
- **Integration tests** — currently zero. Add end-to-end "load fixture → run N ticks → assert state"
  tests as soon as `fp-character` lands.
- **Collision-box debug overlay** — invaluable while building `fp-combat`.

## Legal / clean-room guidance

> This is the load-bearing constraint. Get it wrong and the project is unshippable.

**The principle:** *formats and behavior are facts you may reimplement; Elecbyte's code and anyone's
copyrighted assets are not yours to ship.* (Background: [02 § 4](02-character-ecosystem.md#4-the-ip--copyright-reality).)

**Do:**
1. **Reimplement from the documented formats + observed behavior.** The Elecbyte docs and community
   wikis *describe* behavior; they are not Elecbyte's source. Implementing readers/writers for
   format compatibility is the standard interoperability path.
2. **Use Ikemen GO (MIT, Go) as the behavioral tie-breaker.** It is itself a clean reimplementation;
   referencing *how it behaves* is legitimate. (Don't copy GPL code into an MIT project, etc. — but
   Ikemen GO is MIT, so reading it for behavior is fine.)
3. **Author your own original sample character, common-state file, common spark/sound FX, fonts, and
   default motif.** Ship those.
4. **Keep engine and content strictly separable** — exactly as Elecbyte and Ikemen GO do.

**Don't:**
1. **Don't decompile or copy MUGEN.** No reverse-engineering of the engine binary.
2. **Don't bundle Elecbyte assets** — no Kung Fu Man, no `common1.cns`, no Elecbyte fightfx/common
   sounds/sprites, no system fonts, no default motif.
3. **Don't bundle copyrighted character content** (SNK/Capcom/anime/etc.). Format compatibility lets
   *users* load their own content at their own risk; our distribution ships only original or
   permissively/CC-licensed assets.

The current repo posture is already correct — README states: *"Fighters Paradise is an independent
project. MUGEN is a trademark of Elecbyte. This project does not include any Elecbyte code or
assets."* Keep it that way; the risk concentrates the moment ripped content is bundled *with* the
engine.

## A KFM-equivalent conformance fixture

KFM is the universal MUGEN smoke test ([02 § 1](02-character-ecosystem.md#1-kung-fu-man-kfm--the-reference-fixture)),
but **KFM is an Elecbyte asset we cannot redistribute.** We need our own.

**Proposal:** build an **original "training dummy" character** — fully original sprites (even
programmer-art/geometric is fine to start), with a complete file set (`.def/.sff/.air/.cmd/.cns/.snd`)
exercising the engine's breadth: idle/walk/jump/crouch, a normal attack with a HitDef, a special move
via `.cmd`, a projectile (helper/Projectile), and a couple of get-hit states. Ship it in `assets/`
and wire the demo + integration tests to it instead of `kfm.*`.

This gives us:
- A **legally shippable** default character (the thing every download needs).
- A **golden conformance fixture** for CI ("load fixture, run, assert").
- A **worked example/tutorial** for our own format — the same role KFM plays for MUGEN.

**Compatibility test ladder** (from [02](02-character-ecosystem.md#recommended-compatibility-test-ladder)),
to run against *user-supplied* content, not bundled:
1. Our fixture (core pipeline).
2. Stock KFM + faithful source-accurate characters (correct frame data/hitboxes/complex states).
3. Cheap / god-tier / joke characters (adversarial: custom AI, helpers, odd controllers, extreme
   scaling) — the SaltyBet long tail is the real robustness bar.

## Open questions for the maintainer

These shape the architecture and are worth deciding before Phase 4:

1. **Compatibility target & breadth.** Lock **MUGEN 1.1 beta** as the baseline (matching Ikemen GO)?
   And is **SFF v1 (legacy WinMUGEN) loading** in scope for v1.0, or deferred? This decides how much
   *real-world content* the engine can actually run.
2. **VM strategy.** Confirm the bytecode-compile-then-interpret approach (vs. a tree-walking
   interpreter). Bytecode is more work up front but the right call for per-tick eval at scale.
3. **Determinism goal.** Are replays / rollback netcode a goal? If yes, float-exactness and
   evaluation-order fidelity become hard requirements from Phase 4 — much cheaper to bake in now than
   retrofit.
4. **First fixture scope.** How elaborate should the original conformance character be for v1 — a
   minimal 3-move dummy, or a fuller showcase character?

See [README](README.md) for the doc index.
