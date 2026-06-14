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
| 6.2b | DOING | **Multi-component param model** (loader) | `fp-character` | MUGEN params are comma-lists (`damage=20,5`, `ground.velocity=-4,0`); the loader compiles each as ONE expr → const-0 + spurious `warn!("bad expression")` at load (values only correct because HitDef re-splits). Store params as comma-split **component lists** of compiled exprs so all controllers read `component(i)` uniformly, no spurious warn, and `ctrl_hit_def`'s manual re-split is removed. + fix the `air.type` doc overclaim (CB27). | 6.2 |
| 6.3 | TODO | **Hit resolution + apply** | `fp-combat`+`fp-character` | Given a `HitContact` + defender: hitflag/guardflag/stance check (block vs hit), damage, hitpause (both sides), get-hit velocities, `p2stateno` state-takeover, fall/juggle accounting. Unit-tested with two synthetic Characters. | 6.2 |

(Live two-player fights on screen — ticking P1+P2, running detection each tick, KO/round — are
**Phase 7** `fp-engine` + fp-app, where the demo becomes an actual match.)

### Phase 7 — `fp-engine` (round flow)  *(expand when reached)*
Move loop out of `fp-app`; P1/P2 coordination; round states (intro→fight→KO→win); timer; win
conditions. Deps: Phase 6.

### Phase 8–11 — stage / audio / ui / storyboard  *(expand when reached)*
`fp-stage`, `fp-audio`+SND, `fp-ui`+FNT (lifebars/select/screenpacks), `fp-storyboard`. Largely
parallelizable once the core exists. Deps: Phase 7.

- **S8.1** ✅ DONE — **SND parser** (`fp-formats/src/snd.rs`): `SndFile{load, from_bytes,
  sound(group,sample)}`, validates `ElecbyteSnd`, walks the directory (count-terminated,
  cycle-guarded), real `kfm.snd` = 12 RIFF sounds. +18 tests. Critic PASS (fragile truncation test → CB13).

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
- **CB6** Enforce MUGEN **trigger-group contiguity** (a gap in `trigger1,2,4…` numbering kills later
  groups) when compiling/evaluating `StateController::triggers` — the CNS parser deliberately keeps
  all groups (documented on the field). Apply at trigger-compile (task 4.6). *(added 4.2/4.5 iter)*
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
- **CB26** *(after 5.6e — now ready)* remove the `inject_engine_movement_bridge` velocity-repair in
  fp-app now that `const(velocity.walk.fwd.x)` resolves natively (5.6e). *(added 5.6c)*
- **CB27** *(folded into 6.2b)* `executor.rs` HitDef-controller doc lists `air.type` among parsed
  params, but it isn't read (no `air_type` field; MUGEN defaults air.type→ground.type). Fix the doc.

---

_Ledger initialized 2026-06-13. Update statuses in place as the loop runs._
