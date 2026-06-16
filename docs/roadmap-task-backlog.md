# PRD Expansion — Vision Roadmap (Fidelity + Experience + Content-Import)

> Appendable PRD block consolidating three planning tracks, deduped and reconciled against the **real
> v1.0+ source (2026-06-16, PRs #44–#99)**. IDs are sequential from the current fakoli-state max
> (feature **F021** / task **T051**): features **F022–F034**, tasks **T052–T089**.
>
> **Reconciled out (already shipped — NOT re-filed):** the `:=` assignment operator (T036),
> determinism + whole-`Match` snapshot/restore, the serde **`ReplayLog`/`MatchRecorder`/`replay_match`**
> file format (#38 core), the **`HitDefAttr`** trigger (Task A), `FrontEdgeDist`/`BackEdgeDist`/
> `ScreenPos`, **common-fightfx hit-spark rendering** (#17 — `fightfx.sff`/`.air` ship + load), the
> **drawn `[Combo]` counter** in the screenpack HUD, the `CommandSource` seam, the AI substrate
> (`AiDifficulty`/`AiTuning`/`CpuAi`), `InputBufferSnapshot`, helpers/projectiles/Explod, team
> Simul/Turns, walk/jump locomotion transitions, RoundState/GameTime/MatchOver, and the GUI-free
> behavioral harness.
>
> **Clean-room is a standing acceptance criterion** on every task that writes files or ships content:
> outputs derived from third-party content are third-party content (local-only, never committed);
> tests run against synthetic / `assets/trainingdummy` fixtures, never committed community content.

---

### F022: AI Identity & Self-AI Safety

Wire a first-class notion of "who drives this player" so the `AILevel` trigger is correct and a
character's self-AI stays dark for human players. Closes the #1 ranked mechanics gap and the evilken
`Var(30)=59` self-AI trap.

**Requirements**
- The live entity carries an `ai_level: u8` (0 = human, 1–8 = engine AI), set once at match start.
- The `AILevel` trigger returns that value; a human evaluates `AILevel = 0` true and `AILevel >= 1`
  false. CPU difficulty maps to a fixed 1–8 level.
- A human player can never satisfy the modern cheap-AI idiom (`triggerall = AILevel`); var banks
  initialize to 0 and nothing seeds a magic value engine-side.
- `ai_level` round-trips through `CharacterSnapshot` (replay/rollback determinism).

#### T052: Add an `ai_level` identity field and plumb it from match wiring
- **Title:** `ai_level` entity field + match/CPU plumbing
- **Priority:** P0 (blocks T053)
- **Dependencies:** none
- **Likely files:** `crates/fp-character/src/lib.rs` (Character field + accessor + snapshot),
  `crates/fp-engine/src/lib.rs` (`Player`/`Match`/team setup), `crates/fp-app/src/main.rs` (P1 human → 0,
  P2 `CpuAi` → mapped), `crates/fp-input/src/ai.rs` (`AiDifficulty::ai_level`).
- **Acceptance criteria:**
  - `Character` gains `ai_level: u8` defaulting to 0; `pub fn ai_level(&self) -> u8`.
  - `Match`/`TeamMatch` set each player's `ai_level` at construction from its driver (keyboard/gamepad → 0;
    `CpuAi` → mapped from `AiDifficulty`). Team members inherit their side's driver.
  - `AiDifficulty::{Easy,Normal,Hard}` map to fixed 1–8 (e.g. 2/4/7) via `fn ai_level(self) -> u8`.
  - `ai_level` is part of `CharacterSnapshot` and survives a bincode round-trip.
  - A bare self-only `Character` (no coordinator) defaults to 0.
- **Implementation GUIDES:**
  - Plain field set once (does not change mid-round) — it belongs on the entity like `life`/`power`,
    **not** in `EvalEnv` (it is per-self, not cross-entity).
  - `fp-app` already chooses P2's input (human vs `CpuAi`); thread the same decision into a
    `set_ai_level` call when building the match.
  - `impl AiDifficulty { fn ai_level(self) -> u8 { match self { Easy=>2, Normal=>4, Hard=>7 } } }`
  - Gotcha: `CharacterSnapshot` already carries the RNG seed as a plain `i32`; add the `u8` alongside,
    sorted-stable so two encodes stay byte-equal.
- **Verification:** `cargo test -p fp-engine ai_level` — build a Match with P2 = `CpuAi(Hard)`, assert
  `p2.character.ai_level()==7` and `p1...==0`; round-trip a `CharacterSnapshot` and assert it survives.
- **Reference:** mechanics-ref §4.4–§4.5 (engine sets `AILevel`, character reads it); closes the
  plumbing half of ranked gap #1.

#### T053: Implement the `AILevel` trigger in the evaluator
- **Title:** `AILevel` read-trigger
- **Priority:** P0
- **Dependencies:** T052
- **Likely files:** `crates/fp-character/src/lib.rs` (trigger dispatch, near the zero-arg `Time`/`Life` arms).
- **Acceptance criteria:**
  - `AILevel` (case-insensitive) returns `Value::Int(self.ai_level as i32)`, takes no args.
  - `ai_level==0` → `AILevel = 0` true, `AILevel != 0` false; `ai_level==5` → returns 5.
  - No other trigger regresses (new arm before the unknown-trigger default).
- **Implementation GUIDES:**
  - `name if name.eq_ignore_ascii_case("AILevel") => Value::Int(self.ai_level as i32),`
  - Gotcha: read-only — the character never assigns it; the engine owns it (T052).
- **Verification:** `cargo test -p fp-character ailevel_trigger` — `ai_level=0` evaluates `AILevel`→0,
  `AILevel != 0`→false; `ai_level=5`→5. Behavioral: a fixture gating a move on `triggerall = AILevel`
  fires for the CPU player and never for a human-input replay.
- **Reference:** mechanics-ref §4.1/§4.5; closes ranked gap #1.

#### T054: Guarantee human players never satisfy a cheap-AI var gate (var-init audit + lock-in test)
- **Title:** Cheap-AI var-init safety audit
- **Priority:** P1
- **Dependencies:** T052
- **Likely files:** `crates/fp-character/src/lib.rs` (var storage init / round reset),
  `crates/fp-engine/src/lib.rs` (round reset path), a synthetic fixture.
- **Acceptance criteria:**
  - At round start all `var(0..59)`/`fvar`/`sysvar`/`sysfvar` default to 0 for a human player; no path
    seeds a magic value engine-side.
  - A fixture whose round-init sets `Var(30)=59` *only behind* `triggerall = AILevel` ends round-init
    with `Var(30)==0` for `ai_level=0` and `==59` for `ai_level=5`.
- **Implementation GUIDES:**
  - This is an **audit + regression test**, not new mechanics — confirm var arrays are zero-initialized
    and round reset zeroes them; the fix surface is "ensure nothing seeds a magic value" + the test.
  - Gotcha: the *legacy* WinMUGEN cheap-AI idiom (no `AILevel`) sets the flag down an input path a human
    cannot reproduce — we cannot fully emulate that, but with `AILevel` wired the *modern* idiom is fully
    safe, which is what matters for the evilken class. Document this boundary in the test comment.
- **Verification:** `cargo test -p fp-character cheap_ai_var_safety` — the two-`ai_level` assertion above.
- **Reference:** mechanics-ref §4.2/§4.3 (evilken trap); closes ranked gap #2.

---

### F023: Engine-Default Common States (locomotion + guard + round flow)

Author a complete clean-room `common1.cns` so a character that defines only its own specials inherits
correct movement, guarding, win/lose, intro, and round-init. Today the shipped file is 5000-series
only; locomotion limps along on a synthesized `[Statedef -1]`. Plus the engine friction stop-floor.

**Requirements**
- `common1.cns` gains the documented engine-common states 0–199 (movement, guard, win/lose, intro) and
  5900 (round-init), authored from scratch (MIT, clean-room — NOT derived from Elecbyte).
- A character that `ChangeState`s directly to 0/20/40/120/5900 lands in a real state body.
- The character's own states always win over the common fallback (first-wins merge).
- Friction snaps `vel.x` to 0 below the per-mode `friction.threshold` for Stand/Crouch only.

#### T055: Author the movement / idle common states (0, 10–12, 20, 40/45/50/52, 100, 105/106)
- **Title:** common1.cns — movement/idle states
- **Priority:** P0
- **Dependencies:** none
- **Likely files:** `assets/data/common1.cns` (extend the original clean-room file),
  `crates/fp-character/src/loader.rs` (fallback test).
- **Acceptance criteria:**
  - States 0/10/11/12/20/40/45/50/52/100/105/106 exist with bodies that read the character's own
    `Const(velocity.*)`/`Const(movement.yaccel)` (data-driven, not hard-coded).
  - State 40 resets the air-jump counter at `time=0`; state 50 lands (`Vel y>0 && Pos y>=0` → 52 → 0);
    state 20 chooses `walk.fwd`/`walk.back` by held direction.
  - A character that `ChangeState`s directly to 0/20/40 (not via `[Statedef -1]`) animates.
  - Header comment asserts independent authorship; no Elecbyte text.
- **Implementation GUIDES:**
  - Mirror the canonical layout (mechanics-ref §1.1) but author every controller from scratch; use
    `Const()` reads so speed stays per-character. Jump (40):
    `[State 40, vel] type=VelSet trigger1=Time=0 x=Const(velocity.jump.neu.x) y=Const(velocity.jump.neu.y)`
    then `ChangeState value=50`.
  - Gotcha 1: the loader already injects `append_builtin_ground_locomotion` (loader.rs:1167) as a
    `[Statedef -1]` synthesizing the *transitions*; authoring real state *bodies* must not double-drive
    movement — guard `ChangeAnim` so it doesn't re-trigger each tick.
  - Gotcha 2: the loader's first-wins `merge_cns` means a character's own state 0 must still win — confirm
    the fallback only applies when the character lacks the state.
- **Verification:** `cargo test -p fp-character common1_movement_states` — minimal fixture with NO movement
  states resolves 0/20/40/50 from fallback and `ChangeState 40` sets jump velocity at `time=0`. Behavioral:
  `cargo run -p fp-app -- test-assets/evilken/evilken.def` walk/jump still animate (regression).
- **Reference:** mechanics-ref §1.1–§1.3.

#### T056: Author the guard, win/lose, intro, and round-init common states (120–155, 170/175, 190/191, 5900)
- **Title:** common1.cns — guard/win/intro/round-init states
- **Priority:** P1
- **Dependencies:** T055
- **Likely files:** `assets/data/common1.cns`, `crates/fp-character/src/loader.rs`,
  `crates/fp-engine/src/lib.rs`.
- **Acceptance criteria:**
  - 120/130/131/132/140 (guard) and 150–155 (guard-hit) exist and read guard-velocity behavior; guarding
    holds the character and consumes the hit per the existing guard path.
  - 170/175 (lose/draw), 190/191 (pre-intro/intro), 5900 (round-init: life/power reset hooks, var defaults).
  - 5900 integrates with the existing `RoundView`/round-reset (converges, does not fight the engine).
- **Implementation GUIDES:**
  - Continue clean-room CNS authoring; guard states are mostly `ChangeAnim` + `VelSet 0` + a `ChangeState`
    back to neutral. 5900:
    `[State 5900,life] type=LifeSet trigger1=Time=0 value=Const(data.life)` / `[...ctrl] CtrlSet ... value=1`
    / `[...to stand] ChangeState ... value=0`.
  - Gotcha: the engine owns authoritative life/round reset; 5900 is advisory — if the engine already resets
    life, 5900's `LifeSet` is a harmless no-op on the same value.
- **Verification:** `cargo test -p fp-character common1_guard_and_round_states` (fixture with no guard/init
  states resolves 120/130/5900 from fallback); `cargo test -p fp-engine round_init_uses_5900`.
- **Reference:** mechanics-ref §1.1.

#### T057: Friction snap-to-zero stop-floor in `apply_physics`
- **Title:** Friction snap-to-zero (Stand/Crouch)
- **Priority:** P1
- **Dependencies:** none (consts already loaded — `stand_friction_threshold`/`crouch_friction_threshold`
  exist at lib.rs:561/563)
- **Likely files:** `crates/fp-character/src/executor.rs` (`apply_physics`).
- **Acceptance criteria:**
  - After the friction multiply, if `abs(vel.x) < threshold` (matching Stand/Crouch threshold), `vel.x`→0.
  - Threshold selected by `Physics` mode; Air/None modes unaffected.
  - A 0 threshold means "never snap" (preserve current behavior as the safe default).
- **Implementation GUIDES:**
  ```
  Physics::Stand => { self.vel.x *= mv.stand_friction;
                      if self.vel.x.abs() < mv.stand_friction_threshold { self.vel.x = 0.0; } }
  Physics::Crouch => { self.vel.x *= mv.crouch_friction;
                       if self.vel.x.abs() < mv.crouch_friction_threshold { self.vel.x = 0.0; } }
  ```
  - Gotcha: do NOT snap in `Physics::Air`.
- **Verification:** `cargo test -p fp-character friction_snaps_to_zero` — `vel.x=1.0`, Stand, threshold 2.0
  → after one tick `vel.x==0.0`; threshold 0.0 → decays only.
- **Reference:** mechanics-ref §1.3/§1.4; known-issues #12; closes ranked gap #4.

---

### F024: Charge Command Motions

Parse `~NN`/`/NN` hold-duration tokens so charge moves (`~60$B, F, x`) compile and fire instead of
erroring the whole command to a const-0 fallback.

**Requirements**
- `~NN`/`/NN` parse to a hold-duration on the modified element (default 0 = no requirement).
- A charge element matches only if the direction was held ≥ N consecutive ticks ending at the
  release/transition, within the buffer; a single shared charge timer (MUGEN's one-charge limitation).
- Non-charge commands (`min_hold==0`) behave exactly as before.

#### T058: Add a hold-duration field to Hold/Release and parse `~NN` / `/NN`
- **Title:** Parse charge `~NN`/`/NN` tokens
- **Priority:** P0
- **Dependencies:** none
- **Likely files:** `crates/fp-input/src/command.rs` (`InputModifier`, `parse_single_token`, the
  command-element struct).
- **Acceptance criteria:**
  - `~60$B` parses to a `Release` element with `min_hold=60` on `$B`; `~D` keeps `min_hold=0`.
  - The element struct carries `min_hold: u16` (0 default everywhere).
  - `command = ~60$B, F, x` compiles (no `unknown command token`).
- **Implementation GUIDES:**
  - When the modifier char (`~`/`/`) is consumed, peek for ASCII digits → parse as `min_hold`, then parse
    the rest as direction/button as today. The bug is that the digits currently glue onto the token
    (`60$B`) and hit the unknown-token error.
  - Gotcha: `~` without digits must still work (existing negative-edge tests must stay green).
- **Verification:** `cargo test -p fp-input parse_charge_token` — `~60$B` → `Release`/`min_hold=60`/`$B`;
  `~D` → `min_hold=0`; `command = ~60$B, F, x` compiles to a non-fallback command.
- **Reference:** mechanics-ref §2.1.

#### T059: Enforce charge hold-duration in the backward-scan matcher
- **Title:** Charge hold-duration enforcement
- **Priority:** P0
- **Dependencies:** T058
- **Likely files:** `crates/fp-input/src/command.rs` (matcher / `dir_hit`/release logic),
  `crates/fp-input/src/buffer.rs` (60-frame ring).
- **Acceptance criteria:**
  - A charge element matches only if the `$`-set direction was held ≥ `min_hold` consecutive ticks ending
    at the release/transition, within the window.
  - `~60$B, F, x` fires iff Back (B/DB/UB via `$`) was held ≥60 ticks, then F, then x within `time`.
  - A single shared charge accumulator (no per-direction charge). `min_hold==0` unchanged.
- **Implementation GUIDES:**
  - At the `~NN$B` step, walk further back from the matched release frame and count consecutive held frames;
    require run length ≥ `min_hold`. `dir_held` must honor `$` (B matches B/DB/UB).
  - Gotcha: the charge window can exceed command `time` — the hold count is measured *before* the release;
    `time` applies to the post-release portion. The 60-frame ring caps `min_hold` at 60 (acceptable —
    document the clamp).
- **Verification:** `cargo test -p fp-input charge_requires_hold` — 60 ticks Back→F→x matches; 59 ticks
  Back fails. `cargo test -p fp-input charge_dollar_directiondetect` — DB counts toward `~$B`.
- **Reference:** mechanics-ref §2.1.

---

### F025: Trigger Coverage Completion

Wire the missing read-triggers community content gates on, from data already in `EvalCtx`/`StageView`/
targets. (`HitDefAttr`, `FrontEdgeDist`, `BackEdgeDist`, `ScreenPos` already ship — excluded.)

**Requirements**
- Stage geometry/edge triggers resolve to real camera/localcoord values, consistent with the existing
  edge-distance convention.
- Target/hit introspection (`NumTarget`/`HitVel`/`HitOverridden`) and team/identity (`TeamSide`/
  `PlayerIDExist`) resolve from existing state, not 0.

#### T060: Stage geometry & screen-edge triggers (`GameWidth`/`GameHeight`/`LeftEdge`/`RightEdge`/`TopEdge`/`BottomEdge`)
- **Title:** Edge & game-dimension triggers
- **Priority:** P1
- **Dependencies:** none
- **Likely files:** `crates/fp-character/src/lib.rs` (trigger dispatch + `EvalCtx`),
  `crates/fp-engine/src/lib.rs`/`crates/fp-stage` (values into `StageView`).
- **Acceptance criteria:**
  - `GameWidth`/`GameHeight` return the localcoord/screen logical dimensions (the 320×240-class units the
    existing `ScreenPos`/edge math uses).
  - `LeftEdge`/`RightEdge`/`TopEdge`/`BottomEdge` return camera-relative world coords of the visible edges,
    consistent with `FrontEdgeDist`/`BackEdgeDist`.
  - A unit test pins concrete values; `RightEdge - LeftEdge == GameWidth` in the camera-relative convention.
- **Implementation GUIDES:**
  - `EvalCtx` already computes front/back edge distances + `ScreenPos`, so camera bounds are known
    internally — surface them. `GameWidth`/`GameHeight` are stage/localcoord constants; thread onto
    `StageView` if absent. Zero-arg arms next to `FrontEdgeDist`.
  - Gotcha: use the same coordinate convention as the existing edge triggers — do not introduce a second
    coordinate space.
- **Verification:** `cargo test -p fp-character edge_and_game_dim_triggers` — known stage+camera → assert
  the four edges and `RightEdge - LeftEdge == GameWidth`.
- **Reference:** mechanics-ref §1 (stage/camera) + trigger reference.

#### T061: Target/hit introspection triggers (`NumTarget`, `HitVel`, `HitOverridden`)
- **Title:** Target/hit introspection triggers
- **Priority:** P1
- **Dependencies:** none (targets list + GetHitVars + HitOverride slots exist)
- **Likely files:** `crates/fp-character/src/lib.rs` (dispatch), `crates/fp-character/src/combat.rs`
  (GetHitVars source), `crates/fp-character/src/executor.rs` (HitOverride slots).
- **Acceptance criteria:**
  - `NumTarget` (optional `NumTarget(id)`) returns the count of currently bound targets.
  - `HitVel X`/`HitVel Y` return the velocity imparted by the most recent hit taken (from GetHitVars).
  - `HitOverridden` returns 1 iff the current get-hit is redirected by an active `HitOverride` slot.
- **Implementation GUIDES:**
  - Each reads existing state: `NumTarget` from the targets list, `HitVel` from the GetHitVars struct,
    `HitOverridden` from the 8-slot HitOverride array. `NumTarget(id)` mirrors existing `NumHelper`/`NumProj`
    arg handling. `HitVel` is per-axis — route through the same axis-arg parsing as `Vel`/`Pos`.
- **Verification:** `cargo test -p fp-character target_and_hit_triggers` — thrower with one bound target →
  `NumTarget==1`; populate GetHitVars → `HitVel X` matches; activate a HitOverride slot → `HitOverridden==1`.
- **Reference:** mechanics-ref §5 (targets/throws) + trigger reference.

#### T062: Team/identity triggers (`TeamSide`, `PlayerIDExist`)
- **Title:** Team/identity triggers
- **Priority:** P2
- **Dependencies:** none (team Simul/Turns already exist; redirect playerid id-space exists)
- **Likely files:** `crates/fp-character/src/lib.rs`, `crates/fp-engine/src/lib.rs` (team side / id registry
  into `EvalEnv`).
- **Acceptance criteria:**
  - `TeamSide` returns 1 (left/P1 team) or 2 (right/P2 team).
  - `PlayerIDExist(n)` returns 1 iff a player/helper with that id currently exists.
  - Safe defaults for a single-character `Character` (TeamSide=1, PlayerIDExist=0).
- **Implementation GUIDES:**
  - `TeamSide` is a per-entity constant set at side assignment (like `ai_level` in T052). `PlayerIDExist`
    reuses the redirect playerid id-space, surfaced through `EvalEnv` as a lookup.
  - Gotcha: `TeamSide` is NOT `Facing` — a left-team player can face either way; tie it to roster slot.
- **Verification:** `cargo test -p fp-character team_triggers` — a P2-side character → `TeamSide==2`;
  `PlayerIDExist` 1 for a live helper id, 0 for an unused id.
- **Reference:** mechanics-ref §5.1 + trigger reference.

---

### F026: Player-Facing Legibility Layer (hitbox view, input display, frame data)

Productize the developer F1 overlay + input ring buffer + AIR frame data into a *player-facing* layer.
Highest impact-per-effort in the experience track — the hard parts already exist.

**Requirements**
- Color-coded hitbox/hurtbox overlay, toggleable independently of the dev F1 toggle, facing-mirrored.
- On-screen input display: last N inputs as direction/button glyphs + recognized command name.
- Computed startup/active/recovery + on-block/on-hit frame advantage, displayed in 60Hz ticks.

#### T063: Player-facing hitbox/hurtbox view (color-coded, labeled, toggleable)
- **Title:** Player hitbox/hurtbox overlay
- **Priority:** P0
- **Dependencies:** none (builds on the existing F1 overlay)
- **Likely files:** `crates/fp-app/src/main.rs` (overlay draw + key handling),
  `crates/fp-render/src/renderer.rs` (debug-box primitive), `crates/fp-render/src/text.rs` (labels).
- **Acceptance criteria:**
  - A `TrainingOverlay` (independent of the raw F1 dev toggle) renders Clsn2/hurtbox **blue**,
    Clsn1/hitbox **red**, push/Width box a third color, each semi-transparent, with a small legend.
  - Toggleable per side (P1/P2/both/off); state persists for the session.
  - Boxes are facing-mirrored correctly (reuse `place_clsn`; verify left- and right-facing).
- **Implementation GUIDES:**
  - Do NOT fork the box-mapping math. Extract the existing F1 mapping into a reusable
    `fn collect_clsn_boxes(char, side) -> Vec<DebugBox{rect, kind}>` and feed both overlays from it; the
    player overlay differs only in styling + toggle scope.
    ```rust
    enum ClsnKind { Hit, Hurt, Push }   // Clsn1->Hit, Clsn2->Hurt, Width/push->Push
    struct TrainingOverlay { scope: OverlayScope, show_legend: bool }
    enum OverlayScope { Off, P1, P2, Both }
    ```
  - Gotcha: the dev F1 overlay must keep working unchanged — gate the player overlay behind the
    Training-mode state (T067) or a separate key; reuse the existing alpha-blend path, no new pipeline.
- **Verification:** `cargo test -p fp-app collect_clsn_boxes` — synthetic AIR frame with a known Clsn1+Clsn2
  → two boxes, correct `kind`, facing-mirrored rects. Live: enable overlay on `evilken`, `screencapture`
  per the CLAUDE.md live-debug workflow, confirm red-on-attack / blue-on-hurt.
- **Reference:** player deep-dive §1.3 + gap #2.

#### T064: On-screen input display (last N inputs + recognized command name)
- **Title:** On-screen input display
- **Priority:** P0
- **Dependencies:** none (ring buffer + snapshot exist)
- **Likely files:** `crates/fp-input/src/buffer.rs` (read API), `crates/fp-input/src/command.rs`
  (recognized-command name), `crates/fp-app/src/main.rs` (HUD draw), `crates/fp-render/src/text.rs`.
- **Acceptance criteria:**
  - A strip shows ~16 frames of input as direction (numpad 1–9) glyphs + lit buttons, coalesced runs with a
    repeat count, newest-anchored.
  - When a special command is recognized this frame, its name flashes next to the strip.
  - Toggleable, per-side, off by default outside Training.
- **Implementation GUIDES:**
  - The ring buffer stores `InputState` per frame (`get(0)`=newest). Coalesce newest→oldest into
    `(InputState, repeat)` rows, cap ~16.
    ```
    fn input_display_rows(buf, max_rows) -> Vec<(InputState, u8)>:
        walk i in 0..buf.len(); collapse identical runs into (state, repeat); break at max_rows
    ```
  - Direction→numpad facing-relative (forward = toward opponent). Recognized command: the matcher knows the
    matched names this tick (drives `ActiveCommands`); if not retained post-match, add a `last_matched:
    Vec<CommandName>` updated in the recognize step (no semantic change). Glyphs: HUD font covers 0-9 A-Z —
    draw digits before authoring arrow sprites.
  - Gotcha: keyboard is sampled once per frame — read after the input-fold, before the catch-up tick loop.
- **Verification:** `cargo test -p fp-input input_display_rows` — scripted fwd,fwd,down,down+X → coalesced
  rows + counts. Live: throw a botched fireball (down then late forward) and confirm the crouch-punch read.
- **Reference:** player deep-dive §1.2/§1.3 (pp.37–38) + gap #4.

#### T065: Frame-data computation — startup / active / recovery + on-block advantage
- **Title:** Frame-data readout
- **Priority:** P1
- **Dependencies:** T063 (shares overlay/HUD surface)
- **Likely files:** `crates/fp-formats/src/air.rs` (per-element duration + clsn1 presence),
  `crates/fp-character/src/lib.rs`, new `crates/fp-character/src/framedata.rs`,
  `crates/fp-app/src/main.rs` (readout draw).
- **Acceptance criteria:**
  - For the executing attack, display startup (frames before first Clsn1), active (frames Clsn1 present),
    recovery (remaining to actionable), in 60Hz ticks.
  - Display on-block/on-hit frame advantage when the move connects (e.g. "+3 / −5").
  - Values match a hand-counted reference for `trainingdummy` within ±0 frames for a deterministic move.
- **Implementation GUIDES:**
  - AIR elements carry per-element `time` + Clsn1 presence; compute static frame data per action once at
    load, cached on the character:
    ```
    struct MoveFrameData { startup, active, recovery, total }
    first_active = first elem with has_clsn1; startup=sum(time before); active=sum(contiguous has_clsn1);
    recovery = total - startup - active
    ```
  - On-block/on-hit advantage is dynamic: at contact, defender's blockstun/hitstun (already tracked) minus
    attacker's frames-until-actionable (current move `recovery - elapsed`); surface via a `TickReport` field.
  - Gotcha: looping / variable-cancel states and AIR `time = -1` (hold forever) → display "—", never a wrong
    number, never assert (const-0/`None` fallback per the error philosophy).
- **Verification:** `cargo test -p fp-character MoveFrameData_compute` — synthetic AIR (startup-only,
  startup+active, active+recovery, time=-1 hold) exact counts; `cargo test -p fp-engine` scripted
  attack-on-block advantage == blockstun − attacker-recovery; live hand-count on `trainingdummy`.
- **Reference:** player deep-dive §1.3 (p.12, p.21, p.46) + gap #3.

---

### F027: The Lab — Training Mode, Dummy Control, Record/Playback

A real Training mode (not just the existing select-flow shortcut): mode state, a `DummyCommandSource`,
infinite resources, reset, and record→playback built on the shipped snapshot/replay core.

**Requirements**
- A `GameMode::Training` match: no timeout, no auto-KO, a reserved Training HUD region.
- A controllable dummy (Stand/Crouch/JumpLoop/BlockAll/BlockAfterFirst/CPU), infinite life/meter toggles,
  reset-to-start; block follows facing (cross-up correct).
- Record→playback of a dummy setup that replays deterministically each loop.
- The Versus path stays byte-for-byte unaffected (no `determinism.rs` regression).

#### T066: Training-mode `GameMode` state + menu/round-flow plumbing
- **Title:** `GameMode::Training` plumbing
- **Priority:** P0 (gates F027/F029)
- **Dependencies:** none
- **Likely files:** `crates/fp-app/src/main.rs` (`cli_route`/menu wiring), `crates/fp-app/src/screens.rs`
  (the existing `SelectMode::Training` flow), `crates/fp-engine/src/lib.rs` (round-flow gate).
- **Acceptance criteria:**
  - Selecting Training enters a match flagged `GameMode::Training`; the round timer does not expire and a KO
    does not auto-end the round; a Training HUD region is reserved for T063/T064/T065.
  - A normal match is unaffected (`determinism.rs` still byte-equal on the Versus path).
- **Implementation GUIDES:**
  - `enum GameMode { Versus, Training }` threaded from the menu into match setup; gate round-flow
    side-effects (KO/timeout `RoundState` transitions) on `mode == Versus`. Note: `SelectMode::Training`
    already exists as a *select-flow* shortcut (P2 mirrors P1) — this adds the *match-time* mode it implies.
  - Gotcha: Training disables round termination but must NOT disable the tick/snapshot machinery (record/
    playback depends on it); don't leak Training-only state into `MatchSnapshot` fingerprints.
- **Verification:** `cargo test -p fp-app` route test — Training selection → Training match setup; live: boot
  the app, enter Training, confirm no timeout and no auto-KO end.
- **Reference:** player deep-dive §4 FG-1 + gap #5.

#### T067: Dummy control — stance, block modes, infinite life/meter, reset position
- **Title:** Training dummy control
- **Priority:** P0
- **Dependencies:** T066
- **Likely files:** new `crates/fp-app/src/training/dummy.rs` (a `DummyCommandSource`),
  `crates/fp-character/src/lib.rs` (`set_command_source` ~2490 — reuse), `crates/fp-app/src/main.rs`
  (training menu + keys), `crates/fp-engine/src/lib.rs` (life/meter clamp hook).
- **Acceptance criteria:**
  - A submenu/quick-keys set P2 dummy: Stand · Crouch · JumpLoop · BlockAll · BlockAfterFirst · CPU.
  - Infinite life / infinite meter toggles (either fighter) and a reset-to-start-positions key.
  - Block modes guard cross-ups (block direction follows facing) — verified, not assumed.
- **Implementation GUIDES:**
  - Implement `DummyCommandSource: CommandSource` emitting the held-state commands the executor already
    understands; swap in via `set_command_source` — **no executor change** (mirrors the existing tests that
    drive `holdup`/`fwd`).
    ```rust
    enum DummyMode { Stand, Crouch, JumpLoop, BlockAll, BlockAfterFirst, Cpu }
    // BlockAll => name == block_command() (holdback relative to facing);
    // BlockAfterFirst => was_hit && name == block_command();
    // Crouch => "holddown"; JumpLoop => holdup on a cadence; Stand|Cpu => false
    ```
    `was_hit` from the `TickReport` connecting-hit signal, cleared on reset/round.
  - Infinite life/meter: clamp in `Match::tick`'s post-apply step gated on the toggle (one place,
    snapshot-safe). Reset-position: reuse round-init placement.
  - Gotcha: BlockAll picks `holdback` *relative to the dummy's facing* — recompute each tick from `EvalCtx`
    facing so a jump-over still blocks (exercises crossup-correct blocking). CPU delegates to `CpuAi`.
- **Verification:** `cargo test -p fp-engine`/`-p fp-character` — attack into `BlockAll` → dummy guards,
  chip-only; cross-up jump-in → block holds; infinite-life keeps `life==life_max` after a hit. Live:
  BlockAfterFirst — first hit lands, second blocked.
- **Reference:** player deep-dive §4 FG-1 + §1.2 (pp.54–55), gaps #5/#12.

#### T068: Setup record / playback (rehearse a setup against a replaying dummy)
- **Title:** Training setup record/playback
- **Priority:** P1
- **Dependencies:** T066, T067
- **Likely files:** new `crates/fp-app/src/training/record.rs`, `crates/fp-input/src/buffer.rs`
  (`InputBufferSnapshot` — exists), `crates/fp-engine/src/snapshot.rs` + `crates/fp-engine/src/replay.rs`
  (reuse `snapshot`/`restore_snapshot`/`ReplayLog`), `crates/fp-app/src/main.rs`.
- **Acceptance criteria:**
  - A Record key captures the dummy's per-frame inputs from a start position until Stop; Playback drives the
    dummy by replaying exactly those inputs, looping from the recorded start state.
  - Playback is deterministic — the dummy reproduces the identical motion every loop.
  - Reset re-seats both fighters to the record's start snapshot each loop.
- **Implementation GUIDES:**
  - A recording is `{ start: MatchSnapshot bytes, inputs: Vec<InputState> }`. Record = capture
    `Match::snapshot()` at start, push each frame's dummy `InputState`. Playback = `restore_snapshot(start)`,
    install a `RecordedCommandSource` yielding `inputs[frame]` through the normal matcher path; at end,
    restore + loop. **Reuse the shipped determinism core (`replay.rs`/`snapshot.rs`) — only the recording
    buffer + the playback `CommandSource` are new.**
  - Store *raw* `InputState` (not the recognized command) so a recorded fireball replays as a fireball —
    exactly what `InputBufferSnapshot` models. `PlayerSnapshot::apply_to` already restores `input_buffer` +
    `matcher`, so a loop won't drop a multi-frame motion mid-recognition.
  - Gotcha: only the dummy is replayed; don't snapshot the player-controlled side's future inputs.
- **Verification:** `cargo test -p fp-engine` — record a scripted dummy sequence, play back twice, assert
  `Match::snapshot()` byte-equal at the end of each loop (extends `determinism.rs`). Live: record a
  wakeup-reversal, practice punishing it on loop.
- **Reference:** player deep-dive §1.2, Tip 9 (p.98).

---

### F028: Teaching CPU & Difficulty Ladder

Wire the existing `AiDifficulty`/`AiTuning`/`CpuAi` substrate to a menu selector (today P2 is hardcoded
`Normal`) and add teaching behavior modes — the solo learner's foil.

**Requirements**
- Options exposes Easy/Normal/Hard, deterministically seeded, default Normal.
- Teaching behaviors (Pure Blocker, Reactive DP, Whiff Punisher), deterministic and observably distinct.

#### T069: Menu-selectable CPU difficulty (Easy/Normal/Hard) wired end-to-end
- **Title:** CPU difficulty selector
- **Priority:** P1
- **Dependencies:** T066 (menu surface)
- **Likely files:** `crates/fp-input/src/ai.rs` (substrate exists), `crates/fp-app/src/main.rs` (P2 control
  selection — `CpuAi::new(..., AiDifficulty::Normal)` hardcoded at main.rs:3521), `assets/data/system.def`
  (Options), `crates/fp-ui/src/system_def.rs`.
- **Acceptance criteria:**
  - The Setup/Options screen exposes a CPU Difficulty selector setting P2's `CpuAi` tuning for subsequent
    matches; default Normal.
  - Easy demonstrably blocks/attacks less than Hard (via `block_chance`/`attack_range`).
  - Selection is seeded deterministically (reproducible with a fixed match seed).
- **Implementation GUIDES:**
  - Almost entirely wiring: `CpuAi::new(seed, difficulty)` exists; replace the hardcoded `Normal` with a
    persisted setting (alongside the existing key-remap / HUD-customization settings the Options screen
    already persists). Derive the CPU seed via the existing `derive_player_seed`.
  - Gotcha: don't break the default-match determinism test — default seed + default difficulty must
    reproduce the same play-out. Persist where the other Options live, not a new ad-hoc file.
- **Verification:** `cargo test -p fp-input` — `AiTuning::for_difficulty(Easy).block_chance <
  for_difficulty(Hard).block_chance`. Live: Easy vs Hard, confirm Hard blocks/punishes more.
- **Reference:** player deep-dive gap #15; substrate at `crates/fp-input/src/ai.rs`.

#### T070: Teaching CPU behavior modes (Pure Blocker, Reactive DP, Whiff Punisher)
- **Title:** Teaching CPU behaviors
- **Priority:** P2
- **Dependencies:** T069
- **Likely files:** `crates/fp-input/src/ai.rs` (extend `AiObservation`/`decide`), `crates/fp-app/src/main.rs`.
- **Acceptance criteria:**
  - Selectable modes beyond raw difficulty: Pure Blocker (only blocks), Reactive DP (anti-air/wakeup
    reversal), Whiff Punisher (punishes a missed attack). Each deterministic given a seed, observably
    distinct.
- **Implementation GUIDES:**
  - Extend `AiObservation` with the signals these need — opponent's attack phase (reuse T065 frame-phase via
    `EvalCtx`), distance (`distance()` exists), airborne flag, own wakeup flag. Add a `BehaviorMode` biasing
    `decide`:
    ```
    PureBlocker:   opponent_attacking -> hold_back else neutral
    ReactiveDP:    opponent_airborne && dist<dp_range -> dp; else wakeup+close -> dp; else base
    WhiffPunisher: opponent_recovering && dist<punish_range -> dash_attack; else space
    ```
  - All randomness through the seeded `AiRng` in `ai.rs`. Cap reaction within a human-plausible window
    (don't react in 1 frame — teach, not frustrate).
- **Verification:** `cargo test -p fp-input` — drive each mode with scripted `AiObservation`s (PureBlocker
  only holds back vs an active attack; ReactiveDP emits the DP when opponent airborne in range). Live:
  blockstring vs PureBlocker; anti-air respect vs ReactiveDP.
- **Reference:** player deep-dive §1.2 / Tip 11, gap #15.

---

### F029: Learn to Play — Tutorial, Trials, Movelist

The strategic bet that turns a runtime into a product: be the teacher the genre lacks. Built on the Lab
(F027) and the teaching CPU (F028).

**Requirements**
- An in-app movelist/character-info screen derived from `.cmd` + `.def` metadata, robust to sparse content.
- A data-driven tutorial/trials runner with success detection and a never-soft-lock Skip.

#### T071: In-app movelist / character-info screen
- **Title:** Movelist / character-info screen
- **Priority:** P1
- **Dependencies:** none (complements character-select)
- **Likely files:** `crates/fp-character/src/loader.rs` (read `.cmd` command list + `.def` displayname/
  author — already parsed), `crates/fp-app/src/main.rs` (info screen draw), `crates/fp-render/src/text.rs`.
- **Acceptance criteria:**
  - From character-select, an Info action shows display name, author, and a movelist derived from the
    `.cmd` command definitions (name + motion, e.g. "Fireball — QCF+P").
  - Renders cleanly for a sparse/malformed `.cmd` (never crash; show what's parseable).
- **Implementation GUIDES:**
  - The `.cmd` parser already produces the command table; format each `[Command]` motion into a human string
    (map `~ / $ > + D F B U` to arrows/buttons), recognizing common motions (QCF/QCB/DP/charge) with a
    literal-arrow fallback for exotic commands. Pair with `displayname`/`author` from `LoadedCharacter`.
  - Gotcha: display author content, don't invent — show the raw command name. Shift-JIS `.cmd` already
    decoded by the parser; reuse it.
- **Verification:** `cargo test -p fp-app`/`-p fp-character` — `trainingdummy` movelist contains its known
  commands with expected motion strings. Live: Info on `evilken`/`kfm` shows a readable movelist.
- **Reference:** player deep-dive gap #13, Tips 2/3.

#### T072: Interactive tutorial / trials runner (scripted lesson engine)
- **Title:** Tutorial / trials runner
- **Priority:** P1
- **Dependencies:** T066, T067 (dummy), T063/T064 (overlays), optionally T070
- **Likely files:** new `crates/fp-app/src/training/tutorial.rs`, `assets/data/tutorial/*.def` (original
  clean-room lesson scripts), `crates/fp-app/src/main.rs`, `crates/fp-engine` (success-detection hooks).
- **Acceptance criteria:**
  - A Trials/Tutorial flow runs an ordered lesson list; each lesson states a goal, configures dummy +
    overlays, watches a success condition, advances on success.
  - Ships at least: Block High/Low, Attack-Block-Throw (RPS), Throw a Fireball, Anti-air/DP, a 2-hit BnB.
  - Never soft-locks: a Skip always advances; bad/missing assets fall back gracefully.
- **Implementation GUIDES:**
  - Model a lesson as **data** (authorable clean-room `.def`):
    ```
    struct Lesson { title, instruction, dummy: DummyMode, overlays: OverlayFlags,
                    success: SuccessCond, timeout_hint: Option<u32> }
    // SuccessCond: LandCommand("fireball"), BlockNHits(3), ComboCount(2), AntiAir, ThrowConnected
    ```
    A `TutorialRunner` holds `Vec<Lesson>` + index; each tick applies config, evaluates `success` against
    the live `Match`/`TickReport`, advances on success.
  - Reuse signals already emitted — connecting-hit / command-recognized (`TickReport`/matcher), combo
    counter, guard events, airborne (`EvalCtx`). Add a thin `LessonEvent` enum the runner consumes.
  - Gotcha: lesson scripts are *original* clean-room assets pointing at the shipped `trainingdummy`. The
    runner must Skip-on-unsatisfiable so lessons still load against an arbitrary character. Poll the success
    condition each frame; never block the tick loop.
- **Verification:** `cargo test -p fp-app` — a headless `TutorialRunner` fed scripted events advances
  through the list and detects each `SuccessCond` exactly once. Live: complete "Throw a Fireball" with
  `trainingdummy`.
- **Reference:** player deep-dive §1.2/§1.4 (p.4), FG-4, RPS model pp.58–60.

---

### F030: Game-Feel — Readable Risk/Reward & Momentum

Close the tech-demo-vs-game feel gap on the experience side: hitstop legibility, comeback/meter
readability, and input leniency. (The screenpack `[Combo]` counter already draws — excluded.)

**Requirements**
- Hitstop scales with hit strength and reads as a brief freeze on heavier hits.
- Resource state is legible (max-meter flash, low-life color shift); presentational only.
- Jump-press buffer + a validated command-buffer leniency window, deterministic, no new false matches.

#### T073: Hitstop legibility — scale hitpause with hit strength
- **Title:** Hitstop strength-scaling
- **Priority:** P1
- **Dependencies:** none (hitpause/freeze exist)
- **Likely files:** `crates/fp-character/src/combat.rs` / `crates/fp-engine/src/lib.rs` (hitpause from
  `HitDef.pausetime`), `crates/fp-app/src/main.rs`.
- **Acceptance criteria:**
  - The attacker's hitpause duration scales with `HitDef` `pausetime`/strength so heavy hits read heavier;
    surfaced from existing data, not a new system.
  - No gameplay regression for the KFM/evilken feature set.
- **Implementation GUIDES:**
  - MUGEN's `pausetime` already drives hitpause — surface it rather than inventing. Drive the freeze
    duration from the connecting `HitDef`'s pausetime; keep it deterministic.
  - Gotcha: this is the *attacker's* hitpause; keep it consistent with the existing `FreezeRequest` path.
- **Verification:** `cargo test -p fp-engine` — a heavy-`pausetime` hit yields a longer attacker hitpause
  than a light one. Live: light vs heavy hit reads distinctly.
- **Reference:** player deep-dive §1.3 / gap #8.

#### T074: Comeback/meter legibility — low-life and max-meter visual state
- **Title:** Resource-state HUD legibility
- **Priority:** P2
- **Dependencies:** none
- **Likely files:** `crates/fp-app/src/main.rs` (HUD draw), `crates/fp-ui/src/renderer.rs`.
- **Acceptance criteria:**
  - Power bar flashes/changes color at max (super available); life bar shifts color at low life
    (<25%). Purely presentational; never crashes on missing HUD assets.
- **Implementation GUIDES:**
  - Bars already draw; add threshold-based tinting driven from the per-frame HUD values, reusing the
    PalFX-style tint / quad colors. Drive the flash from frame count (no RNG → deterministic). Respect the
    `fight.def` screenpack styling when present.
- **Verification:** unit threshold→color mapping test; live: build meter to max (flash), drop below 25%
  (red-shift).
- **Reference:** player deep-dive §1.3 (pp.121–123) / gap #9.

#### T075: Input leniency — jump-press buffer + command-buffer window
- **Title:** Input leniency (jump buffer + command window)
- **Priority:** P1
- **Dependencies:** none
- **Likely files:** `crates/fp-input/src/buffer.rs`, `crates/fp-input/src/command.rs` (matcher window),
  `crates/fp-character/src/loader.rs` (`BUILTIN_GROUND_LOCOMOTION_CNS` jump gate, loader.rs:1100).
- **Acceptance criteria:**
  - A jump press buffered within a small window (~3–4 frames) before landing executes on the first
    actionable frame.
  - The command matcher tolerates a small leniency window without making unrelated commands misfire.
  - Versus determinism unchanged (buffering is deterministic, in the input layer).
- **Implementation GUIDES:**
  - The ring buffer holds the last 60 frames. Jump-buffer = on the actionable transition, scan the last N
    frames for an unconsumed up-press and fire it. Command leniency = expose the matcher's frame window as a
    small config; validate no false-positive increase.
  - Gotcha: only apply buffered/variable jump to the engine's *built-in* locomotion fallback
    (`BUILTIN_GROUND_LOCOMOTION_CNS`) — variable jump height is a *content* concern for authored jump arcs,
    don't override content. Add a regression test (from the `real_kfm_cmd`-style fixtures) asserting no new
    false matches (a `QCF` must not eat a `DP`). Keep sampling once-per-frame.
- **Verification:** `cargo test -p fp-input` — jump pressed 2 frames before landing fires on the first
  actionable frame; assert no new false command matches against existing fixtures. Live: mash jump on
  landing recovery and feel it come out reliably.
- **Reference:** player deep-dive §1.3 (p.8); engines paper §2.2.

---

### F031: Study & Compete — Replay Study UI + Rollback Groundwork

Build the study + competition layer on the **already-shipped** replay/determinism core (`replay.rs`,
`snapshot.rs`, `determinism.rs`). The replay *file format* is done; this adds the *UI* and the
rollback audit.

**Requirements**
- A replay viewer loads a `ReplayLog`, plays it, and supports pause/step±1/seek, with the F026 overlays
  toggled on the replay.
- A rollback-ready audit harness proves the save/advance/restore/re-advance byte-equality invariant and
  measures snapshot cost.

#### T076: Replay study UI — load, scrub frame-by-frame, overlays on playback
- **Title:** Replay study UI (scrub + overlays)
- **Priority:** P1
- **Dependencies:** T063/T064/T065 (overlays); reuses `replay.rs`/`snapshot.rs` (exist)
- **Likely files:** `crates/fp-app/src/main.rs` (replay-viewer mode + transport),
  `crates/fp-engine/src/replay.rs` / `crates/fp-engine/src/snapshot.rs` (seek-via-restore + re-sim).
- **Acceptance criteria:**
  - A Replay Viewer loads a `ReplayLog`, plays it, and supports pause, step ±1 frame, and seek (scrub) to an
    arbitrary frame.
  - The F026 overlays (hitbox view, input display, frame data) toggle on the replay.
- **Implementation GUIDES:**
  - Seek = restore the start snapshot then fast-forward N frames feeding the recorded inputs (engine is
    forward-only + deterministic; re-sim from start is cheap at fighting-game tick counts). For long
    replays, cache snapshots ("keyframes") every K frames and seek from the nearest:
    ```
    seek(f): kf = nearest_keyframe <= f; restore(kf.snapshot); for i in kf.frame..f: tick(inputs[i])
    ```
  - The overlays are pure draw layers over the live `Match` — once the replay drives the same `Match`, they
    "just work" (the payoff of building F026 on live state). Step-back = seek-to-(current−1) (re-sim); never
    attempt reverse integration. Keep transport input separate from match input; never block on file I/O in
    the tick loop.
- **Verification:** unit — `seek(f)` then `seek(f)` again yields identical snapshots. Live: record a match,
  open it in the viewer, scrub to a specific hit, toggle hitboxes, confirm the connecting box.
- **Reference:** player deep-dive §4 FG-5; builds on the shipped `replay.rs`.

#### T077: Rollback-ready snapshot audit + harness (netplay groundwork)
- **Title:** Rollback-readiness audit harness
- **Priority:** P2
- **Dependencies:** T076 (replay exercises save/restore under load)
- **Likely files:** `crates/fp-engine/src/snapshot.rs`, `crates/fp-engine/src/lib.rs` (tick re-entrancy),
  new `crates/fp-engine/tests/rollback.rs`.
- **Acceptance criteria:**
  - The snapshot API is validated for rollback: save state, advance K with predicted inputs, restore,
    re-advance with corrected inputs → byte-equal to a from-scratch run with those inputs (GGPO invariant).
  - A documented measurement of snapshot size + save/restore cost per frame (rollback budget).
  - No netcode transport is built — groundwork only.
- **Implementation GUIDES:**
  - Rollback needs exactly what's shipped — fast `capture`/`apply_to` + deterministic re-sim. Add a
    `rollback.rs` test doing the save→advance→rollback→re-advance loop, asserting byte-equality vs the
    canonical run; measure `snapshot()` bytes + capture/restore wall-time (typical rollback ≤ ~7–8 frames).
  - Gotcha: any nondeterminism (HashMap iteration order in serialized state, float NaN, time-based RNG)
    breaks rollback — the determinism tests guard most; the audit *proves* it holds under repeated
    save/restore. Per-tick scratch must clear on restore — confirm under rollback.
- **Verification:** `cargo test -p fp-engine --test rollback` (the byte-equality loop); a `--nocapture`
  print of snapshot size + save/restore timing.
- **Reference:** player deep-dive §4 FG-5 / gap #14 / Tip 10 (GGPO); builds on the shipped determinism core.

---

### F032: Projectile & Hit-State Completeness

Aggregate id-less projectile triggers and finish the GetHitVar set.

**Requirements**
- Bare no-id `Proj*` triggers aggregate across all owned projectiles; `ProjCancelTime`/`NumProjID` resolve.
- The three unpopulated GetHitVars (`hit_type`/`hitcount`/`isbound`) are populated (15/15).

#### T078: Bare no-id `Proj*` triggers + `ProjCancelTime` + `NumProjID`
- **Title:** Id-less projectile triggers + ProjCancelTime/NumProjID
- **Priority:** P2
- **Dependencies:** none (per-id `proj_events` tracker exists, lib.rs:1825)
- **Likely files:** `crates/fp-character/src/lib.rs` (`parse_proj_trigger`, `ProjContactTracker`,
  `proj_events`).
- **Acceptance criteria:**
  - `ProjHit`/`ProjContact`/`ProjGuarded` with no id return the aggregate across all owner projectiles (true
    iff any matches; time = most-recent matching event).
  - `ProjCancelTime` returns ticks since cancel; `NumProjID(id)` counts live projectiles with that projid.
  - Existing per-id `Proj*<id>` triggers unchanged.
- **Implementation GUIDES:**
  - `proj_events` is `HashMap<i32, ProjContactTracker>`. No-id form folds across entries:
    ```
    Some(id) => proj_events.get(&id).map(|t| t.hit_time()).unwrap_or(NEVER),
    None     => proj_events.values().map(|t| t.hit_time()).filter(|&x| x!=NEVER).min().unwrap_or(NEVER)
    ```
    Add a branch in `parse_proj_trigger` where the id is absent → aggregate path. Keep MUGEN's comparison
    semantics consistent between forms.
- **Verification:** `cargo test -p fp-character bare_proj_triggers` — events on two projids; bare `ProjHit`
  true if either hit; aggregate time = the more recent.
- **Reference:** mechanics-ref §5.2; known-issues bullet.

#### T079: Populate the three remaining GetHitVars (`hit_type`, `hitcount`, `isbound`)
- **Title:** Complete the GetHitVar set
- **Priority:** P2
- **Dependencies:** none
- **Likely files:** `crates/fp-character/src/combat.rs` (`resolve_attack` GetHitVars).
- **Acceptance criteria:**
  - `GetHitVar(type)` returns the hit's type code; `GetHitVar(hitcount)` the running combo hit count on the
    defender; `GetHitVar(isbound)` whether the defender is target-bound. 15/15 populated (was 12/15).
- **Implementation GUIDES:**
  - `resolve_attack` already populates 12 — extend the same write. `hitcount` increments per connecting hit
    while the combo is live (reset on neutral); `isbound` reads the `TargetBind` state.
  - Gotcha: `hitcount` is the defender's combo counter — single source of truth with the HUD combo counter
    (`active_combo_count`), don't double-count.
- **Verification:** `cargo test -p fp-character gethitvar_completeness` — a 3-hit combo →
  `GetHitVar(hitcount)==3`; bind a target → `GetHitVar(isbound)==1`.
- **Reference:** known-issues "Some GetHitVars stay at defaults"; mechanics-ref §5.

---

### F033: SuperPause Defence/Invuln Fidelity & Controller Polish

Honor the mechanically-meaningful SuperPause params and close the last no-op controller.

**Requirements**
- SuperPause `unhittable` makes the triggerer invulnerable for the pause; `p2defmul` scales the opponent's
  defence for the window.
- `LifebarAction` has a real dispatch arm (no warn-no-op).

#### T080: SuperPause `p2defmul` + `unhittable` windows
- **Title:** SuperPause defence/invuln windows
- **Priority:** P2
- **Dependencies:** none (freeze timer + `FreezeExempt` exist, fp-engine/src/lib.rs:590)
- **Likely files:** `crates/fp-engine/src/lib.rs` (freeze application), `crates/fp-character/src/executor.rs`
  (SuperPause arm params), `crates/fp-character/src/combat.rs` / `crates/fp-character/src/invuln.rs`.
- **Acceptance criteria:**
  - During a SuperPause the triggerer is invulnerable iff `unhittable=1`; the opponent's effective defence is
    multiplied by `p2defmul` for the pause window. Defaults match MUGEN; the window ends with the pause.
- **Implementation GUIDES:**
  - The freeze already tracks the exempt triggerer. Attach `SuperPauseEffect { unhittable, p2defmul,
    until_tick }` to the player; consult it in the invuln gate (`invuln.rs`) and the damage step in
    `resolve_attack`.
  - Gotcha: `p2defmul` applies to the *opponent's* defence, not the triggerer's. Reset the multiplier when
    the freeze clears.
- **Verification:** `cargo test -p fp-engine superpause_unhittable` — SuperPause `unhittable=1` → an incoming
  hit during the pause deals no damage; `p2defmul=2.0` → post-pause damage to opponent doubles.
- **Reference:** mechanics-ref §3.2/§3.3.

#### T081: Implement the `LifebarAction` controller (currently a no-op)
- **Title:** `LifebarAction` controller arm
- **Priority:** P3
- **Dependencies:** none
- **Likely files:** `crates/fp-character/src/executor.rs` (dispatch arm), `crates/fp-engine/src/lib.rs`
  (lifebar/round-flow signal).
- **Acceptance criteria:**
  - `LifebarAction` has a real arm that signals the round-flow/HUD instead of the logged no-op; recognized
    and routed (no debug-no-op). Cosmetic effect on the announcer is an acceptable first cut.
- **Implementation GUIDES:**
  - Emit a `TickReport` signal (deferred-effects pattern) that `fp-engine` consumes. Keep it minimal — a
    recognized arm flagging "win-pose announced" is enough for parity; full lifebar choreography is out of
    scope.
- **Verification:** `cargo test -p fp-character lifebaraction_recognized` — dispatches to a real arm (no
  warn-no-op) and sets the expected report flag.
- **Reference:** controller reference; closes the last MISSING controller from the grep.

---

### F034: Content Import / Preprocessing Pipeline

An offline `fp-app import` step that ingests as-authored MUGEN content, runs the *existing* tolerant
load, and emits a repair report, a repaired text overlay, and a local-only IR cache. It **never adds
tolerance** — it calls the F018 tolerant parsers as the repair oracle.

**Requirements**
- `import --report` collects every repair the loader/parsers already performed into a tiered report
  (Repaired / Flagged / Advisory), rendered human-readable and as stable JSON; `--strict` gates on flags.
- `import --out` emits a repaired text overlay (CNS/CMD/AIR/DEF; SFF/SND reported, never modified) that
  re-parses with zero warnings; clean files round-trip byte-identical.
- A hash-keyed, local-only IR cache skips re-parse/compile on a fresh hit and invalidates on any source or
  format-version change.
- **Clean-room write-guard:** every writing path refuses to write under tracked `assets/`, writes only under
  a gitignored output/cache root, prints the license reminder, and is tested against synthetic/trainingdummy
  fixtures — never committed third-party content.

#### T082: Import core — `ImportReport`/`Repair` model + repair inventory over the tolerant load
- **Title:** Import core model + repair collection
- **Priority:** High
- **Dependencies:** none
- **Likely files:** `crates/fp-character/src/loader.rs` (capturable repair hook beside the `warn!` sites),
  new `crates/fp-app/src/import.rs`, `crates/fp-app/src/validate.rs` (reuse `analyze`,
  `check_missing_sprites`), `crates/fp-app/src/main.rs` (`import` route mirroring `validate`).
- **Acceptance criteria:**
  - `ImportReport` with three tiers holding
    `Repair { file, line_no: Option<usize>, kind: RepairKind, original, replacement: Option<String> }` +
    an `is_clean()` ≡ "zero Flagged".
  - `import --report <char.def>` prints the human report and exits 0 even with flags; `--report-json <path>`
    writes JSON.
  - A synthetic malformed-CNS fixture (a `[Statedef]` with `Special cancelling`, `t`, an empty-key line, one
    empty trigger expr, and a zero-dim sprite referenced by an AIR frame) imports to a report with ≥1
    `StrayLine`, ≥1 `EmptyExpr` (Repaired), and the `ZeroDimSprite` advisory — asserted by exact tally.
  - The shipped `assets/trainingdummy` imports with zero Flagged (`is_clean()` true). No file written.
- **Implementation GUIDES:**
  - Thread an optional `&mut dyn RepairSink` through `LoadedCharacter::load` so each `warn!` site *also*
    pushes a `Repair` (keep the warns — make them capturable, not replaced). After load, walk the compiled
    graph for `is_fallback` exprs and reuse `validate::analyze`/`check_missing_sprites`.
    ```
    for E in controller triggers: if E.is_fallback:
        E.source.trim().is_empty() -> Repaired(EmptyExpr, DROP) else Flagged(TruncatedExpr, E.source)
    for K in P.components (NOT joined P.source) where K.is_fallback: same split
    ```
  - `enum RepairKind { StrayLine, MalformedHeader, EmptyKey, EmptyExpr, TruncatedExpr, JunkColumn,
    ColonHeader, DeadFrame, ZeroDimSprite, MissingSpriteRef, PartialSff, PartialSnd, Transcoded, AiVarHint }`;
    `enum Tier { Repaired, Flagged, Advisory }`.
  - Gotcha: do NOT double-count multi-value params (`damage = 20, 5`) — iterate `P.components`, never the
    joined source. Do not block on flags. Write NO file in this task. `param_is_optional` is a small
    allow-list (start empty → flag everything), never a guess.
- **Verification:** `cargo test -p fp-app import_core` — synthetic-fixture tier/kind tally +
  `trainingdummy.is_clean()`. `cargo clippy -p fp-app -p fp-character --all-targets -- -D warnings`. Manual
  (local third-party): `cargo run -p fp-app -- import --report test-assets/evilken/evilken.def`.
- **Reference:** content-import design §3, §5.2/§5.4, §9 T052.

#### T083: CNS/CMD text-overlay repair (stray lines, empty keys, malformed/colon headers)
- **Title:** CNS/CMD repaired-text overlay
- **Priority:** High
- **Dependencies:** T082
- **Likely files:** `crates/fp-app/src/import.rs`, `crates/fp-formats/src/cns.rs` (expose
  `SectionKind::parse` + comment-strip helper), `crates/fp-formats/src/text.rs`.
- **Acceptance criteria:**
  - A synthetic CNS with `Special cancelling`, a bare `t`, `= 5` (empty key), `[State 9999: Foo]` (colon),
    and `[GarbageHeader` imports to an overlay that re-parses with **zero `CNS:` warnings**; the report lists
    `StrayLine`×2, `EmptyKey`×1, `ColonHeader`×1, `MalformedHeader`×1.
  - A clean CNS round-trips byte-identical. Overlay written only under the cache/output dir, never `assets/`
    (guard tested).
- **Implementation GUIDES:**
  - A **line-level** transform — never a full re-emit from the parsed model (loses comments/ordering).
    Preserve indentation + line endings; transform only the three provably-safe shapes:
    ```
    t = strip_comments_and_trim(L)
    header: if SectionKind::parse(inner) is None -> report(MalformedHeader); emit "; [unparsed] "+L
            elif colon in "[State N: label]" -> report(ColonHeader); emit colon->comma in header only
    contains '=': empty key -> report(EmptyKey); emit "; "+L ; else emit L (NEVER touch a real key)
    else: report(StrayLine); emit "; "+L
    ```
  - Gotcha: never comment a line containing `=` (a real key). Preserve the trailing newline. Colon→comma in
    the header only, never in a value. `.cmd` is parsed as CNS — share the classifier.
- **Verification:** `cargo test -p fp-app overlay_cns` (zero-warns re-parse + byte-identical clean
  round-trip); `cargo test -p fp-app import_write_guard` (write under `assets/` refused). Manual: import
  evilken, re-parse overlay with `RUST_LOG=warn`, grep `CNS:` → none.
- **Reference:** content-import design §3.A, §5.1, §9 T053.

#### T084: AIR overlay repair + dead-frame / zero-dim pruning (opt-in)
- **Title:** AIR overlay + `--prune` dead frames
- **Priority:** Medium
- **Dependencies:** T082
- **Likely files:** `crates/fp-app/src/import.rs`, `crates/fp-formats/src/air.rs` (column-salvage rule +
  frame model), `crates/fp-formats/src/sff/mod.rs` (sprite presence + linked-index).
- **Acceptance criteria:**
  - A synthetic AIR with a `2..A` column imports to an overlay carrying salvaged `2` + a `JunkColumn` repair.
  - `--prune`: a frame whose `(group,image)` is absent (or a non-linked 0×0 sprite) is removed and reported
    `DeadFrame` (Repaired); without `--prune` it is only flagged (`MissingSpriteRef`).
  - Linked / 0×0-by-design sprites are not treated as dead (via `linked_index`); pruning never empties an
    action's last frame (AIR hard-errors on zero actions) — flagged, not pruned.
- **Implementation GUIDES:**
  - Reuse the AIR salvage rule already in the parser (air.rs:508 strips `2..A`→`2`) as the oracle. Build
    `present = {(g,i) | sff sprite w>0 && h>0 && linked_index.is_none()}`; reuse the factored-out
    `check_missing_sprites` loop.
  - Gotcha: a 0×0 sprite that *is* linked resolves to real pixels — do not prune references to it. Keep the
    overlay a line-level edit (drop the specific frame line), never re-serialize the `[Begin Action]` block.
- **Verification:** `cargo test -p fp-app overlay_air` — salvaged-column + pruned-frame counts + the
  linked-sprite-survives-prune case.
- **Reference:** content-import design §3.D, §5.4, §9 T054.

#### T085: Import report rendering (human + JSON) + severity gate
- **Title:** Import report faces + `--strict`
- **Priority:** Medium
- **Dependencies:** T082
- **Likely files:** `crates/fp-app/src/import.rs`, `crates/fp-app/src/validate.rs` (share
  `LICENSE_REMINDER` + `render_report` style), root `Cargo.toml` (add `serde_json`).
- **Acceptance criteria:**
  - Human output groups by tier with per-category counts + `file:line`.
  - `--report-json <path>` emits stable, sorted JSON — byte-identical across two runs on identical input.
  - `--strict` exits non-zero iff `flagged` is non-empty; default exits 0. Clean content prints
    `PASS — no repairs needed`; `LICENSE_REMINDER` prints every run.
- **Implementation GUIDES:**
  - Extend the existing `render_report` pattern; reuse `LICENSE_REMINDER` verbatim. Sort every list by
    `(file, line_no, kind)` before render/serialize (mirror `snapshot.rs:189`). JSON via `serde_json::
    to_string_pretty` over the sorted vecs.
  - Gotcha: JSON must be stable — sort *before* serializing, never rely on HashMap order. `--strict` is for
    CI/fakoli evidence (`assert N repairs, 0 flags`); its exit code is the only flag-tied behavior.
- **Verification:** `cargo test -p fp-app import_report` — JSON snapshot equality across two encodes;
  `--strict` exit-code (flagged→non-zero, clean→0).
- **Reference:** content-import design §7, §9 T055.

#### T086: Serializable static load types (`Expr`/`CompiledState`/`LoadedCharacter`/`SffFile`)
- **Title:** Serde seam for the static load graph
- **Priority:** Medium
- **Dependencies:** T082
- **Likely files:** `crates/fp-vm/src/parser.rs` (`Expr`), `crates/fp-character/src/loader.rs`
  (`CompiledExpr`/`CompiledParam`/`CompiledController`/`CompiledState`/`LoadedCharacter`/
  `CharacterConstants`), `crates/fp-character/src/lib.rs`, `crates/fp-formats/src/sff/mod.rs`,
  `crates/fp-formats/src/air.rs`, `crates/fp-formats/src/cmd.rs`, root `Cargo.toml`.
- **Acceptance criteria:**
  - Each type derives `serde::{Serialize, Deserialize}`; a `trainingdummy` round-trips through bincode and is
    structurally equal (`PartialEq`) to the original.
  - Two serializations of the same `LoadedCharacter` are byte-identical (maps in sorted key order).
  - `SffFile` pixel-buffer round-trip is lossless; index-0-transparent invariant intact.
  - `#![warn(missing_docs)]` satisfied; `cargo clippy --workspace --all-targets -- -D warnings` clean. (No
    cache file yet — serialization seam only.)
- **Implementation GUIDES:**
  - Derive bottom-up: `fp_vm::Expr` (the leaf), then `CompiledExpr`/`CompiledParam` →
    `CompiledController`/`CompiledState` → `LoadedCharacter`, then `SffFile`/`AirFile`/`CmdFile`. For
    `HashMap` fields (`LoadedCharacter.states: HashMap<i32, CompiledState>`) serialize via a sorted
    intermediate, or `BTreeMap` if it doesn't regress the executor's keyed `states.get(&n)` hot path (verify
    perf first). Add `IrCacheHeader { format_version: u32, source_hash: [u8;32] }` (consumed by T087); wire
    no cache file here.
  - Gotcha: do NOT derive serde on the *runtime* `Character` — it is already snapshot-serialized
    (`snapshot.rs:56`); only the static load graph is in scope. Bump a `COMPILER_IR_VERSION` const on any
    layout change.
- **Verification:** `cargo test -p fp-vm -p fp-character -p fp-formats serde_roundtrip` — structural +
  byte-equality (two encodes) + SFF-pixel-lossless. `cargo clippy --workspace --all-targets -- -D warnings`.
- **Reference:** content-import design §4(b), §6.3, §9 T056.

#### T087: Local IR cache — hash-keyed read/write + invalidation
- **Title:** Hash-keyed local IR cache
- **Priority:** Medium
- **Dependencies:** T086
- **Likely files:** `crates/fp-character/src/loader.rs` (cache check atop `load`), new
  `crates/fp-character/src/ir_cache.rs`, `.gitignore` (add `.fp-cache/`), root `Cargo.toml`
  (add `blake3` + `sha2`).
- **Acceptance criteria:**
  - First load writes a cache file under `$FP_CACHE_DIR` (default `<workspace>/.fp-cache/`); a second load
    deserializes it instead of re-parsing (verified via a hit/miss counter or by mutating cached bytes).
  - Editing any source input (`.def`/`.cns`/`.cmd`/`.air`/`.sff`/`.snd`/`.act`) changes the key → re-import.
  - A corrupt or older-`format_version` cache is discarded without panic; the load still succeeds via the
    full path. The cache root is gitignored; the tool refuses to write inside `assets/`. `FP_NO_CACHE=1`
    disables the cache.
- **Implementation GUIDES:**
  - Atop `LoadedCharacter::load`: compute key, probe cache, on a verified hit deserialize + return; else full
    load + write cache. **Any** cache error → silent fall-through. The warn-flood suppression is a *side
    effect* of caching the compile step — NOT a separate suppression switch; do not mute warns on a miss.
    ```
    inputs = sorted([(relpath, sha256(bytes)) for f in def_referenced_files])
    key = blake3(encode(inputs) || PARSER_FORMAT_VERSION || COMPILER_IR_VERSION)
    write: bincode(graph) to temp file, atomic-rename into $FP_CACHE_DIR/{key}.fpir
    ```
  - Gotcha: invalidation must be airtight — hash the `.def` *and every referenced file* + both version
    consts. Atomic write (temp + rename). Consider the design §10 "Tier-1 text IR + raw SFF" split if
    pixel-buffer cache size is a problem.
- **Verification:** `cargo test -p fp-character ir_cache` — hit/miss, invalidate-on-edit, corrupt-cache
  never-panics. Manual (local): load evilken twice; the second run shows no `CNS:`/`bad expression` warns and
  is faster.
- **Reference:** content-import design §4(b), §6.1/§6.2, §9 T057.

#### T088: Engine consumes imported overlays + clean-room write-guard + docs
- **Title:** Engine adoption of overlays + write-guard + docs
- **Priority:** Low
- **Dependencies:** T083, T084, T085
- **Likely files:** `crates/fp-app/src/main.rs` (accept an overlay dir in directory-discovery),
  `crates/fp-app/src/import.rs` (write-guard), `docs/content-guide.md`, `docs/known-issues.md`,
  `.gitignore` (`*.imported/`, `.fp-cache/`).
- **Acceptance criteria:**
  - `import --out <dir> <char.def>` writes the overlay + report under `<dir>`, and the overlay dir
    loads and runs the repaired character.
  - Writing outputs under tracked `assets/` is refused with a clear error (canonicalize + prefix-match).
  - `content-guide.md` documents the workflow + three tiers; `known-issues.md` notes the overlay is
    text-only (SFF/SND reported, never modified) and restates the clean-room "local-only, derived, never
    committed" rule. `LICENSE_REMINDER` prints every run. `cargo clippy --workspace --all-targets -- -D
    warnings` + `cargo fmt --all --check` clean.
- **Implementation GUIDES:**
  - Reuse the existing directory-discovery path so an overlay dir is loadable like any roster dir. Implement
    the write-guard once and call it before *any* file write (overlay, report, cache):
    ```
    fn assert_writable(out): out=canonicalize_or_parent(out); assets=workspace/assets canonicalize;
        if out.starts_with(assets) -> Err(CleanRoomGuard)
    ```
  - Gotcha: `.gitignore` must list `*.imported/` + `.fp-cache/`. Keep the engine load path behavior-identical
    when no overlay is pointed at — overlays are strictly opt-in.
- **Verification:** `cargo test -p fp-app import_guard` — write-under-`assets/` rejection + end-to-end
  import→load→run on a synthetic/trainingdummy fixture (not committed third-party content). `cargo fmt --all
  --check`.
- **Reference:** content-import design §4, §6.1, §9 T058.

---

## Cross-task invariants (do not relearn)
- **Reuse the substrate; never fork it.** Overlays read live `Match` state (so they work on replays);
  training dummies are a `CommandSource` impl (no executor change); record/playback + replay UI build on the
  shipped `replay.rs`/`snapshot.rs`; import calls the tolerant parsers as its oracle; common states are CNS
  text the existing loader compiles.
- **Import never adds tolerance; repair, never invent.** Auto-fix only provably-inert content (empty trigger
  ≡ never-fires; dead frame draws nothing). Required-but-missing params are flagged, never filled.
- **Clean-room is non-negotiable.** Every writing/content task refuses to write under `assets/`, writes only
  under a gitignored output/cache root, prints `LICENSE_REMINDER`, and is tested against
  synthetic/trainingdummy fixtures — never committed third-party content. New default content is original.
- **Determinism via the input layer + sort-before-encode.** Buffering/leniency, CPU difficulty, and dummy
  replay live in the deterministic input layer so replays reproduce; JSON report + IR cache sort before
  encoding so identical input ⇒ byte-identical output.
- **Never crash on bad content.** New surfaces (overlays, import, training, tutorial) inherit the const-0 /
  invisible / recoverable-`FpError` discipline; display "—" rather than a wrong frame-data number.
