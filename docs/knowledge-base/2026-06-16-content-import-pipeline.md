# Content Import / Preprocessing Pipeline — Design (2026-06-16)

> **Status:** design only, no code. Research + proposed-task spec for a future
> executing agent. READ-ONLY investigation; nothing in the engine was changed to
> produce this doc.
>
> **Premise.** Today we run as-authored community MUGEN content (evilken, plus the
> ~20 characters under `test-assets/community/`) directly through the live load
> path. That produces a flood of load-time warnings and a handful of runtime
> surprises. This doc evaluates adding an **offline content-import / preprocessing
> step** that ingests as-authored content and emits a **normalized "imported"
> form** so the engine runs cleaner content with far fewer warnings and fewer
> runtime edge cases.

---

## 1. Executive summary

- **Recommended imported form:** a **layered** design — (1) a per-source-file
  **repaired-text overlay** plus an **import report** emitted by an extended
  `validate`/new `fp-import` CLI (the authoritative, human-auditable artifact),
  and (2) a **derived, local-only IR cache** (bincode of a new serializable
  `LoadedCharacter`/`SffFile`) that skips re-parse+re-compile on subsequent loads.
  The overlay is the *normalization*; the IR cache is the *speed/robustness*
  payoff. Do the overlay + report first (highest leverage, lowest risk); the IR
  cache is a follow-on.
- **Top 3 highest-leverage repairs** (by warning volume × ease × safety):
  1. **Strip / comment out stray non-`key=value`, non-section "prose" lines** in
     CNS (`Special cancelling`, `t`, `Rage Burst mode Power minus`, `//[State …]`)
     — the single largest warning source in `/tmp/evilken.log`.
  2. **Salvage empty / truncated expressions** (`""` → `0` or drop the controller
     param; `"M-"` → flag) — second largest source; today silently becomes
     const-0 and is invisible at runtime.
  3. **Drop / placeholder zero-dimension and dangling-reference sprites** at import
     so the renderer never logs `Sprite (g,i) has zero dimensions` and AIR frames
     that point at absent sprites are flagged once, offline.
- **Division of labor vs in-flight runtime work:** the F018 community-robustness
  family (Shift-JIS decode `T034`, SFF v2 sub-header resilience `T037`, helper
  lifecycle `T032`, Explod `T033`, `FU/BU` tokens `T035`, `:=` `T036`) and the
  ongoing colon-header / `.def`-encoding fixes (PR #94, current `cns.rs`
  `split_number_label`) make the **engine tolerant at runtime**. Import is
  **complementary**: it reuses those same tolerant parsers, then *records what was
  repaired* and *persists a clean form* — it does not re-implement tolerance.
- **Clean-room:** the IR cache and any rewritten bundle are **derived, local-only,
  never committed or shipped** — same posture as the gitignored `test-assets/`
  symlink. The cache lives under a gitignored dir keyed by a source-content hash.

---

## 2. The current load path (what already happens)

`fp_character::loader::LoadedCharacter::load` (`crates/fp-character/src/loader.rs`)
drives the whole thing:

1. Parse `.def` (`fp_formats::def::DefFile::load`) → read `[Info]`/`[Files]`.
2. Load `SffFile`, `AirFile`, each `CnsFile`, the `CmdFile`, the `SndFile`, and
   external `.act` palettes — every path resolved relative to the `.def` dir.
3. **Merge** CNS state files in MUGEN order (earlier wins; `stcommon` last). The
   `.cmd` is parsed as CNS too and its `[Statedef -1]` controllers appended.
4. Re-read constants from `[Data]`/`[Size]`/`[Velocity]`/`[Movement]`.
5. **Compile** every trigger and controller param to an `fp_vm::Expr` AST
   (`CompiledExpr::compile`, loader.rs:93; `CompiledParam::compile`, loader.rs:151).
   A parse failure → `Expr::Int(0)` fallback + `is_fallback = true` + a
   `tracing::warn!` (loader.rs:101).

Two things to anchor on:

- **Decoding never fails.** `fp_formats::text::decode_text_bytes` (text.rs:49)
  tries UTF-8, then transcodes Shift-JIS, substituting U+FFFD — and **never**
  returns `Err`. `read_text_file` only errors on actual I/O (text.rs:89). So
  encoding is *not* a load-blocker today; it is a warning + a possibly-garbled
  comment/label.
- **The compiled state graph is the natural normalized IR.** Loading already
  produces `LoadedCharacter { states: HashMap<i32, CompiledState>, sff, air, cmd,
  snd, palettes, constants }`. The compiled `CompiledExpr` even carries
  `is_fallback` and the raw `source` — i.e. the loader **already records each
  repair** in-memory; it just discards that record after a warn.

### What the runtime parsers already recover from (so import must *not* duplicate)

| Format | Recovery (file:line) | Behavior |
|--------|----------------------|----------|
| text  | non-UTF-8 → Shift-JIS, then U+FFFD (`text.rs:49-71`) | never fails |
| text  | strip UTF-8 BOM (`text.rs:35`) | silent |
| CNS   | malformed section header → skip (`cns.rs:259-261`) | warn+skip |
| CNS   | `[State N: label]` colon separator now accepted (`cns.rs:419,437,461 split_number_label`) | **fixed** — colon = comma |
| CNS   | line with no `=` → skip (`cns.rs:271-273`) | warn+skip |
| CNS   | empty key → skip (`cns.rs:277-279`) | warn+skip |
| CNS   | `[State …]` before any `[Statedef]` → drop (`cns.rs:237-244`) | warn+skip |
| CNS   | `[State …]` with no `type` → keep as untyped (`cns.rs:365-370`) | warn+keep |
| CMD   | `[Command]` missing `name` → drop (`cmd.rs:169-173`) | warn+drop |
| CMD   | bad `command.time`/`buffer.time` → default (`cmd.rs:196-207`) | warn+default |
| DEF   | `key=val` outside any section → ignore (`def.rs:76-88`) | silent |
| AIR   | `[Begin Action N, label]` → leading int (`air.rs:354-356`) | silent |
| AIR   | frame column trailing junk `2..A`→`2` (`air.rs:508-539`) | warn+leading int |
| AIR   | `Clsn…` stray chars (`Clsn2Defaultf:`) tolerated (`air.rs:361-376`) | silent |
| AIR   | `< 4` Clsn coords → drop (`air.rs:412-414`) | warn+drop |
| AIR   | `< 5` frame columns → drop frame (`air.rs:431-432`) | silent |
| AIR   | **no actions at all → hard error** (`air.rs:252-254`) | `Err` |
| SND   | sub-header / payload past EOF → stop walk, keep prior (`snd.rs:289-335`) | warn+partial |
| SND   | bad signature / too small → hard error (`snd.rs:203-219`) | `Err` |
| SFF v1| sub-header offset out of range → stop walk (`v1.rs:129-130`) | warn+partial |
| SFF v1| missing trailing palette → reuse prior (`v1.rs:281-310`) | debug+reuse |
| SFF v2| sprite block past EOF / truncated → placeholders (`sff/mod.rs:172-188`) | warn+placeholder |
| SFF v2| LData/TData past EOF → truncate (`sff/mod.rs:214-242`) | warn+truncate |
| SFF v2| RLE/LZ5 short decode → zero-pad (`compression.rs:56-325`) | warn+pad |
| SFF   | oversize alloc (RLE5 / PNG pixel count) → recoverable err (`compression.rs:100,392`) | err (per-sprite) |
| SFF   | unknown version byte → error (`sff/mod.rs:582-606`) | hard error |

**Conclusion:** the parsers are *already very tolerant*. The value of an import
step is **not** more tolerance — it is (a) **persisting** the cleaned result so the
warnings stop recurring on every load, (b) **surfacing** the repairs to the author
in one report instead of a log flood, and (c) **fixing the residue** the tolerant
parsers can only *skip* (the stray prose lines), so even the warning disappears.

---

## 3. Issue taxonomy: import-fixable vs runtime-only

Grouped from `/tmp/evilken.log` + the parser inventory. For each: can an **offline
import** repair it, or must it stay a **runtime** behavior?

### A. Parser-recoverable text issues — **import-fixable (normalize the source)**
- Stray prose lines inside a `[Statedef]`: `Special cancelling` (evilken.cns:4765,
  4941, 5030), `t` (24034), `Rage Burst mode Power minus`, accidental
  `//[State 80050,1]`. The parser skips+warns; **import should comment them out**
  (`; Special cancelling`) so they round-trip but no longer warn.
- `[State N: Name]` colon headers — **already fixed at runtime** (`split_number_label`).
  Import only needs to *rewrite* `:`→`,` in the emitted bundle for downstream-tool
  compatibility; it is not a correctness gap any more.
- AIR `2..A`-style trailing junk → `2`: import rewrites the column to its salvaged
  value and records the original.

### B. Expression-compile failures — **import-fixable to *flag*, partial to *repair*** 
- `""` (empty expression) → const-0: extremely common (10+ in evilken). Import
  classifies: an empty *trigger* should be **dropped** (a trigger that is `0` never
  fires anyway — dropping it is semantically identical and removes the warn); an
  empty *param* on a controller that requires it is a **hard flag** (the controller
  is broken — report it, do not invent a value).
- `"M-"` (truncated) → unparseable: **flag only**. Import must not guess `M-` meant
  `M-1` or `Move…`. Surface it in the report with file/line; leave the const-0
  fallback intact.
- Genuinely-valid multi-value params (`damage = 20, 5`) are **not** failures —
  `CompiledParam` already splits top-level commas (loader.rs:151). Import must use
  the same splitter so it does not mis-flag them.

### C. Encoding — **runtime-handled; import *normalizes* opportunistically**
- Shift-JIS / non-UTF-8 is fully handled at runtime (`text.rs:49`, T034). Import
  does **not** need to fix loadability. It *may* transcode the emitted overlay to
  UTF-8 once, so the cleaned bundle is editor-friendly and the per-load
  "decoded as Shift-JIS" warn disappears. This is a convenience, not a correctness
  fix — and it must preserve U+FFFD-free round-trips where possible.

### D. Sprite / SFF binary issues — **mostly runtime; import *flags* + can *prune*** 
- SFF sub-header out-of-range, truncation, zero-pad: runtime-tolerant
  (T037, `sff/mod.rs`, `compression.rs`). Import should **not** rewrite the binary
  SFF (risky, and SFF is the one asset we must never re-author into a derived
  artifact lightly). Instead it **reports** which sprites came back as placeholders.
- Zero-dimension sprites (`Sprite (40,0) has zero dimensions`, main.rs:436): these
  are detected at *draw* time, not parse time. Import can **pre-detect** them
  (a parsed `SffFile` sprite with `width==0||height==0` that is *not* a deliberate
  linked sprite) and (a) report them, and (b) in the IR cache, mark AIR frames
  referencing them so the renderer skips silently with no warn.
- AIR-frame→missing-sprite references: the existing `validate` already detects
  these (`validate.rs:337 check_missing_sprites`). Import folds that check into the
  report and can optionally drop the dead frames from the emitted overlay.

### E. Semantic / AI quirks — **runtime-only; import *annotates*, never *edits*** 
- `Var(30)` self-AI latch: many community chars use a `sysvar`/`var(30)` as an
  internal "AI is active" or "round-start once" latch (cf. engine's own use at
  `fp-engine/src/lib.rs:9894-9919`). Import **must not** rewrite gameplay variables
  — that changes character behavior. It may **annotate** the report ("character
  uses `var(30)` as an AI/latch sysvar; may behave unexpectedly under our baseline
  AI") so a human knows, but the repair is engine-side (AI handling), not import.
- Unsupported controllers (`validate.rs:159 SUPPORTED_CONTROLLERS`): runtime
  no-ops. Import reports the tally; the fix is implementing the controller, not
  preprocessing.

**Rule of thumb:** import fixes **syntax/representation** (categories A, B-flag,
C-normalize, D-prune-references); the **engine** owns **semantics** (categories
B-repair-of-required-params, E). Import is allowed to *delete* things that are
provably inert (an empty trigger, a dead frame) but never to *invent* values.

---

## 4. What is the "imported form"? — options & recommendation

### Option (a) — Normalized on-disk content bundle (rewritten clean `.def`/`.cns`/…)
Re-emit each text asset with repairs applied (stray lines commented, colons→commas,
junk columns salvaged), to a parallel dir.
- **+** Human-readable; the author can diff and adopt fixes upstream; tool-portable.
- **+** No new serialization burden — it is just MUGEN text.
- **−** Re-emitting CNS/AIR faithfully is a *lossy* round-trip risk (comment
  placement, whitespace, ordering). Binary SFF cannot be safely rewritten.
- **−** Clean-room: a rewritten `.cns` of a third-party char is still third-party
  content; must be local-only, never committed.

### Option (b) — Compiled / serialized IR cache (parsed+compiled state graph)
Serialize the `LoadedCharacter` (states, AIR table, CMD, SFF index, constants) via
the existing `serde`+`bincode` stack to a local cache, keyed by source hash.
- **+** Skips re-parse **and** re-compile on every load → fast, and the warnings
  *never re-emit* because the warn happens during compile, which is now cached.
- **+** Deterministic encoding already proven for the snapshot path
  (`fp-engine/src/snapshot.rs`, `bincode`, sorted maps for determinism).
- **−** `LoadedCharacter`/`CompiledState`/`CompiledExpr`/`SffFile` are **not**
  `Serialize` today (only the *runtime* snapshot types are — see §6). Adding the
  derives + a cache-format version stamp is real work.
- **−** Cache invalidation must be airtight (source hash + parser version).

### Option (c) — In-memory normalize-on-load pass
A normalization pass between parse and compile, applied every load; no on-disk
artifact.
- **+** Simplest; no cache, no clean-room artifact question.
- **−** Pays the cost every load; produces no author-facing report; does not reduce
  warnings unless it also suppresses them (which hides real problems).
- This is essentially "more runtime tolerance" — the thing F018 is already doing.
  Low marginal value over the status quo.

### Option (d) — `fp-import` / extended `validate` CLI emitting a repaired bundle + report
A dedicated offline command: `fp-app import <char.def>` (or
`validate --fix --out <dir>`) that produces (a)'s overlay **and** a structured
import report, and optionally warms (b)'s IR cache.
- **+** Author-facing, explicit, opt-in, auditable; no behavior change at runtime
  unless the user points the engine at the imported dir.
- **+** Reuses the entire `validate.rs` analysis machinery (it already walks the
  compiled graph and finds fallbacks, missing sprites, unresolved refs).

### Recommendation — **layer (d) over (a), with (b) as a follow-on; never (c) alone**

1. **Phase 1 (highest leverage):** Build the import CLI (d) that runs the existing
   tolerant load, captures every repair (the loader *already* knows them — see §3),
   and emits:
   - a **repaired text overlay** (a) for the *text* assets only (CNS/CMD/AIR/DEF) —
     stray lines commented, junk columns salvaged, dead frames optionally pruned,
     colons normalized; **SFF/SND left byte-identical** (only referenced/reported);
   - a machine- and human-readable **import report** (§7).
2. **Phase 2 (speed/robustness payoff):** Add `Serialize`/`Deserialize` to the
   static load types and a hash-keyed **IR cache** (b). On load, if a fresh cache
   exists, deserialize it instead of re-parsing → the warn flood never recurs and
   cold-start is faster.

The overlay is the **canonical normalized form a human can adopt**; the IR cache is
the **derived performance artifact**. (c) is rejected as a standalone because it
adds per-load cost with no report and no persistence.

---

## 5. Repair heuristics — guides + pseudocode (NOT implementations)

> All heuristics are **conservative**: delete only provably-inert content, never
> invent values, always record the original in the report. Reuse the *existing*
> tolerant parsers as the oracle — do not write a second parser.

### 5.1 Tolerant CNS line classification (stray-prose repair) — category A
The parser already skips a no-`=`, non-`[section]` line (cns.rs:271). Import needs
to *rewrite* it to a comment so it survives but stops warning.

```
for each raw line L in a [Statedef] body:
    t = strip_comments_and_trim(L)          # reuse text.rs comment rules (;, //, #)
    if t.is_empty(): emit L unchanged; continue
    if is_section_header(t):                 # starts '[' ends ']'
        if SectionKind::parse(inner(t)).is_none():
            report(MALFORMED_HEADER, L); emit ("; [unparsed] " + L)   # neutralize
        else: emit L unchanged
        continue
    if t.contains('='):
        (k, v) = split_first_eq(t)
        if k.trim().is_empty(): report(EMPTY_KEY, L); emit ("; " + L); continue
        emit L unchanged; continue
    # no '=', not a section, not a comment  ->  stray prose (the big win)
    report(STRAY_LINE, L)
    emit ("; " + L)                          # comment it out, keep for round-trip
```
- **Key data structure:** `Repair { file, line_no, kind: RepairKind, original: String,
  replacement: Option<String> }`. `RepairKind ∈ {StrayLine, MalformedHeader,
  EmptyKey, EmptyExpr, TruncatedExpr, JunkColumn, ColonHeader, DeadFrame,
  ZeroDimSprite, MissingSpriteRef, PartialSff, PartialSnd, Transcoded, AiVarHint}`.
- **Gotcha:** do NOT comment out a line just because *we* don't understand it if it
  *does* contain `=` — that is a real key the engine may consume. Only the
  no-`=`/non-section/non-comment shape is "stray".
- **Gotcha:** preserve original line endings/indentation in `emit` so the overlay
  diffs minimally against the source.

### 5.2 Salvaging const-0 / empty expressions — category B
Run *after* compile so you reuse `CompiledExpr::is_fallback` + `.source`
(loader.rs:74-110) rather than re-parsing.

```
for each compiled controller C in the merged graph:
    for each trigger expr E in C.triggerall + C.triggers[*].conditions:
        if E.is_fallback:
            if E.source.trim().is_empty():
                # empty trigger == 0 == never fires; dropping is behavior-preserving
                report(EMPTY_EXPR, site=trigger, action=DROP)
                mark trigger for removal in overlay
            else:
                report(TRUNCATED_EXPR, site=trigger, source=E.source, action=FLAG)
    for each param P in C.params:
        for component K in P.components where K.is_fallback:
            if K.source.trim().is_empty() and param_is_optional(C.type, P.name):
                report(EMPTY_EXPR, site=param, action=DROP_PARAM)
            else:
                report(TRUNCATED_EXPR, site=param, source=K.source, action=FLAG)
```
- **Key insight:** the *trigger* case is the safe auto-repair (drop ≡ never-fires);
  the *param* case is mostly flag-only because a controller may *need* the param.
- **Gotcha:** `param_is_optional` must be a small allow-list, NOT a guess. Start
  empty (flag everything) and grow it only for params with a documented default.
- **Gotcha:** never coalesce `value = 20, 5` into one expr — iterate
  `P.components` (already top-level-comma-split).

### 5.3 Encoding detection / transcode — category C
Loadability is already solved; this only normalizes the overlay.

```
bytes = read(path)
if is_valid_utf8(strip_bom(bytes)): copy through unchanged   # no transcode
else:
    (text, had_errors) = SHIFT_JIS.decode(bytes)             # reuse encoding_rs
    report(TRANSCODED, path, lossy=had_errors)
    if had_errors: report a WARNING (some bytes became U+FFFD — author must check)
    write overlay as UTF-8
```
- **Gotcha:** only transcode *text* assets (CNS/CMD/AIR/DEF). Never touch SFF/SND
  bytes.
- **Gotcha:** Shift-JIS is the dominant but not only legacy encoding. If a file is
  not UTF-8 and Shift-JIS yields many U+FFFD, *flag it* rather than silently
  producing garbage; the right fix is the engine's `decode_text_bytes`, not import.

### 5.4 Zero-dimension / dangling-reference sprite handling — category D
Read the parsed `SffFile` + `AirFile`; do not touch binaries.

```
present = { (s.group, s.image) for s in sff.sprites if s.width>0 && s.height>0 }
deliberate_links = { sprites that are intentional linked/copy frames }   # width 0 by design
for s in sff.sprites where s.width==0 || s.height==0:
    if s not in deliberate_links: report(ZERO_DIM_SPRITE, s.group, s.image)
for action A in air.actions, frame F in A.frames:
    if (F.group, F.image) not in present:
        report(MISSING_SPRITE_REF or ZERO_DIM_REF, A.number, frame_idx)
        if overlay_pruning_enabled: mark F for removal   # frame draws nothing anyway
```
- **Reuse:** `validate.rs:337 check_missing_sprites` is exactly this loop —
  factor it out and share.
- **Gotcha:** linked sprites (`v1.rs:166`, `data_length==0`) are *legitimately*
  0×0 — distinguish them from broken sprites via `linked_index`. Do not prune
  references to a linked sprite that resolves to real pixels.

### 5.5 AI / sysvar annotation — category E (annotate only, never edit)
```
for each controller writing var(N)/sysvar(N) where N in KNOWN_AI_LATCH_VARS (e.g. 30..59):
    if used as a guard in many triggers: report(AI_VAR_HINT, var=N, count)
# never rewrite. Output is advisory text only.
```
- **Gotcha:** this is *informational*. `var(30)` is a legal gameplay variable; the
  engine itself uses `var(30)` as a once-latch (`fp-engine/src/lib.rs:9903`).
  Rewriting it would corrupt the character.

---

## 6. Clean-room, caching, determinism, invalidation

### 6.1 Clean-room contract
- The repaired overlay and the IR cache are **derived from third-party content** →
  they are **third-party content** and must be treated exactly like the gitignored
  `test-assets/` symlink: **local-only, never committed, never shipped.**
- The import tool must **refuse to write** its outputs inside the tracked
  `assets/` tree (guard against accidentally normalizing a shipped original into a
  derived artifact). Default output dir is a gitignored cache root.
- The CLI prints the existing `LICENSE_REMINDER` (`validate.rs:550`) on every run.

### 6.2 Where the cache lives + invalidation
- **Location:** a single gitignored root, e.g. `$FP_CACHE_DIR` (default
  `<workspace>/.fp-cache/` or the OS cache dir). One subdir per imported character,
  named by a content hash. Add `.fp-cache/` and any `*.imported/` overlay dir to
  `.gitignore`.
- **Cache key = hash of all source inputs + parser/compiler version.**
  ```
  key = blake3( sorted([ (relpath, sha256(bytes)) for f in def_referenced_files ])
                || PARSER_FORMAT_VERSION || COMPILER_IR_VERSION )
  ```
  Hash the `.def` *and every file it references* (CNS, CMD, AIR, SFF, SND, ACT) so
  editing any input invalidates. Bump `COMPILER_IR_VERSION` whenever
  `CompiledState`/`CompiledExpr`/`SffFile` layout changes — a stale cache from an
  older binary must never deserialize into a newer struct.
- **Validation on read:** if the key matches → deserialize; on *any* bincode error
  or version mismatch → discard and re-import (never trust a stale/corrupt cache).
  Mirror the snapshot path's never-panic posture (`snapshot.rs:292`).

### 6.3 Determinism
- The snapshot path already proves deterministic bincode is achievable: it sorts
  process-randomized `HashMap`s before encoding (`fp-character/src/snapshot.rs:189`
  — `fire_counts`/`proj_events` sorted). The IR cache MUST do the same: serialize
  `LoadedCharacter.states` and any maps in **sorted key order** so two imports of
  identical inputs produce byte-identical caches (and the import report is stable).
- **What is NOT serializable today (the Phase-2 work):** `LoadedCharacter`,
  `CompiledState`, `CompiledController`, `CompiledExpr` (and the `fp_vm::Expr` AST),
  `SffFile`/`Sprite`, `AirFile`, `CmdFile` carry **no** serde derives. Only the
  *runtime* types do (`MatchSnapshot`, `CharacterSnapshot`, `InputBufferSnapshot`,
  `HitDef`, the input/flag enums — see `snapshot.rs` files). Adding
  `Serialize`/`Deserialize` to the static graph + `fp_vm::Expr` is the core of the
  IR-cache task.

---

## 7. Import-report UX

A single run produces **two** report faces from one `ImportReport` struct:

- **Human face** (extend `validate.rs:render_report`): a per-category section with
  counts, e.g.
  ```
  Import report: Evil Ken  (src: test-assets/evilken/)
    states 396  sprites 1208  anims 503  sound yes
  Repairs applied (overlay written to evilken.imported/):
    stray lines commented (4):
      - evilken.cns:4765  "Special cancelling"
      - evilken.cns:24034 "t"
    empty triggers dropped (10):  ...
    AIR junk columns salvaged (1):  action 1234 frame 7  "2..A" -> 2
  Flagged — needs author attention (not auto-fixed) (2):
    - evilken.cns:18920  truncated expression "M-" (kept as const-0)
  Advisory:
    - zero-dimension sprite (40,0) referenced by anim 5031
    - character uses var(30) as a latch/AI sysvar (may interact with baseline AI)
    - unsupported controllers: ParentVarSet (2), DestroySelf (1)
  ```
- **Machine face:** the same data as JSON (`--report-json out.json`) so CI / the
  fakoli-state evidence path can assert "N repairs, 0 flags" on a fixture.
- **Severity tiers:** `repaired` (auto, behavior-preserving) / `flagged` (author
  must decide) / `advisory` (informational, no action). `is_clean()` ≡ zero
  `flagged`. The reminder is that **`repaired` items are also what the author should
  fix upstream** — the report's whole point is to feed fixes back to the source.

---

## 8. Division of labor (explicit, to avoid duplicating in-flight work)

| Concern | Owner | Why |
|---------|-------|-----|
| Load *despite* bad bytes/encoding | **Runtime** (text.rs T034, sff T037, …) | already done; must never block a load |
| Stop *warning* on every load | **Import** (overlay + IR cache) | persist the cleaned form once |
| Repair representation (stray lines, junk cols, colons) | **Import** | offline, auditable, author-facing |
| Drop provably-inert content (empty triggers, dead frames) | **Import** | behavior-preserving normalization |
| Repair *required* missing params | **Neither** (flag only) | inventing values changes behavior |
| Semantics: unsupported controllers, AI vars | **Runtime / engine** | import only annotates |
| Speed up cold load | **Import** (IR cache) | reuse serde/bincode determinism |

Import **calls** the tolerant parsers; it does not replace or fork them. If a parser
gains new tolerance, import inherits it for free.

---

## 9. Proposed ledger tasks

> Feature numbering continues from the current ledger (highest existing feature
> `F021`, highest task `T051` — verified in `.fakoli-state/state.db`). New feature
> `F022`; new tasks `T052`–`T058`. Field shape matches the fakoli-state schema
> (`title`, `priority`, `dependencies`, `acceptance_criteria`, `implementation_notes`,
> `verification {commands, manual_steps, required_evidence}`, `likely_files`).

### Feature F022 — Content Import / Preprocessing Pipeline
**Description:** An offline import step that ingests as-authored MUGEN content,
runs the existing tolerant load, and emits (a) a repaired **text overlay**, (b) a
structured **import report** (human + JSON), and (c) a local-only, hash-keyed
**IR cache** of the compiled state graph so subsequent loads skip re-parse/compile
and the load-time warning flood does not recur. Complementary to the F018
runtime-robustness work (it reuses those parsers, never re-implements tolerance).
Clean-room: all outputs are derived, local-only, never committed or shipped.

---

#### T052 — Import core: `ImportReport` model + repair-collection over the existing load
- **Priority:** high
- **Dependencies:** none
- **Likely files:** `crates/fp-character/src/loader.rs` (expose repair hooks),
  `crates/fp-app/src/import.rs` (new), `crates/fp-app/src/validate.rs` (reuse
  `check_missing_sprites`, `analyze`), `crates/fp-app/src/main.rs` (CLI route).
- **Description:** Introduce an `ImportReport`/`Repair` data model and a function
  that loads a character through the *existing* tolerant path and collects every
  repair the loader/parsers already know about (fallback exprs via
  `CompiledExpr::is_fallback` + `.source`, missing/zero-dim sprites via the
  validate loop, partial SFF/SND, transcoded files). No source rewriting yet —
  this task is the inventory + model + a `report` subcommand that prints it.
- **Acceptance criteria:**
  - A new `ImportReport` struct with `repaired` / `flagged` / `advisory` tiers and a
    `Repair { file, line_no: Option<usize>, kind: RepairKind, original, replacement }`.
  - `fp-app import --report <char.def>` prints the human report and exits 0 even
    with flags (it is a report, not a gate); `--report-json <path>` writes JSON.
  - Loading evilken produces a report listing ≥1 stray-line, ≥1 empty-expr, and the
    zero-dim sprite advisory — asserted by a test against the shipped trainingdummy
    plus a synthetic fixture (NOT against committed third-party content).
  - `cargo clippy -p fp-app -p fp-character --all-targets -- -D warnings` clean.
- **Implementation guides:**
  - **Approach:** thread an optional `&mut Vec<Repair>` (or a `RepairSink` trait)
    through `LoadedCharacter::load` so the existing `tracing::warn!` sites *also*
    push a structured `Repair`. Keep the warns (back-compat) but make them
    capturable. Reuse `validate::analyze` for sprite/state findings.
  - **Pseudocode:** see §5.2 (expr salvage) and §5.4 (sprite checks). Build the
    report by: load → walk compiled graph for `is_fallback` → walk SFF/AIR for
    zero-dim/dangling → read parser-recorded partials.
  - **Data structures:** `enum RepairKind { StrayLine, MalformedHeader, EmptyKey,
    EmptyExpr, TruncatedExpr, JunkColumn, ColonHeader, DeadFrame, ZeroDimSprite,
    MissingSpriteRef, PartialSff, PartialSnd, Transcoded, AiVarHint }`,
    `enum Tier { Repaired, Flagged, Advisory }`.
  - **Gotchas:** do not double-count the `CompiledParam` multi-value split as
    failures (use `.components`); do not block on flags; never write any file in
    this task.
- **Verification:**
  - `commands`: `cargo test -p fp-app import_`, `cargo clippy -p fp-app -p fp-character --all-targets -- -D warnings`
  - `manual_steps`: `cargo run -p fp-app -- import --report test-assets/evilken/evilken.def` (local only) shows the categorized report.
  - `required_evidence`: test output showing a synthetic malformed-CNS fixture yields the expected `RepairKind` tally.

---

#### T053 — CNS/CMD text-overlay repair (stray lines, empty keys, colon headers)
- **Priority:** high
- **Dependencies:** T052
- **Likely files:** `crates/fp-app/src/import.rs`, `crates/fp-formats/src/cns.rs`
  (expose line classification helpers), `crates/fp-formats/src/text.rs`.
- **Description:** Emit a repaired `.cns`/`.cmd` overlay: stray non-`key=value`
  non-section prose lines commented (`; …`), empty-key lines commented, unparsed
  section headers neutralized, colon separators normalized to commas. Byte-for-byte
  unchanged for clean files.
- **Acceptance criteria:**
  - A synthetic CNS fixture containing `Special cancelling`, `t`, an empty-key line,
    and `[State 9999: Foo]` imports to an overlay that re-parses with **zero
    `CNS:` warnings**, and the report lists exactly those N repairs.
  - A clean CNS fixture round-trips **byte-identical** (no spurious diffs).
  - Overlay is written only under the cache/output dir, never under `assets/`.
  - clippy clean for touched crates.
- **Implementation guides:**
  - **Approach:** §5.1 pseudocode. Process line-by-line, preserving line endings and
    indentation; only transform the three safe shapes. Reuse `SectionKind::parse`
    (cns.rs:404) as the header oracle and the comment rules from text/cns.
  - **Data structures:** reuse `Repair`; an `Overlay { path, content: String }`.
  - **Gotchas:** never comment a line that contains `=` (it is a real key);
    preserve the file's original trailing newline; transform colon→comma only in the
    *header*, never inside a value.
- **Verification:**
  - `commands`: `cargo test -p fp-app overlay_cns`, re-parse assertion test.
  - `manual_steps`: import evilken locally; grep the overlay re-parse log for `CNS:` warns → none.
  - `required_evidence`: before/after warn counts on the synthetic fixture.

---

#### T054 — AIR overlay repair + dead-frame/zero-dim pruning (opt-in)
- **Priority:** medium
- **Dependencies:** T052
- **Likely files:** `crates/fp-app/src/import.rs`, `crates/fp-formats/src/air.rs`,
  `crates/fp-formats/src/sff/mod.rs`.
- **Description:** Salvage AIR frame columns with trailing junk in the overlay
  (`2..A`→`2`), and—behind a `--prune` flag—drop AIR frames that reference absent or
  zero-dimension sprites (they draw nothing). Report every change.
- **Acceptance criteria:**
  - A synthetic AIR with a `2..A` column imports to an overlay with the salvaged
    value and a `JunkColumn` repair recorded.
  - With `--prune`, a frame referencing a `(g,i)` absent from the SFF is removed and
    reported as `DeadFrame`; without `--prune`, it is only flagged.
  - Linked/0×0-by-design sprites are NOT treated as dead (distinguished via
    `linked_index`).
  - clippy clean.
- **Implementation guides:**
  - **Approach:** §5.4. Build the `present` set from the parsed `SffFile`
    (excluding deliberate links); reuse `validate::check_missing_sprites`.
  - **Gotchas:** AIR has a hard-error when *all* actions are gone (air.rs:252) —
    pruning must never empty an action's only frame without warning; never prune
    references to a linked sprite that resolves to real pixels.
- **Verification:**
  - `commands`: `cargo test -p fp-app overlay_air`
  - `required_evidence`: test showing salvaged column + pruned-frame count on a fixture.

---

#### T055 — Import report rendering (human + JSON) + severity gate
- **Priority:** medium
- **Dependencies:** T052
- **Likely files:** `crates/fp-app/src/import.rs`, `crates/fp-app/src/validate.rs`
  (share `LICENSE_REMINDER` + render style).
- **Description:** Render `ImportReport` to the human format (§7) and to JSON. Add an
  optional `--strict` mode that exits non-zero when any `Flagged` item exists (for
  CI), while the default stays exit-0.
- **Acceptance criteria:**
  - Human output groups by tier with per-category counts and file:line.
  - `--report-json` emits stable, sorted JSON (deterministic for identical input).
  - `--strict` exits non-zero iff `flagged` is non-empty; default exit 0.
  - Clean content prints `PASS — no repairs needed`.
  - clippy clean.
- **Implementation guides:**
  - **Approach:** extend the `render_report` pattern from `validate.rs:563`. Sort all
    lists by (file, line, kind) for determinism.
  - **Gotchas:** JSON must be stable across runs — sort before serializing (mirror
    `snapshot.rs:189`).
- **Verification:**
  - `commands:` `cargo test -p fp-app import_report`
  - `required_evidence`: JSON snapshot test; `--strict` exit-code test.

---

#### T056 — Serializable static load types (`LoadedCharacter`/`CompiledState`/`Expr`/`SffFile`)
- **Priority:** medium
- **Dependencies:** T052
- **Likely files:** `crates/fp-vm/src/parser.rs` (`Expr`), `crates/fp-character/src/loader.rs`,
  `crates/fp-character/src/lib.rs`, `crates/fp-formats/src/sff/mod.rs`,
  `crates/fp-formats/src/air.rs`, `crates/fp-formats/src/cmd.rs`, root `Cargo.toml`.
- **Description:** Add `serde::{Serialize, Deserialize}` to the static load graph
  (`Expr` AST, `CompiledExpr`, `CompiledParam`, `CompiledController`, `CompiledState`,
  `LoadedCharacter`, `SffFile`/`Sprite`, `AirFile`, `CmdFile`, `CharacterConstants`).
  Enable round-trip via bincode with **deterministic** (sorted-map) encoding. No
  cache file yet — this task is the serialization seam only.
- **Acceptance criteria:**
  - Each listed type derives `Serialize`/`Deserialize`; a unit test round-trips a
    loaded trainingdummy through bincode and asserts the deserialized graph is
    structurally equal.
  - Two serializations of the same `LoadedCharacter` are **byte-identical** (sorted
    maps).
  - `#![warn(missing_docs)]` and clippy `-D warnings` stay clean across all touched
    library crates.
- **Implementation guides:**
  - **Approach:** derive on the AST first (`fp_vm::Expr`), then bottom-up. For
    `HashMap` fields, serialize via a sorted intermediate (or switch to `BTreeMap`
    where it does not regress lookup-heavy hot paths) — see `snapshot.rs:189` for the
    sort-before-encode precedent.
  - **Data structures:** add a top-level `IrCacheHeader { format_version, source_hash }`
    (used by T057) but do not wire the cache file here.
  - **Gotchas:** `SffFile` holds large pixel buffers — confirm bincode handles them
    and the round-trip is lossless; keep index 0 = transparent invariant intact.
    Do not derive on the *runtime* `Character` (already snapshot-serialized) — only
    the static load result.
- **Verification:**
  - `commands`: `cargo test -p fp-vm -p fp-character -p fp-formats serde_roundtrip`,
    `cargo clippy --workspace --all-targets -- -D warnings`
  - `required_evidence`: byte-equality test output for two encodes of one character.

---

#### T057 — Local IR cache: hash-keyed read/write + invalidation
- **Priority:** medium
- **Dependencies:** T056
- **Likely files:** `crates/fp-character/src/loader.rs` (cache check in `load`),
  `crates/fp-character/src/ir_cache.rs` (new), `.gitignore`, root `Cargo.toml`
  (blake3/sha2 dep if not present).
- **Description:** On `LoadedCharacter::load`, compute the source-content hash (all
  `.def`-referenced files + format/compiler version), and if a fresh cache exists
  under the gitignored cache root, deserialize it instead of re-parsing. Otherwise
  load normally and write the cache. Any cache error → silent re-import.
- **Acceptance criteria:**
  - First load of a fixture writes a cache file under `$FP_CACHE_DIR`; second load
    deserializes it (verified by a counter/log or by mutating the cache and observing
    the change reflected).
  - Editing any source input invalidates the cache (hash changes → re-import).
  - A corrupt/old-version cache is discarded without panic; load still succeeds.
  - The cache root is gitignored; the tool refuses to write inside `assets/`.
  - clippy clean.
- **Implementation guides:**
  - **Approach:** §6.2 key formula. `key = blake3(sorted (relpath, sha256(bytes))
    pairs || PARSER_FORMAT_VERSION || COMPILER_IR_VERSION)`. Read path: try cache →
    on `Ok` deserialize + verify header → use; on any error fall through to full load
    + write.
  - **Data structures:** `IrCacheHeader` from T056; cache file path =
    `$FP_CACHE_DIR/<key>.fpir`.
  - **Gotchas:** must be opt-out via env (`FP_NO_CACHE=1`) for debugging; never trust
    a cache whose header version differs; warn-flood suppression is a *side effect*
    of caching the compile, not a separate suppression switch (don't hide warns on
    cache miss).
- **Verification:**
  - `commands`: `cargo test -p fp-character ir_cache`
  - `manual_steps`: load evilken twice locally; second run shows no `bad expression`
    / `CNS:` warns (compile was cached) and is faster.
  - `required_evidence`: cache hit/miss test; invalidation-on-edit test.

---

#### T058 — Engine consumes imported overlays + docs/clean-room guard
- **Priority:** low
- **Dependencies:** T053, T054, T055
- **Likely files:** `crates/fp-app/src/main.rs` (point a load at an `.imported` dir),
  `crates/fp-app/src/import.rs`, `docs/content-guide.md`, `docs/known-issues.md`,
  `.gitignore`.
- **Description:** Let `fp-app` load from an imported overlay dir (so a user can
  adopt the cleaned content), add a clean-room write-guard (refuse outputs under
  tracked `assets/`), and document the import workflow + report format. Print the
  `LICENSE_REMINDER` on every import run.
- **Acceptance criteria:**
  - `fp-app import --out <dir> <char.def>` writes overlay + report there;
    `fp-app <dir>/<char>.imported.def` loads and runs the repaired char.
  - Attempting to write outputs under `assets/` is refused with a clear error.
  - `docs/content-guide.md` documents the import step and the report tiers;
    `docs/known-issues.md` notes overlay is text-only (SFF/SND unmodified).
  - clippy + `cargo fmt --check` clean.
- **Implementation guides:**
  - **Approach:** reuse the existing directory-discovery path (`discover_*` in
    main.rs) to accept an overlay dir. The write-guard canonicalizes the output path
    and rejects any prefix matching the workspace `assets/`.
  - **Gotchas:** never commit overlays or caches — add `*.imported/`, `.fp-cache/` to
    `.gitignore`; the doc must restate the clean-room "local-only, derived" rule.
- **Verification:**
  - `commands`: `cargo test -p fp-app import_guard`, `cargo fmt --all --check`
  - `manual_steps`: import + run a community char from its overlay locally.
  - `required_evidence`: write-guard rejection test; end-to-end import→run on a fixture.

---

## 10. Open questions / risks for the executing agent
- **Overlay round-trip fidelity for AIR/CNS is the main risk.** Keep the overlay a
  *line-level* transform (comment/replace specific lines), never a full re-emit from
  the parsed model, to avoid losing comments/ordering.
- **IR cache size:** `SffFile` pixel buffers dominate; the cache may be large.
  Consider caching only the *compiled text graph* (CNS/CMD/AIR) and keeping SFF as
  the on-disk binary (it is already a binary container) — a "Tier-1 text IR + raw
  SFF" split. Decide in T056/T057.
- **`param_is_optional` allow-list (§5.2):** start empty (flag-only) and grow it from
  the MUGEN controller docs; never guess.
- **Do not let import become a warn-suppressor.** The goal is to *eliminate the
  cause* (persist clean content) and *surface* the rest, not to mute `tracing::warn!`.
