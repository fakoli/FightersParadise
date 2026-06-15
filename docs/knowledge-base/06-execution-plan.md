# 06 — Execution Plan: The Build Loop & Agent Team

This is the **operational plan** for building Fighters Paradise roadmap-item-by-roadmap-item via a
repeatable, reviewed agent loop. It is the durable state the loop reads and updates each iteration.

- **What to build** comes from [05-reimplementation-roadmap.md](05-reimplementation-roadmap.md).
- **How it behaves** comes from [03-engine-architecture.md](03-engine-architecture.md).
- **This doc** = the *who* (agent team), the *how* (loop protocol), and the *task ledger* (ordered
  work + status). The ledger at the bottom is the single source of truth for "what's next."

---

## The agent team

Five specialized roles. Each maps to a concrete subagent implementation. The orchestrator (main
loop) selects the task, runs the team, verifies, and records.

| Role | Codename | Implementation | Responsibility |
|------|----------|----------------|----------------|
| **Coding** | **Forge** | `general-purpose` @ **opus** | Implements the task in idiomatic Rust against its acceptance criteria. Follows CLAUDE.md conventions (never panic, `thiserror`, `tracing`, `#![warn(missing_docs)]`). |
| **Testing** | **Proctor** | `general-purpose` | Writes/extends unit + integration tests, runs `cargo test`/`clippy` for affected crates, reports pass/fail + output. |
| **Review** | **Critic** | `fakoli-crew:critic` | Staff-engineer Rust review of the diff vs. acceptance criteria. Verdict: `PASS` / `SHOULD_FIX` / `MUST_FIX`. Reports, does not fix. |
| **Infrastructure** | **Anvil** | `general-purpose` | Toolchain, Cargo workspace wiring, CI, dependency/build/lint config, test-asset plumbing. Not engine logic. |
| **Research** | **Scout** | `fakoli-crew:scout` / `general-purpose`+web | Background investigation: format/behavior details, sample content, prior art (Ikemen GO, Elecbyte docs). Produces structured findings. |

> **Toolchain note for all agents:** Rust is installed via rustup. If `cargo` isn't on `PATH`, use
> `source "$HOME/.cargo/env"` first, or call `"$HOME/.cargo/bin/cargo"` directly. SDL2 is installed
> via Homebrew; `.cargo/config.toml` adds `/opt/homebrew/lib` to the linker path.

---

## The loop protocol

Each iteration completes **one** ledger task (the first `TODO`, respecting dependencies):

1. **Select** — read the ledger; pick the next `TODO` whose dependencies are all `DONE`.
2. **Research** *(conditional)* — if the task is flagged `needs-research`, **Scout** gathers what's
   needed first; its findings feed Forge.
3. **Implement** — **Forge** writes the code to satisfy the acceptance criteria.
4. **Test** — **Proctor** adds/updates tests and runs `cargo test` (+ `clippy`) on affected crates.
5. **Review** — **Critic** reviews the diff vs. acceptance criteria → verdict.
6. **Fix gate** — if `MUST_FIX` or tests/clippy fail → Forge fixes → re-test → re-review. Bounded to
   **2 retries**; if still red, mark the task `BLOCKED` with notes and stop.
7. **Verify** — orchestrator runs `cargo build && cargo test && cargo clippy --workspace` to confirm
   the whole workspace is green.
8. **Record** — mark the task `DONE` (or `BLOCKED`), append evidence (tests added, review verdict),
   and update `CHANGELOG.md` / `CLAUDE.md` / the crate status table when structure changed.
9. **Report & continue** — summarize the iteration; the loop fires the next one.

**Loop body** = the reusable workflow at `../../../.fp-loop/fp-build-task.mjs` (outside the engine
repo, to keep it clean). It runs steps 2–6 as a deterministic pipeline; the orchestrator does 1, 7,
8, 9.

### Definition of Done (every task)
- Acceptance criteria met.
- New behavior covered by tests; `cargo test --workspace` green.
- `cargo clippy --workspace` clean (zero warnings — repo standard).
- Critic verdict `PASS` (or `SHOULD_FIX` with deferred items logged to the backlog).
- Public items documented (`#![warn(missing_docs)]` holds).

### Guardrails
- **Clean-room:** never copy Elecbyte/MUGEN engine source or copyrighted assets into the repo.
  Sample content lives in gitignored `test-assets/` and is for local testing only.
- **Never panic on bad content:** parsers/VM return `FpResult` + safe defaults (CLAUDE.md rule).
- **One task at a time** on the shared tree (no parallel file mutation) until tasks are provably
  independent — then isolate with worktrees.

### Known real-content gotchas (from the KFM fixture)
- **Text formats are UTF-8 *with BOM* + CRLF.** `kfm.cns/.air/.cmd` start with a UTF-8 BOM and use
  `\r\n`. The CNS parser (4.5) **must** strip a leading BOM and tolerate CRLF; AIR/CMD/DEF parsers
  should be re-checked against the real fixture too. (Existing parsers were only tested on synthetic
  inline bytes.)
- **KFM 1.0 uses SFFv2 sprites** but **SFFv1 intro/ending sprites** — one fixture exercises both
  sprite paths.

---

## How to run the loop

```
/loop continue building FightersParadise per docs/knowledge-base/06-execution-plan.md
```

Each firing: the orchestrator reads the ledger, runs the loop body on the next task, verifies,
updates the ledger, and reports. Stop anytime; state is durable in this file.

---

## Task ledger

Status keys: `TODO` · `DOING` · `DONE` · `BLOCKED`. Tasks run top-down; respect `deps`.

### Phase 0 — Foundation & infrastructure  *(unblocks the loop itself)*

| ID | Status | Task | Crate(s) | Acceptance criteria | Deps |
|----|--------|------|----------|--------------------|------|
| 0.1 | DONE | Install Rust toolchain; establish green baseline | — | ✅ rustup 1.96.0; `cargo test --workspace` = **139 passed / 0 failed**; only a transitive-dep (`block 0.1.6`) future-incompat notice, not our code. | — |
| 0.2 | DONE | Pull sample MUGEN content (KFM + extras) into gitignored `test-assets/` | — | ✅ Full KFM set in `test-assets/kfm/` (CC BY-NC, Elecbyte) — `kfm.{def,cns,cmd,air,snd}`, `kfm.sff` (**SFFv2**), intro/ending/motif (**SFFv1**), `common1.cns`; all `.sff` verified `ElecbyteSpr`; `test-assets/` gitignored. See `test-assets/SOURCES.md`. | — |
| 0.3 | DONE | Real-fixture integration tests (absorbs CB1) | `fp-formats/tests` | ✅ `tests/real_content.rs` (7 skip-if-missing tests) load real KFM. **Found & fixed 2 real bugs:** SFFv2 header read counts at wrong offsets → loaded **0 sprites** (rewrote `sff/header.rs`); SFFv1 was rejected (added `sff/v1.rs` w/ PCX RLE). + BOM strip in air/cmd/def. Critic **PASS**. Workspace **380 tests** green. | 0.1, 0.2 |
| 0.4 | DONE | CI workflow: build + test + clippy on push/PR | `.github/workflows` | ✅ `.github/workflows/ci.yml` — SDL2 + stable toolchain; clippy `-D warnings`, build, test on push+PR to main. Activates on first push (not pushed autonomously). | 0.1 |

### Phase 4 — Expression VM + CNS parser  *(the keystone — see [05](05-reimplementation-roadmap.md#the-keystone-why-fp-vm-comes-first))*

| ID | Status | Task | Crate(s) | Acceptance criteria | Deps |
|----|--------|------|----------|--------------------|------|
| 4.1 | DONE | Expression **lexer** | `fp-vm` | ✅ `lexer::{tokenize, Token, TokenKind}` — **46 unit + 2 doc tests**; clippy + `cargo doc -D warnings` clean. Infallible `tokenize -> Vec<Token>`; bad chars → `TokenKind::Unknown(c)` (debug-logged, flood-safe); accepts `==` alias. Critic SHOULD_FIX applied: fixed broken `crate::parser` intra-doc link, downgraded Unknown logging to `debug!`, documented int-overflow→0 safety-net (saturation decision → CB4). | 0.1 |
| 4.2 | DONE | **AST + precedence parser** | `fp-vm` | ✅ `parser::{Expr, BinaryOp, UnaryOp, Bound, ParseError, parse, parse_str}` — precedence-climbing; **81 lib + 5 doc tests**; clippy + doc clean. Critic **PASS**. SHOULD_FIX applied: precedence ladder now documented in [03 §4](03-engine-architecture.md#4-the-trigger--expression-system). | 4.1 |
| 4.3 | DONE | **Trigger model + EvalContext** | `fp-vm` | ✅ `eval::{Value, Redirect, EvalContext, MockContext}` — Int(`i32`)/Float(`f32`) with saturating coercions (CB4), object-safe trait, in-memory mock; **148 unit + 8 doc tests**; clippy + doc clean. Critic SHOULD_FIX folded into 4.4 (remove test-only `t_ref`; decide `enemy(n)`/`enemynear` index → CB8). | 4.2 |
| 4.4 | DONE | **Evaluator** (tree-walk) | `fp-vm` | ✅ `evaluator::eval(&Expr, &dyn EvalContext) -> Value` + Park–Miller `Rng` seam; full 07-semantics (trunc int div, div/0→0, right-assoc saturating `**`, short-circuit, ranges+`!=`, lazy cond/ifelse, math fns). **227 lib + 11 doc tests.** Orchestrator fixed Critic SHOULD_FIX: **bare `random` was bypassing the RNG seam** (returned 0; a test *pinned* the bug) → routed `Ident("random")`→`eval_random`, rewrote the test to assert real [0,999]. | 4.3 |
| 4.5 | DONE | **CNS parser** | `fp-formats` | ✅ `cns::{CnsFile, Statedef, StateController, TriggerGroup}` — BOM/CRLF-tolerant, negative statedefs, parses real `kfm.cns` + `common1.cns`; **72 tests**; clippy clean. Critic **PASS**. Also fixed 2 pre-existing `manual_repeat_n` clippy errors in `sff/compression.rs`. SHOULD_FIX applied: contiguity deviation documented on `StateController::triggers` (→ CB6). | 0.1 |
| 4.6 | DONE | **Integration:** kfm.cns → lex/parse/eval (keystone validation) | `fp-vm`/tests | ✅ `tests/cns_integration.rs` — **812 real triggers, 733 parse cleanly (90.3%), 0 panics**; curated triggers evaluate correctly. Critic **PASS**. Surfaced 4 real gaps (NOT `:=`) → task **4.10**. | 4.4, 4.5, 4.8, 0.3 |
| 4.7 | TODO | *(perf, optional)* AST → **bytecode** + stack VM | `fp-vm` | Compile AST to bytecode; stack interpreter matches tree-walk results on the test corpus; micro-bench recorded. | 4.4 |
| 4.10 | DONE | **Real-content trigger support** | `fp-vm` | ✅ kfm.cns clean-parse **90.3%→100% (812/812)**. Axis-suffix triggers (`Vel Y`/`Pos X`), `AnimElem=N,op M`, dotted call args, **`command="x"` string-eq** (via `EvalContext::command_active` — moves fire now). 685 workspace tests. Commit `145ed85`. Critic SHOULD_FIX → 4.11. | 4.6 |
| 4.11 | DONE | **VM correctness follow-ups** (4.10 review) | `fp-vm` | ✅ Critic **PASS, no should-fix**. (a) `EvalContext::trigger_str` member-key seam (GetHitVar passes name, not value); (b) TimeMod/AnimElemNo dropped from AnimElem family (safe degrade); (c) AnimElem tail at relational precedence (no `&&`-swallow). 721 workspace tests. Commit `8b66c87`. | 4.10 |
| 4.8 | DONE | **Redirection** parsing + eval | `fp-vm` | ✅ `Expr::Redirected`; parser lookahead binds redirect looser than all ops + nests (`enemy, helper(1), x`); evaluator `eval_redirect` via `EvalContext::redirect` (missing→0). **CB8 resolved**: `enemy(n>0)`→`EnemyNear(n)` (lowered + documented). +31 tests. Critic **PASS** (doc/warn nits → CB12). | 4.2, 4.4 |
| 4.9 | TODO | **`:=` assignment** parsing + eval | `fp-vm` | Add `Expr` + parser + eval for in-expression assignment `var(x):=y` / `fvar(x):=y` (returns the stored value per [07](07-evaluator-semantics.md)); evaluator needs a mutable context hook. Deferred by 4.2. | 4.2, 4.4 |

### Phase 5 — `fp-character` (data-driven state machine)  *(the demo-able milestone)*

Replaces `fp-app`'s hardcoded movement with a character driven entirely by its own CNS. Built on the
finalized fp-vm `EvalContext`. **Phase 4 is complete** (4.1–4.6, 4.8, 4.10, 4.11; 4.7 bytecode + 4.9
`:=` remain optional/deferred).

| ID | Status | Task | Crate(s) | Acceptance criteria | Deps |
|----|--------|------|----------|--------------------|------|
| 5.1 | DONE | **Character entity struct + `EvalContext` impl** | `fp-character` | ✅ `Character` struct (pos/vel, facing, life/power, ctrl, statetype/movetype/physics enums, anim+elem+time, state-no/prev/time, var/fvar/sysvar banks, constants) + `impl EvalContext` resolving standard KFM triggers (incl. letter-coded `StateType=A`) + `CommandSource` seam; 33 tests evaluate real parsed triggers through fp-vm. Critic SHOULD_FIX → **folded into 5.2**: `AnimElemTime` must return a NEGATIVE sentinel for not-reached elements (VM tail-guard contract; currently 0 → every element reads "reached"); + use/remove the `tracing` dep. | 4.11 |
| 5.2 | DONE | **Character loader** (.def → ready Character) | `fp-character` | ✅ `LoadedCharacter::load` — parses .def, loads+merges SFF/AIR/CNS(+stcommon)/CMD/SND, reads `[Data]` constants, compiles all exprs via fp-vm (bad→const-0+warn). Real `kfm.def` loads end-to-end (gated). 5.1 AnimElemTime sentinel fixed; tracing used. 844 tests. Commit `e8b9775`. SHOULD_FIX: `[Size]/[Velocity]/[Movement]` constants → **5.3**; merge-order/I-O-dedup/cmd-snd-error nits → CB16-18. | 5.1 |
| 5.3 | DONE | **State-machine executor** (+ full constants) | `fp-character` | ✅ `Character::tick()` — `-3/-2/-1/current` order; triggerall+group gating w/ **CB6 contiguity**; persistent/ignorehitpause; state entry + ChangeState; anim advance from AIR; statedef physics (S/C friction, A gravity). Dispatch: ChangeState/VelSet/VelAdd/CtrlSet/Null. Constants `[Size]/[Velocity]/[Movement]` loaded (5.2 overclaim fixed). Gated KFM 30-frame tick. 912 tests. Commit `eba6d70`. SHOULD_FIX → 5.4. | 5.1, 5.2 |
| 5.4 | DONE | **Core state controllers** (+ 5.3 fixes) | `fp-character` | ✅ ChangeAnim(2), PosSet/Add, VarSet/Add/VarRangeSet (all banks), StateTypeSet, Turn, PlaySnd-stub — data-driven. All 4 5.3 fixes landed (jump.up 2-comp, fire_counts keying, exit-clause, prev_state test). 177 fp-character / 960 workspace tests. Commit `3173af0`. SHOULD_FIX (StateTypeSet expr-token test, VarRangeSet doc reword, ignorehitpause clarity) → CB19-21. | 5.3 |
| 5.5 | DONE | **fp-app integration** (KFM moves from CNS) | `fp-app`+`fp-character` | ✅ **PHASE 5 DONE** — fp-app loads KFM, feeds input→commands, ticks the CNS state machine, renders the current AIR frame; hardcoded SM removed; loader merges `.cmd` `[Statedef -1]`. Headless test (runs): hold-Forward→walk state 20→stand. 979 tests; clippy clean. Commit `ccebde1`. **Band-aids** (inject walk bridge, drop `alive`, strip `$`/`>`) mask engine gaps → **5.6**. | 5.4 |
| 5.6 | SPLIT | **Engine-gap fixes** (faithful KFM) — split into 5.6a/b/c | — | Original combined task BLOCKED (workflow's single-crate scope-guard boxed the agent in fp-input). Re-split per crate (each fits the workflow + is more reviewable). **Loop policy learned: workflow tasks = ONE crate; multi-crate work must be split (or use a direct agent for genuinely atomic cross-crate changes).** | 5.5 |
| 5.6a | DONE | fp-input `$`/`>` command symbols | `fp-input` | ✅ `$` (direction-detect) + `>` (strict) in compile_command/CommandMatcher; `/$F` (holdfwd) compiles + matches when forward held (incl. diagonals). +16 tests. 1061 workspace tests. | 5.5 |
| 5.6b | DONE | `alive` trigger (+ common-state trigger audit) | `fp-character` | ✅ `alive`=>Life>0 (case-insensitive); audited all kfm/common1 triggers, documented the rest (HitOver/RoundState/P2BodyDist…) as deferred to Phase 6/7. +10 tests; 1073 workspace. Critic PASS. Commit `470dafe`. | 5.5 |
| 5.6c | DONE | Remove fp-app band-aids (faithful KFM walk) | `fp-app` | ✅ 2 of 3 band-aids removed (raw `/$F` compiles; `alive` keeps stand out of death 5050). Strengthened walk test (state 20 + strictly advancing). 1 minimal TODO'd shim left, gated on 2 diagnosed gaps → 5.6d + CB25. 1084 tests. Commit `7983361`. | 5.6a, 5.6b |
| 5.6d | DOING | **`const(<member>)` resolution** (vm routing) | `fp-vm` | Route `const(<member>)` like `GetHitVar` — pass the member NAME via `trigger_str` (member-keyed func set), not evaluate the dotted ident as a nested trigger. Additive/safe. Enables 5.6e. | 4.11 |
| 5.6e | DONE | **`const(<member>)` resolution** (char resolver) | `fp-character` | ✅ `trigger_str("const",member)` → CharacterConstants (velocity/size/movement/data). Gated KFM test: `const(velocity.walk.fwd.x)`==2.4 via parse+eval. +17 tests; 1125 workspace. Commit `2dc4e09`. **Closes the const() gap.** Residuals: CB26 (drop shim velocity-repair), CB25 (stand↔walk engine built-in). | 5.6d |

### Phase 6 — `fp-combat` (characters can fight)

Built on the faithful VM (const/alive/commands resolved). Physics prep done: **P6.1** ✅ AABB
(`collision.rs`), **P6.2** ✅ push/bound (`push.rs`).

| ID | Status | Task | Crate(s) | Acceptance criteria | Deps |
|----|--------|------|----------|--------------------|------|
| 6.1 | DONE | **HitDef data model + hit-detection primitive** | `fp-combat` | ✅ `HitDef` plain-data + `AttackAttr::parse` + `HitFlags` bit-set + `detect_hit`/`detect_hit_contact` (place_clsn+any_overlap). Leaf crate (fp-physics+fp-core, no cycle). Manual Default w/ MUGEN sentinels (hitflag `MAF`, chainid `-1`). +24 tests; 1163 workspace. Commit `7d7888c`. | P6.1 |
| 6.2 | DONE | **HitDef controller + GetHitVars + get-hit states** | `fp-character` | ✅ HitDef controller builds `active_hitdef` (string params raw, numeric via eval); `GetHitVars` + `trigger_str("gethitvar")` resolves the last 5.6b deferral; get-hit states 5000-5xxx documented runnable. 250 fp-character / 1195 workspace. Commit `bfc5a58`. SHOULD_FIX → 6.2b + CB27. | 6.1 |
| 6.2b | DONE | **Multi-component param model** (loader) | `fp-character`(+fp-app) | ✅ `CompiledParam{components}` splits params on top-level commas; controllers read `eval_param_component`; no spurious warns; CB27 fixed. **Public-type change broke fp-app** (params type) → fp-app migrated to `CompiledParam` in the same (amended) commit. 1221 tests. Commit `ccc4908`. Removing the walk repair exposed the **facing-relative velocity** gap → 6.2c. | 6.2 |
| 6.2c | DONE | **Facing-relative velocity** (executor) + drop walk repair | `fp-character`+`fp-app` | ✅ World-pos integration moved INTO the executor: `pos.x += vel.x * facing` (vel stays facing-relative; `Vel X` facing-relative, validated vs common1 walk anim-select; PosAdd facing-relative; PosSet/Pos X absolute). fp-app walk repair removed (**CB26 done**); CB25 bridge kept. KFM walks both facings from its own data. 1228 tests. Commit `127883e`. | 6.2b |
| 6.3a | DONE | **Hit resolution logic** (pure) | `fp-combat` | ✅ `resolve_hit(&HitDef, DefenderState) -> HitOutcome` — Guard/Hit/Miss (guard=holding-back+guardflag admits stance; empty=unblockable), damage, facing-relative knockback, pause/hit times, fall, get-hit state (p2stateno else 5000/5010/5020). 42 tests; Critic PASS. Commit `103da80`. SHOULD_FIX → CB28 (distinct slidetime/ctrltime). | 6.1 |
| 6.3b | DONE | **Hit detection + apply** (two characters) | `fp-character` | A function over two Characters: extract current-frame Clsn1(attacker)/Clsn2(defender) from their AIR, run `fp_combat::detect_hit`; if hit + attacker has `active_hitdef` + not already hit this move, call `resolve_hit`, then APPLY to defender — life -= damage, set knockback vel (mirrored by attacker facing), ChangeState to the get-hit state (`p2stateno` or common 5000-series), populate `GetHitVars`, set hitpause on both, clear/flag the HitDef (hitonce). Unit-tested with two synthetic Characters. | 6.3a, 6.2 |

(Live two-player fights on screen — ticking P1+P2, running detection each tick, KO/round — are
**Phase 7** `fp-engine` + fp-app, where the demo becomes an actual match.)

### Phase 7 — `fp-engine` + 2-player (the playable match)

**Phase 6 combat mechanics COMPLETE** (6.1 HitDef model, 6.2 controller, 6.2b param model, 6.2c
facing velocity, 6.3a resolution, 6.3b detection+apply). Now wire two characters into a real match.

| ID | Status | Task | Crate(s) | Acceptance criteria | Deps |
|----|--------|------|----------|--------------------|------|
| 7.1 | DONE | **Match coordinator** (2 players) | `fp-engine` | `Match` holds P1+P2 (Character + LoadedCharacter each); `Match::tick()` ticks both, runs `combat::resolve_attack` BOTH directions, applies player-push (P6.2) + screen-bound clamp, keeps each character facing the opponent (facep2 baseline), advances a round state machine (intro→fight→KO when a life hits 0→win) + a round timer. Headless-tested (two KFMs: P1 hit → P2 life drops; KO ends the round). Deps: fp-character/fp-combat/fp-physics. | 6.3b |
| 7.2 | DONE | **fp-app 2-player render + input** | `fp-app` | Drive a `fp-engine::Match` in the window: render BOTH characters from their current AIR frame; P1 = keyboard, P2 = idle/dummy (or a 2nd input map); a minimal life readout; round-state feedback (KO). The "two characters fight on screen" demo. Deps: 7.1 |
| 7.3 | DONE | **Command-pipeline fidelity + built-in locomotion** (atomic, multi-crate → direct Agent, not the single-crate workflow) | `fp-engine`+`fp-character`+`fp-app` | **A:** each `Player` owns an `InputBuffer` + `CommandMatcher` built from the loaded `.cmd`; `Match::tick` takes RAW directional+button input, runs each player's matcher → real command set (`holdfwd`/`FF`/`QCF_x`/specials), feeding the executor. Deletes the invented `fwd`/`back` names + `inject_walk_velocity_bridge`. **B:** implement MUGEN's hardcoded ground locomotion command-states in the executor (gated on `ctrl`), the transitions ABSENT from KFM data — empirically confirmed missing from common1 `[Statedef 0]` AND kfm.cmd `[Statedef -1]`: `0→40` holdup, `0→10` holddown, `0→20` holdfwd∥holdback; `20→40`, `20→10`, `20→0` (release); `11→12` !holddown. **C:** delete the three fp-app shims; fp-app feeds raw input only. Stock KFM must walk/crouch/jump/stop with ZERO app shims; two-player headless attack test still green. Airjump (45 on holdup-in-air) deferred → CB. Deps: 7.2 |

### Phase 8–11 — stage / audio / ui / storyboard  *(expand when reached)*
`fp-stage`, `fp-audio`+SND, `fp-ui`+FNT (lifebars/select/screenpacks), `fp-storyboard`. Largely
parallelizable once the core exists. Deps: Phase 7.

- **S8.1** ✅ DONE — **SND parser** (`fp-formats/src/snd.rs`): `SndFile{load, from_bytes,
  sound(group,sample)}`, validates `ElecbyteSnd`, walks the directory (count-terminated,
  cycle-guarded), real `kfm.snd` = 12 RIFF sounds. +18 tests. Critic PASS (fragile truncation test → CB13).
- **8.2** DONE — **fp-audio playback core** (`fp-audio`, single-crate). Build the stub into: `Sound::decode(&[u8])` (rodio WAV→in-memory replayable PCM); object-safe `AudioBackend` seam + `RodioBackend` (graceful no-device), `NullBackend`, test `RecordingBackend`; `AudioSystem` with MUGEN channel cut-off (occupied channel stops previous; channel<0 always-new) + NullBackend fallback. Decoupled from fp-formats (operates on raw WAV bytes). rodio wired into Cargo.toml (orchestrator infra). Deps: S8.1. *(running via fp-loop-batch w7q1rehbq)*
- **8.3a** DONE — `PlaySnd` controller emits `SoundRequest` into `TickReport` (`fp-character`, single-crate). Replace the logged no-op stub: parse `value=group,sample` (optional `F` group-prefix → `common` flag), `channel`, `volumescale`, `loop`; push a documented `SoundRequest{group,sample,channel,volume_scale,loop,common}` onto `TickReport.sound_requests` (cleared each tick). No audio dep — fp-character stays pure simulation; the request is data the engine/app plays. Deps: 8.2. *(via fp-loop-batch)*
- **8.3b** DONE — surface + play (`fp-engine` exposes per-player `sound_requests` from `tick`; `fp-app` owns an `fp_audio::AudioSystem` + lazily decodes `LoadedCharacter` SndFile sounds → `Sound`, plays each request). Cross-crate → direct Agent `ac56ee167f6c6abe4`. Deps: 8.3a.
- **8.4** DONE — HitDef `hitsound`/`guardsound` impact audio + fidelity fix (`HitResources.hitsound`/`guardsound` are currently a lossy single `i32` dropping the sample; real KFM uses `hitsound = 5, 0`). fp-combat: `SoundId{group,sample,common}` + parse group,sample with F-prefix; fp-character: `AttackResolution.hit_sound` (hitsound on Hit, guardsound on Guard); fp-engine: stop discarding `resolve_attack`, append the attacker's hit/guard sound as a `SoundRequest` (flows through the 8.3b play path). Cross-crate atomic → direct Agent. Deps: 8.3b. *(direct Agent)*

**Phase 8 (audio) COMPLETE** — SND parser (S8.1), fp-audio playback (8.2), PlaySnd emit+play (8.3a/b),
HitDef impact sounds (8.4). Hitsound/guardsound use the INVERSE prefix convention from PlaySnd
(default=common, `S`=own); orchestrator caught the Forge agent implementing it backwards.

- **11.1** DONE — Storyboard `.def` parser + typed scene model (`fp-storyboard`, single-crate).
  Validated against real `test-assets/kfm-motif-sffv1/intro.def` etc. (the only stub-crate work with
  real fixtures — no stage `.def`/`fight.def`/`.fnt` in assets). Parsing+model only; rendering deferred.
  Deps: DEF parser. *(via fp-loop-batch wkszpf4ly)*
- **6.5** DONE — **Hitpause (impact freeze)** (`fp-character`, single-crate). On a connecting hit both
  players freeze for `HitDef.pausetime.p1`/`.p2`; the executor tick skips anim/Time/physics advance
  while `Character.hitpause_time > 0`, running only `ignorehitpause`-flagged controllers (wired but
  unused since 5.3). A core combat-feel feature still missing. Symmetric-pause simplification (hitshake
  nuance deferred). Deps: 6.x combat. *(via fp-loop-batch wferstbec)*
- **6.6** DONE — **Power / super meter gain** (`fp-character`, single-crate). `Character.power` existed
  + the `Power` trigger read it, but power was NEVER increased → KFM supers (gated `power >= 1000`)
  unreachable. Apply the `[Statedef]` header `poweradd` on state entry (KFM's meter source — every
  attack state adds power) + implement the `PowerAdd`/`PowerSet` controllers; clamp to `[0,power_max]`.
  Enables KFM's signature super moves. Deps: 6.x. *(via fp-loop-batch wfh9xkpkd)*
- **7.4** DONE — **Best-of-N match flow** (`fp-engine`, single-crate). Match was SINGLE-round
  (Intro→Fight→Ko→Win terminal); real MUGEN is best-of-3. Add `rounds_to_win` (def 2), per-player
  round-win counts, `round_number`, a terminal match-over state + `match_winner()`, and round RESET
  between rounds (life→max, start positions/facing, clear transient state + timer). Deps: 7.1.
  *(via fp-loop-batch wn2xh1py6)*

### ⭐ Audit-driven roadmap — see [08-faithfulness-audit.md](08-faithfulness-audit.md)
The ultracode multi-agent audit verified **65 gaps** → **39-item prioritized roadmap** (full list +
rationale in 08). Drive these top-down; they supersede ad-hoc probing.
- **A.P1+P2** DONE — **Cross-entity eval context** (keystone): thread the opponent (+ stage bounds)
  into `Character::tick` so redirects (`p2`/`enemy`/`root`), `P2Dist`/`P2BodyDist`/`p2,life`/`p2,stateno`,
  and edge-distance triggers resolve (today all read 0 → AI/spacing/throws dead). `EvalCtx` wrapper;
  cross-crate (fp-character tick sig + fp-engine Match::tick + fp-app). Unblocks ~1/3 of the audit.
  *(direct Agent `a8f74496277caa12c`)*
- Next per 08: P3 SelfState, P4 jump/run velocities, P5 Const720p, P6 AnimElemTime(n), P7
  GetHitVar(animtype), P8 throw system (needs P1/P2), P9 NotHitBy/HitBy i-frames, …

### Cross-cutting backlog  *(schedule opportunistically; groomed each iteration)*
- ~~**SFF v1 parser**~~ ✅ DONE (task 0.3 added `sff/v1.rs` w/ PCX RLE decoder; loads intro/ending sprites).
- ~~**AABB collision** in `fp-physics`~~ ✅ DONE (task P6.1: `collision::{Clsn, Facing, rects_overlap, place_clsn, any_overlap, any_clsn_overlap}` — the Clsn1×Clsn2 hit-detection primitive). Critic PASS; orchestrator fixed a false `to_rect` doc claim.
- **Original conformance fixture character** (ships in `assets/`; replaces KFM in demo/CI) —
  see [05](05-reimplementation-roadmap.md#a-kfm-equivalent-conformance-fixture).
- Collision-box debug overlay; replay/determinism harness.
- **CB1** *(folded into task 0.3)* Re-validate AIR/CMD/DEF parsers against the real KFM fixture.
- **CB2** Lexer/parser **fuzz / property tests** (`fp-vm`) — adversarial-content robustness for the
  cheap/joke-character long tail. *(added 4.1 iter)*
- **CB3** Adopt **rustfmt**: run `cargo fmt --all` once (67 files currently unformatted), add
  `rustfmt.toml`, then enable the `cargo fmt --check` gate in CI. *(needs ratification — changes the
  project standard; CLAUDE.md currently mandates only test + clippy)*
- **CB4** *(resolved by [07-evaluator-semantics.md](07-evaluator-semantics.md))* Evaluator numeric
  semantics: values are int(`i32`)/float(`f32`), any float operand promotes; `/` is truncating integer
  division when both operands are int; `%` is int-only; `**` is right-assoc; **overflow SATURATES to
  `i32::MIN`/`i32::MAX`** (matches Ikemen GO — change the lexer's overflow→0 to saturate; use
  `wrapping_*` only on native i32 `+ - *`); `random` is a Park–Miller LCG (must live in rollback
  state). Implement in task 4.4.
- **CB5** **`fp-vm` prelude/error type** — if the VM needs its own `VmError`, wire it into `FpError`
  (`thiserror` `#[from]`) so the never-panic contract holds end-to-end. *(added 4.1 iter)*
- **CB6** ✅ DONE (T023). MUGEN **trigger-group contiguity** (a gap in `trigger1,2,4…` numbering kills
  later groups) is enforced at evaluation time. The rule lives in `fp_vm::triggers::active_group_indices`
  (the shared trigger engine) and `fp-character`'s executor (`contiguous_groups`) delegates to it; the CNS
  parser still deliberately keeps all groups. Unit-tested in fp-vm incl. the non-contiguous `1,2,4` case.
  *(added 4.2/4.5 iter; resolved T023)*
- **CB7** Make baseline/CI clippy run `--all-targets` (it caught a latent `useless_vec` in fp-render
  tests + `manual_repeat_n` in sff/compression that plain `cargo test` missed). CI already does;
  keep it. *(added 4.2/4.5 iter — fixed)*
- **CB8** Redirect coverage: decide how `enemy(n)` (n>0, simul/turns) and bare `enemynear` map onto
  the `Redirect` enum — add an index to `Enemy`, or lower `enemy(n)`→`EnemyNear` at parse time.
  Resolve when wiring redirections in 4.4/4.6. *(added 4.3 iter)*
- **CB9** Clsn box corner ordering: `air.rs::parse_clsn_entry` normalizes corners via min/max
  (good for AABB overlap, but discards literal ordering). Decide if exact-MUGEN/debug-overlay needs
  the raw rectangle preserved. Currently normalized. *(added 0.3 iter)*
- **CB10** Refresh `CLAUDE.md` + root `README.md` crate-status tables once Phase 4 lands — `fp-vm` is
  no longer a stub (lexer+parser+eval), `fp-formats` now does CNS + SFFv1. Keeper task. *(added 4.3 iter)*
- **CB11** `docs/format-specs/sff-v2.md` had the wrong header offsets (caused the SFFv2 zero-sprite
  bug). *(fixed)*
- **CB12** `fp-vm` 4.8 nit: redirect-id saturation in `scan_redirect_id` (parser.rs) should
  `tracing::warn!` for parity with `saturate_i64_to_i32`. *(added 4.8 iter)*
- **CB13** `snd.rs` `recovers_partial_on_truncated_entry` test rewires the chain via a fragile
  linear-scan for `old_len`; rebuild the truncated fixture deterministically. *(added S8.1 iter)*
- **CB14** `cns_integration.rs` known-unsupported guard uses substring matching; tighten to
  token-aware/anchored matching so a real parser regression can't slip past. *(added 4.6 iter, minor)*
- **CB15** `push.rs` `resolve_push` doctest opens with a dangling/self-contradicting comment; clean
  it up. *(added P6.2 iter, cosmetic)*
- **CB16** `loader.rs` CNS merge order: confirm `cns` vs `st*` precedence on a same-state-number
  collision against the engine ref; add a synthetic test pinning the winner; document the order.
  *(added 5.2 iter)*
- **CB17** `loader.rs` `load_constants` re-reads/re-parses the cns file (once as CNS, once as DEF) —
  dedup the I/O on the load path. *(added 5.2 iter, perf)*
- **CB18** `loader.rs` `load_optional` can't distinguish "file missing" from "file present but
  corrupt" for cmd/snd; differentiate in the warn message. *(added 5.2 iter, minor)*
- **CB19** `executor.rs` StateTypeSet reads the bare token from `expr.source` (not eval'd) — add a
  test pinning that an expression-valued statetype param is a safe no-op. *(added 5.4 iter)*
- **CB20** `executor.rs` VarRangeSet doc attributes the whole-bank default to "MUGEN behavior" — it's
  the engine's safe-default; reword for doc honesty. *(added 5.4 iter)*
- **CB21** `executor.rs` `ignorehitpause` is compiled+stored but not read in dispatch (deferred to
  hitpause); add a visible reference/note at the dispatch site. *(added 5.4 iter)*
- **CB22** fp-app `drop_unevaluable_alive_controllers` matches `alive` by substring (`contains`);
  use a word-boundary/token check. *(superseded once 5.6 adds the `alive` trigger)*
- **CB23** fp-app `inject_default_movement` walk-bridge detection compares ChangeState value source
  to `"20"` literally; would mis-skip on `20.0`/`16+4`. *(superseded once 5.6 removes the injection)*
- **CB24** *(done in 5.6c)* fp-app headless walk test strengthened to strictly-advancing in state 20.
- **CB25** **Stand↔walk (and crouch/jump) command→state transitions are MUGEN engine built-ins**
  absent from KFM data — fp-app currently injects them via a minimal shim. Move these built-in common
  command-state transitions into the `fp-character` executor (special-state handling) so stock chars
  move without an app shim. *(added 5.6c — investigate vs Ikemen; likely a Phase-5/common-states item)*
- **CB26** ✅ *(done in 6.2c)* removed the fp-app walk-velocity repair; facing-relative velocity now
  lives in the executor (`pos.x += vel.x * facing`).
- **CB27** *(folded into 6.2b)* `executor.rs` HitDef-controller doc lists `air.type` among parsed
  params, but it isn't read (no `air_type` field; MUGEN defaults air.type→ground.type). Fix the doc.
- **CB28** `fp-combat` HitDef models only `hittimes` (ground/air/guard); MUGEN's `ground.slidetime`,
  `guard.slidetime`, `guard.ctrltime`, `airguard.ctrltime` are independent — `resolve_hit` currently
  approximates slidetime/ctrltime as hittime. Add the distinct fields (+ controller parse) for
  faithful hitstun/blockstun frame data. *(added 6.3a — refinement, not blocking first combat)*
- **CB29** `fp-character` `resolve_attack`/`change_state`/`tick_with` take a bare
  `HashMap<i32, CompiledState>` in their public signatures; introduce a re-exported `StateGraph` type
  alias so the API reads consistently and the representation can change without rippling. *(added 6.3b)*
- **CB30** Executor hit-pause currently runs NO controllers during the pause; MUGEN also runs
  `ignorehitpause`-flagged controllers mid-pause. Implement that exception (doc now states it's
  deferred). *(added 6.3b — benign until a get-hit state needs it)*
- **CB31** `fp-engine` declares `tracing` but never logs; wire `tracing::debug!` on round-state
  transitions (KO/time-over/round start) or drop the dep. *(added 7.1)*
- **CB32** fp-app samples the keyboard INSIDE the fixed-timestep catch-up loop, so a multi-tick
  catch-up replays one physical input N times (distorts press-vs-hold/command timing on hitches).
  Sample once per real frame. *(added 7.2)*
- **CB33** fp-app `draw_player` hardcodes `ground_y = win_h*0.8` (legacy viewer uses 0.7), divorced
  from the world→screen mapping; derive a named `GROUND_Y_FRACTION` from the coord system. *(added 7.2)*
- **CB34** Jump faithfulness: now that jump is reachable (7.3), verify state 40 (jumpstart) holds for
  its prejump frames before chaining to 50 (airborne) rather than resolving in one tick — i.e. the
  prejump pause is preserved against real common1 timing. Also implement **airjump** (state 45 on
  holdup-while-airborne) which 7.3 deferred. *(added 7.3, Critic CONSIDER #5)*
- **CB35** `CommandMatcher::active_command_names_in` allocates a `Vec` per player per tick on the hot
  path; offer an iterator-returning variant if profiling ever flags it. Low priority. *(added 7.3)*
- **CB36** Looping sound playback: `SoundRequest.looping` is parsed (8.3a) and carried but ignored at
  playback (8.3b) — `AudioSystem::play_sound` is one-shot. Add a looping play path (rodio
  `repeat_infinite`) + a way to stop looped channels. *(added 8.3b)*
- **CB37** Common/fight sound file: PlaySnd `F`-prefixed groups (`common=true`) currently fall back to
  the character's own SND (8.3b). Load the engine common SND (`data/common.snd`/fight motif) once and
  route `common=true` requests to it. *(added 8.3b)*
- **CB38** fp-storyboard BG-group layer matching re-scans all sections per group (O(n*groups)) and couples grouping to naming; a layer could attach to two overlapping-prefix groups. Refactor to assign each "[<name> <layer>]" to its single nearest-preceding "[<name>Def]" in file order. *(added 11.1, Critic SHOULD_FIX)*
- **CB39** Pin the `persistent = n` + `ignorehitpause` interaction during frozen ticks: a flagged controller fires every nth qualifying tick, and frozen ticks DO count (plausibly MUGEN-correct but untested). Add a test locking the chosen semantics. *(added 6.5, Critic SHOULD_FIX)*
- **CB40** Get-hit state ENTRY exprs lose the opponent: resolve_attack -> change_state uses EvalEnv::self_only(), so a [Statedef 5xxx] header that references p2dist/enemy reads 0. Harmless for stock common1/KFM (static headers); thread the attacker as the defender opponent into the get-hit enter_state (a `change_state_vs`). *(added by audit-keystone Critic; 3-lens)*
- **CB41** `enemynear(n)` with n>0 silently resolves to the single opponent (1-v-1 model); add a debug log/note so a future multi-enemy impl does not inherit a wrong-target bug. *(audit-keystone nit)*
- **CB42** AnimElemTime(n): elements AFTER a hold-forever frame (ticks<=0) inherit the hold frame offset and read non-negative ("reached") instead of negative ("never reached"). Sentinelize offsets strictly after the first hold-forever frame. Low impact (post-hold elements are malformed authoring). *(added A.P6, Critic SHOULD_FIX)*

### ✅ Locomotion-shim debt — RESOLVED by 7.3
~~fp-app carried engine-gap shims (`inject_engine_movement_bridge`, `inject_walk_velocity_bridge`,
`fwd/back` vs `holdfwd/holdback`).~~ **Done (7.3, commit pending):** (A) each `fp_engine` `Player`
runs its OWN `CommandMatcher` from the loaded `.cmd` (real `holdfwd`/`FF`/specials — `fwd`/`back`
invention deleted); (B) MUGEN's built-in ground locomotion (stand↔walk↔crouch↔jumpstart — empirically
confirmed absent from ALL KFM data, a hardcoded engine built-in) is now injected by `fp-character`'s
loader as canonical `[Statedef -1]` controllers appended after the character's own, with first-
matching-`ChangeState`-wins priority (executor fix); (C) all three fp-app shims deleted. Stock KFM
walks/crouches/jumps/stops with ZERO app shims, both single-char and two-player paths. **This also
closes CB25** (locomotion built-ins) and the input-pipeline half of the old debt.

---

## ⭐⭐ Ledger update — the 23-PR audit-P run (2026-06-14)

This run drove the [08 faithfulness audit](08-faithfulness-audit.md) 39-item roadmap top-down via the
build loop, landing the bulk of it across **23 PRs (`#5`–`#23`+ the keystone/follow-ups)** merged to
`main` on top of `7e5021d`. The audit-`PXX` task ids map 1:1 onto the audit's item numbers (`P25` =
audit #25, etc.). **Result: the audit is effectively done except #38, #39, and the net-new M5 moat.**

### What landed (by PR / audit item)

| Task | PR | Status | What merged |
|------|----|--------|-------------|
| **P25 / P35** | #5 | DONE | SFF v1 trailing-palette extraction + SFF v2 PNG decode (`png` crate; Png8 indices+PLTE, Png24/32 RGBA via `decode_sprite_rgba`). |
| **P28** | #6 | DONE | RNG-in-state: `Character` Park-Miller seed *in state*; the `random` trigger returns a real `[0,999]`. |
| **P26 / P27** | #7 | DONE | Power-bar HUD (`Player::power()`/`power_max()` + blue bar) + keyboard sampled once per real frame. |
| **P13 / P10 / P23 / P9b / P18** | #8 | DONE | Executor dispatch arms: `AssertSpecial` (NoWalk/NoAutoTurn/Intro per-tick flags), `Width` (per-state push override), `HitVelSet`/`HitFallSet`/`HitFallVel`/`HitFallDamage` (+ HitDef `fall.damage`/`fall.xvelocity`), `HitOverride` (8-slot), and `getpower`/`givepower` on-hit power. |
| **P34** | #9 | DONE | Clsn debug overlay (F1 toggle; `fp-render` debug-box primitive). |
| **P37** | #10 | DONE | `fp-vm` proptest property/fuzz harness (lexer/parser/evaluator). |
| **P21** | #11 | DONE | `RoundState`/`GameTime`/`MatchOver` triggers threaded via `RoundView` (previously pinned 0). |
| **P30** | #12 | PARTIAL | FNT v1 parser + `fp-render` `draw_text`/glyph path. *Asset-blocked* (no real `.fnt` fixture); consumed by the screenpack, not the legacy quad HUD. |
| **P29** | #13 | PARTIAL | Stage `.def` parser + parallax background render — **`fp-stage` graduates from stub**. *Caveat:* tile/velocity/mask/`type=anim` + camera vertical-follow parsed-not-rendered; no real stage fixture. |
| **P20** | #14 | DONE | Priority/trade clash resolution (`fp-combat resolve_clash` + an `fp-engine` reconciled pass) for simultaneous hits. |
| **P16** | #15 | DONE | Dropped Statedef headers (sprpriority/juggle/facep2/hitdefpersist/movehitpersist) + `SprPriority` controller + sprite draw-order + air-juggle. |
| **P39a** | #16 | PARTIAL | `.act` palette **parser** + extended AIR `scale`/`angle`/`Interpolate` parsing. *Parser only* — `.act` runtime + team/turns/tag modes (#39) remain open. |
| **P24** | #17 | DONE | `Pause`/`SuperPause` global match freeze (`fp-engine` freeze timer; trigger clock keeps ticking, `GameTime` held). |
| **P36** | #18 | DONE | Character validator CLI (`fp-app -- validate`, found 23 real KFM problems) + a real CI gate + a SHIPPED ORIGINAL clean-room `assets/trainingdummy/` character (CI loads + matches + validates it). **Closes the #36 CI no-op gap.** |
| **#19** | #19 | DONE | `fp-vm` arith funnels NaN/non-finite to `Bottom` (no public NaN) + proptest doc fixes. **The open fp-vm-NaN follow-ups are now closed.** |
| **P33** | #20 | PARTIAL | `PalFX`/`AfterImage`/`AfterImageTime` controllers + a PalFX color-tint render uniform (`palette.wgsl`) + `fp-app` draw wiring. *Caveat:* the afterimage trail is a motion-smear approximation (no true frame-history ghost ring); `sinadd`/`TimeGap`/`FrameGap`/`Trans`/`PalBright`/`PalContrast` not modeled. |
| **P31** | #21 | PARTIAL | `fight.def` screenpack: typed model+parser + `ScreenpackHud` renderer + `fp-app` load/fallback — **`fp-ui` graduates from stub**. *Caveat:* `[Combo]`/`[Face]` parsed-not-drawn; single `bg0` layer; no `fight.def`/`fight.sff` fixture; falls back to the quad HUD when absent. |
| **P17** | #22 | PARTIAL | Hit-spark **effect-entity infrastructure** (`fp-engine` effect list: spawn-on-hit / tick / expire; `fp-app` render; own-spark path works + tested). *Caveat:* KFM renders NO visible spark — it authors common-`fightfx` `sparkno` and **no `fightfx.sff` loader exists yet**; the `S`-prefix own-spark form is flattened upstream (`parse_resource_id` strips `S`→positive id). |
| **P32** | #23 | PARTIAL | Intro/ending storyboard playback: `fp-storyboard` graduates from parser-only (adds a `StoryboardPlayer`) + `fp-app` overlay during Intro/ending. *Caveat:* scene fadein/fadeout + per-scene clearcolor + BGM not applied; the intro fixed-60-frame timer is not tied to storyboard length. |

(The cross-entity-eval keystone + redirects/P2Dist — audit #1/#2 — and the earlier ✅DONE items #3–#8,
#11, #12, #14, #15, #19(damage-mult), #22 landed in prior runs; see the audit-driven section above and
[08](08-faithfulness-audit.md) for the per-item status column.)

### Structural facts now true
- **`fp-stage` and `fp-ui` are NO LONGER stubs**; **`fp-storyboard` is no longer parser-only** (it has
  a `StoryboardPlayer`).
- The executor dispatch chain now handles **~40 controllers** (was ~30): added `AssertSpecial`,
  `Width`, `HitVelSet`/`HitFallSet`/`HitFallVel`/`HitFallDamage`, `HitOverride`, `SprPriority`,
  `Pause`, `SuperPause`, `PalFX`, `AfterImage`, `AfterImageTime`.
- Workspace tests now **~2,045** (was ~1,769).
- **CI loads + matches + validates the shipped original `assets/trainingdummy` character** — the #36
  green-no-op gap is fixed; the clean-room contract was updated (in `CLAUDE.md`) to permit this
  original content.

### Remaining forward-looking work (NOT done)
- **#38** — replay / determinism / rollback + whole-`Match` state serialization (no `serde`/`bincode`
  whole-state path yet). The next big lever; **unblocked** now that #28 RNG-in-state is in.
- **#39** — team / turns / tag modes + `.act` palette *runtime* consumption (the #39a parser side is
  done).
- **M5 moat** (not in the 39-item audit) — hot-reload, authoring tooling, packaging, and rounding out
  the original default content into a visually complete out-of-box experience.
- The honest render-fidelity **Partials** above (stage tile/velocity/anim layers + camera v-follow;
  screenpack `[Combo]`/`[Face]` + multi-layer bg; true AfterImage ghost ring; common-`fightfx` spark
  loader; storyboard fades/clearcolor/BGM; FNT/screenpack/stage real fixtures) — scheduled
  opportunistically as content and fixtures demand.

---

_Ledger initialized 2026-06-13. Update statuses in place as the loop runs._
