# Behavioral Test Harness — GUI-free, CI-runnable verification of in-game behavior

**Date:** 2026-06-16
**Status:** Design / proposal (research doc; no source changed)
**Author:** research+design session

## Problem

Today the only reliable way to confirm a code change has the intended *in-game* effect is
computer-use / `screencapture` driving the real SDL2 window — clunky and fragile (window focus, OS
permission dialogs, the compositor filter that hides `fp-app`; see `CLAUDE.md` "Live-debugging" notes).
We want a **consistent, GUI-free, CI-runnable** way to assert two things without a window:

- **(A) Full range of motion** — walk fwd/back, crouch, stand, jump (neutral/fwd/back), turn/auto-face:
  assert the character enters the right *state* and that pos/velocity respond.
- **(B) Move execution** — that the special/super moves a character *has* can be *performed*: synthesize
  the command's motion (QCF+a, DP+a, charge, double-QCF supers with meter) as a frame-by-frame input
  sequence, feed it, and assert the move's state fires. `evilken` (supers + 3000-power meter) is the
  prime worked example.

The good news from the research below: **the engine already exposes the exact seam we need.** The real
fight input path is pure and headless — `MatchInput → InputState → InputBuffer → CommandMatcher →
Character` — and there is already a deterministic record/replay + snapshot harness. We do **not** need a
window for (A) or (B). A headless renderer is a separate, optional follow-on for *visual* (golden-image)
verification.

---

## 1. How input drives a character today (the two paths)

There are **two** ways command names reach a `Character`. Understanding both is the whole design.

### Path 1 — synthetic command source (what the seed tests use)

`Character` reads active command names through a trait `CommandSource`
(`crates/fp-character/src/lib.rs:613`). Tests inject a known set with
`ActiveCommands::from_names([...])` (`lib.rs:638`, `:650`) via
`Character::set_command_source(Box<dyn CommandSource>)` (`lib.rs:2490`).

The seed test `training_dummy_walks_forward_and_back_via_const_velocity`
(`crates/fp-character/src/lib.rs:5104`) is the canonical example: it loads the shipped Training Dummy,
sets `ch.facing = Facing::Right; ch.state_no = 0; ch.ctrl = true`, calls
`ch.set_command_source(Box::new(ActiveCommands::from_names(["holdfwd"])))`, then loops
`ch.tick(&loaded, None, StageView::default())` 30× and asserts it reaches walk state 20 *and*
`pos.x` strictly increases (`:5137-5164`).

This path **bypasses `CommandMatcher` entirely** — the test asserts "command `holdfwd` is active ⇒
executor does the right thing." It does **not** prove the *motion* `~D,DF,F,a` would be recognized as
`QCF_a`. It is perfect for (A) range-of-motion, where the four built-in commands are the only ones
involved (see §3), but insufficient for (B) move execution.

### Path 2 — the real matcher (what the running game uses; the backbone for (B))

`fp_engine::Match` runs the *real* recognizer. Per fight tick, `Player::feed_input`
(`crates/fp-engine/src/lib.rs:3270`) does exactly four things:

```rust
fn feed_input(&mut self, input: MatchInput) {
    let raw = match_input_to_state(input);              // lib.rs:3271 — straight field copy, NO pre-rotate
    self.input_buffer.push(raw);                        // lib.rs:3272 — 60-frame ring buffer
    let facing_right = self.character.facing == Facing::Right;
    self.matcher.check_commands(&self.input_buffer, facing_right);   // lib.rs:3275 — REAL CommandMatcher
    // ... holding_back bookkeeping ...
    let active = snapshot_active_commands(&self.matcher, &self.command_defs); // lib.rs:3285
    self.character.set_command_source(Box::new(active)); // lib.rs:3286 — installs the matched names
}
```

`match_input_to_state` (`lib.rs:3320`) is a straight field copy — `MatchInput`'s `left/right/up/down`
are **absolute screen directions**; facing is resolved later inside the matcher
(`fp_input::logical_direction`, `fp_input::CommandMatcher::check_commands`). The matcher is built once at
`Player::new` from the character's own `.cmd`: `CommandMatcher::new(loaded.command_defs())`
(`fp-engine/src/lib.rs:778`).

`MatchInput` (`fp-engine/src/lib.rs:137`) is the renderer-/input-agnostic shape we feed: 4 directions +
6 buttons, all `bool`, all defaulting to "not held"; `MatchInput::none()` (`:163`) is the neutral frame.
`Match::tick(p1, p2)` (`:2094`) → `tick_with_partners` (`:2111`) feeds both players (`:2159`) only when
`RoundState::Fight` is active.

**Key consequence:** to test (B) we synthesize a `Vec<MatchInput>`, feed it frame-by-frame, and the
*engine's own* `CommandMatcher` decides whether the motion counts. We never reimplement input semantics.

### CommandMatcher's motion model (reuse this, don't reinvent)

`fp_input::CommandDef { name, elements: Vec<CommandElement>, time, buffer_time }`
(`crates/fp-input/src/command.rs:84`). `CommandElement` (`:31`) is `Dir { token, modifier, detect,
strict }` | `Button { button, modifier, strict }` | `Simultaneous(Vec<..>)`. `compile_command(&str)`
(`:431`) parses MUGEN `~ / $ >` + `+` `,` natively. The matcher scans the buffer backwards
(`try_match`, `:260`): each element matches a distinct, earlier frame within `time`; `Press` requires
the element absent on the *previous* buffer frame; `Hold` requires it currently held; `Release` requires
present-prev / absent-now; `>` (strict) forces frame-adjacency. The existing matcher unit tests
(`command.rs:773` `matcher_qcf_detection`, `:1132` strict, `:1640` simultaneous) already build
frame-by-frame `InputState` buffers and assert detection — they are the proof that a synthesizer is
"just" generating those frames as `MatchInput`s.

---

## 2. Recommended: command/motion synthesizer

A `CommandDef` is a list of `CommandElement`s with a `time` window. The synthesizer lowers that symbolic
motion into a **frame-by-frame `Vec<MatchInput>`** such that, when fed through `Match::feed_input`, the
matcher reports `name` active. Critically, the synthesizer's correctness is **certified by the matcher
itself** (a self-test below), so the synthesizer is allowed to be a simple, conservative lowering — it
does not need to be a second source of truth for input semantics.

### API sketch

Lives in a small new module reusable from tests. `fp-input` is the natural home (it owns
`CommandElement`/`DirToken`/`Button`/`InputState` and has no heavy deps); `fp-engine` then offers a thin
`MatchInput` adapter. Keep it `#[cfg(any(test, feature = "test-synth"))]` or a plain `pub` helper module
— it is small and useful enough to expose.

```rust
// crates/fp-input/src/synth.rs  (new)
/// One synthesized frame, in ABSOLUTE screen directions (facing resolved later
/// by the matcher, exactly like MatchInput / match_input_to_state).
pub use crate::state::InputState;

/// Lower a parsed motion into the frames that perform it for the given facing.
/// `facing_right` decides whether logical Forward = hardware right (it must
/// match the Character's facing when these frames are fed).
///
/// Strategy (deliberately simple, validated against the real matcher):
///   * For each CommandElement in order, emit ONE frame holding that element's
///     direction/button(s), plus, for `Press`/`Release` semantics, the required
///     transition frame (a neutral or button-up frame before a Press).
///   * `Hold` elements emit a held frame; `Release` emits press-then-release.
///   * `Simultaneous` sets all members on one frame.
///   * `>` (strict) elements are emitted with NO gap frame after them.
///   * Charge motions (`/$B` held then `F`) are emitted by repeating the held
///     `Hold` frame `charge_frames` times before the release/forward element.
/// Pads with leading neutral frames so a press has a clean prior frame.
pub fn synthesize_motion(
    elements: &[CommandElement],
    facing_right: bool,
    opts: SynthOpts,        // charge_frames, inter_element_gap (default 0/1), pre_neutral
) -> Vec<InputState>;

pub struct SynthOpts {
    /// Frames to hold a charge (`/$B`/`/$D` Hold element) before the next element.
    pub charge_frames: u32,        // default e.g. 60 (covers MUGEN's typical 'time' for charge cmds)
    /// Neutral frames prepended so the first Press sees a clean prior frame.
    pub pre_neutral: u32,          // default 2
}
```

```rust
// crates/fp-engine/src/lib.rs (or a test-support module) — thin adapter
impl From<&InputState> for MatchInput { /* reuse the existing field copy, inverse of match_input_to_state */ }

/// Synthesize the frames for a named command of a loaded character.
pub fn synthesize_command(
    loaded: &LoadedCharacter, name: &str, facing_right: bool, opts: SynthOpts,
) -> Option<Vec<MatchInput>> {
    let def = loaded.command_defs().into_iter().find(|d| d.name.eq_ignore_ascii_case(name))?;
    Some(fp_input::synth::synthesize_motion(&def.elements, facing_right, opts)
        .iter().map(MatchInput::from).collect())
}
```

### The self-validating contract (this is what makes it trustworthy)

The synthesizer is **verified by replaying its output through the very matcher it targets**, in the
synthesizer's own unit tests:

```rust
// for each CommandDef the synthesizer can lower:
let frames = synthesize_motion(&def.elements, true, SynthOpts::default());
let mut buf = InputBuffer::new();
let mut matcher = CommandMatcher::new(vec![def.clone()]);
let mut fired = false;
for f in &frames { buf.push(*f); matcher.check_commands(&buf, true); fired |= matcher.command_active(&def.name); }
assert!(fired, "synthesized frames must make `{}` fire", def.name);
```

Because the *acceptance check is the engine's own matcher*, the synthesizer can be a naive lowering and
still be provably correct: if a future change to matcher semantics breaks the lowering, this self-test
fails and tells us to update `synthesize_motion`, not the matcher. This is the standard fighting-game
approach (build the legal-move buffer, run the recognizer over it) and matches how MUGEN-family engines
test inputs (see References).

### Why generate frames rather than just `ActiveCommands::from_names([name])`?

For (A) the `from_names` shortcut is fine (and is what the seed test uses). For (B), feeding the *name*
directly would test only "if `QCF_a` fires, does state 3xx happen" — it would **not** prove the *motion*
is recognizable, which is the actual question ("can this move be performed?"). Synthesizing real frames
and routing them through `feed_input` exercises `compile_command` + `try_match` + facing resolution +
the `time`/`buffer_time` window — the full chain a player hits.

---

## 3. Range-of-motion (A): table-driven pattern

Reuse the T048/T052 seed pattern directly (single `Character` + `ActiveCommands` + `tick`), but make it
**table-driven** over (command set → expected state / pos / vel assertions). The four built-in
locomotion commands the engine's State -1 bridge looks for are **exactly** `"holdfwd"`, `"holdback"`,
`"holdup"`, `"holddown"` (engine-injected State -1 ChangeState controllers; see `loader.rs:1085-1135`
and the air-jump `self.commands.is_active("holdup")` at `executor.rs:1086`). The bridge transitions:

| Held command(s) | from state | → state | meaning |
|---|---|---|---|
| `holdup` (ctrl) | 0 / 20 | 40 | jump start |
| `holddown` (and not `holdup`) | 0 / 20 | 10 | crouch start |
| `holdfwd` or `holdback` (not up/down) | 0 | 20 | walk |
| none of the four | 20 | 0 | back to stand |
| (auto) crouch chain | 11 | 12 | crouch→stand |
| (auto) `pos.y>=0 && vel.y>=0` | 50/51 | 52 | land |

Worked harness (single character, no opponent needed for pure locomotion):

```rust
struct RomCase {
    name: &'static str,
    cmds: &'static [&'static str],   // installed as ActiveCommands each tick
    start_state: i32, facing: Facing, ctrl: bool,
    ticks: u32,
    expect_state: i32,               // state it must reach within `ticks`
    expect_pos_x: Ordering,          // Greater / Less / Equal vs start
    expect_pos_y: Ordering,          // for jumps: Less (up is -y) at apex
}

fn run_rom(loaded: &LoadedCharacter, c: &RomCase) -> RomResult { /* seed-test loop, record reached states + pos/vel */ }

// table
const ROM: &[RomCase] = &[
    RomCase { name: "walk fwd",  cmds: &["holdfwd"],  start_state: 0, facing: Facing::Right, ctrl: true, ticks: 30, expect_state: 20, expect_pos_x: Greater, expect_pos_y: Equal },
    RomCase { name: "walk back", cmds: &["holdback"], /* ... */ expect_state: 20, expect_pos_x: Less, expect_pos_y: Equal },
    RomCase { name: "crouch",    cmds: &["holddown"], expect_state: 10, expect_pos_x: Equal, expect_pos_y: Equal },
    RomCase { name: "stand",     cmds: &[],           start_state: 20, expect_state: 0, /* ... */ },
    RomCase { name: "jump neu",  cmds: &["holdup"],   expect_state: 40, expect_pos_y: Less /* rises */ },
    RomCase { name: "jump fwd",  cmds: &["holdup","holdfwd"], expect_state: 40, expect_pos_x: Greater, expect_pos_y: Less },
    RomCase { name: "jump back", cmds: &["holdup","holdback"], expect_state: 40, expect_pos_x: Less,  expect_pos_y: Less },
];
```

Assert on these public `Character` fields after the loop:
`pos: Vec2<f32>` (`lib.rs:1562`), `vel: Vec2<f32>` (`:1564`), `state_no: i32` (`:1654`),
`ctrl: bool` (`:1578`), `state_type: StateType` (`:1592`; `Standing/Crouching/Air/Lying`, enum `:155`),
`move_type: MoveType` (`:1594`; enum `:211`), `power: i32` (`:1574`), `power_max: i32` (`:1576`),
`life: i32`. `Character::tick(&loaded, None, StageView::default())` (`executor.rs:807`); `StageView`
default is `[-200,200]` and is fine for locomotion.

**Auto-turn / face-opponent** needs an opponent, so run those few cases on a two-player `Match` (place P1
behind/ahead of P2, feed neutral, assert `facing` flips) rather than the single-character loop.

This crate already ships ~15 single-`Character` locomotion tests (e.g. `executor.rs:5598-6527` use
`ActiveCommands::from_names(["holdup"])` for jump/airjump), so the table-driven version is a consolidation
+ extension of an established, passing pattern — low risk.

---

## 4. Per-character "all declared moves are performable" (B)

The flagship test, asset-gated on `test-assets/` like the existing KFM tests (skip-if-absent — these
characters are **not** clean-room-shippable). Worked example: **evilken** (`test-assets/evilken/`,
SFF v1, has supers + a 3000 power meter).

### What evilken actually declares (grounding the example)

From `test-assets/evilken/evilken.cmd`:
- Special (QCF): `name = "QCF_a"`, `command = ~D, DF, F, a` (and `_x/_b/_c/_y/_z` variants).
- DP-ish / charge supers and **double-QCF supers**: `name = "QCF2_a"`,
  `command = ~D, DF, F, D, DF, F, a` (and `QCF2_x`, `QCB2_*`, `HCF_x` `~D,DF,F,DF,D,DB,B,x`,
  charge `dest1` `B, DB, D, DF, F, x+y`, `asuraf_*` `~F, D, DF, a+b`, …).
- Super gating: `evilken.cns` has `triggerall = power >= 3000` on the super-move state-changes
  (e.g. the Shoryureppa family). So a super test must set `power = power_max` (3000) first.

### The command → state mapping problem (and how to handle it)

The mapping "command `QCF2_x` ⇒ state 3450" is **not** precomputed on `LoadedCharacter`. It lives in the
`[Statedef -1]` ChangeState controllers, which the loader merges into `loaded.states.get(&-1)`
(`merge_cmd_statedefs`, `loader.rs:591-608`). Two viable approaches:

- **(Recommended) Behavioral assertion — don't decode the mapping.** Drive the move from neutral with
  `ctrl=true` (+ `power=power_max` for supers) and assert the character **leaves the neutral/idle
  stand/walk states** into an attack: `move_type == MoveType::Attack` (`lib.rs:1594`) and/or
  `state_no` changed to something `> 0` and not in the locomotion set `{0,10,11,12,20,40,45,50,51,52}`.
  This proves "the move is performable" without coupling the test to a specific authored state number,
  which is exactly the property we want (it survives a character being rebalanced).
- **(Optional, stricter) Decode the expected state** for a known command by scanning
  `loaded.states[&-1]` controllers for a ChangeState whose trigger references `command = "<name>"` and
  reading its `value`. More precise but couples the test to the data file's internal numbering; reserve
  for a few hand-picked signature moves (e.g. evilken's known super state numbers).

### Test shape

```rust
#[test]
fn evilken_all_declared_moves_are_performable() {
    let def = test_assets_dir().join("evilken/evilken.def");
    if !def.exists() { eprintln!("skip: evilken absent"); return; }   // asset-gate like KFM tests
    let loaded = LoadedCharacter::load(&def).expect("load");

    // Enumerate the character's own commands (skip the AI-probe "CPUn"/recovery ones).
    let defs = loaded.command_defs();   // Vec<CommandDef> from the .cmd  (loader.rs:746)
    for def_cmd in defs.iter().filter(|d| is_real_move(&d.name)) {
        // Build a fresh two-player Match so we exercise the REAL matcher path.
        let mut m = build_match(&loaded, &loaded);   // see §5 constructor notes
        into_fight(&mut m);                          // skip intro -> RoundState::Fight
        // Grant control + meter (supers gate on power>=3000).
        m.p1_mut().character.ctrl = true;
        m.p1_mut().character.power = m.p1().character.power_max;
        let before_move = m.p1().character.move_type;

        // Synthesize this command's motion as MatchInputs and feed them; pad with
        // a few neutral tail frames so the buffered match has time to fire the state.
        let facing_right = m.p1().character.facing == Facing::Right;
        let mut frames = synthesize_command(&loaded, &def_cmd.name, facing_right, SynthOpts::default())
            .expect("synthesizable");
        frames.extend(std::iter::repeat(MatchInput::none()).take(8));

        let mut fired = false;
        for f in frames {
            m.tick(f, MatchInput::none());
            if m.p1().character.move_type == MoveType::Attack { fired = true; break; }
        }
        assert!(fired, "evilken command `{}` ({}) did not produce an attack",
                def_cmd.name, def_cmd.command);
    }
}
```

`loaded.command_defs()` (`loader.rs:746`) already filters out uncompilable commands (warn+skip), so the
enumeration is exactly the performable vocabulary. The `is_real_move` filter drops the AI-detection probe
commands (evilken's `CPU1..CPUn` like `command = D,D,U,U,D,U`) and `hold*`/recovery entries by
name-prefix, leaving the actual special/super list. For supers, the per-command self-test of §2 already
guarantees the *motion* is recognized; this test additionally proves the *executor fires the move* given
ctrl + meter.

This composes the building blocks: **enumerate** (`command_defs`) → **synthesize** (§2) → **feed real
path** (`Match::tick`/`feed_input`) → **assert behavioral change** (`move_type`/`state_no`).

---

## 5. Backbone: reuse the determinism + record/replay harness

`fp-engine` already has the exact infrastructure for a "behavioral trace" test, and it should be the
backbone rather than something new:

- `Match::new(p1, p2, bounds)` / `with_round_seconds` / `with_config` (`fp-engine/src/lib.rs:1646+`);
  `Player::new(character, loaded)` builds the matcher from `loaded.command_defs()` (`:778`).
- Read-only accessors `Match::p1()/p2() -> &Player` (`:1826/:1832`); `Player.character` is public.
- **Determinism**: `Match::seed_players(seed)` + identical inputs ⇒ identical final state
  (`tests/determinism.rs` `determinism_same_seed_and_inputs_hash_identically`).
- **Record/replay**: `ReplayLog { match_seed, bounds, …, inputs: Vec<(MatchInput, MatchInput)> }`,
  `MatchRecorder`, and `replay_match(&mut Match, &ReplayLog)` (`fp-engine/src/replay.rs`), with a
  `record_then_replay_reproduces_final_state` test. There is also `crates/fp-engine/tests/kfm_replay.rs`.
- **Snapshot**: `MatchSnapshot`/`PlayerSnapshot` (`snapshot.rs`); `Match::snapshot()/restore_snapshot()`.

**Can it be the behavioral-trace backbone? Yes.** A behavioral test *is* a short scripted
`Vec<(MatchInput,MatchInput)>` fed at a fixed seed with assertions sampled along the way. The synthesizer
(§2) is precisely a generator of the P1 half of that script. A "golden trace" variant can record the
`(state_no, pos, vel, move_type)` sequence and assert it against a committed fixture — but prefer
**semantic assertions** (state entered, pos monotonic, move fired) over exact golden traces so tests
don't churn on every tuning change.

---

## 6. Headless render readback (golden-image) — feasibility

**Recommended: yes, as a separate, smaller follow-on — not part of the first slice.** It closes the
"`fp-render` has no headless GPU readback, so visual bugs pass CPU tests" gap (`CLAUDE.md`), which is a
real class of bug (the #40 vertex-overwrite and #41/KFM-v2 palette bugs were invisible to `cargo test`).

### Why it's feasible (low-to-moderate effort)

The renderer uses **wgpu 24** (`Cargo.toml:52`) and `pollster` is already a `fp-render` dev-dependency
(`crates/fp-render/Cargo.toml`). wgpu's offscreen render-to-buffer readback is a well-trodden path (the
official wgpu "capture" example and Learn-Wgpu's "Wgpu without a window" do exactly this): render to an
owned `Texture` with `RENDER_ATTACHMENT | COPY_SRC`, `copy_texture_to_buffer` into a `COPY_DST |
MAP_READ` buffer (row-padded to `COPY_BYTES_PER_ROW_ALIGNMENT = 256`), `map_async` + `device.poll(Wait)`,
then read `get_mapped_range()` into `Vec<u8>` RGBA.

### What blocks it today (the work)

The renderer is hard-coupled to a surface: `Renderer::new(instance, surface: wgpu::Surface<'static>, …)`
(`renderer.rs:199`) takes a surface, picks `surface_format` from `surface.get_capabilities`
(`:227-233`), and configures it (`:245`). `begin_frame` calls `surface.get_current_texture()`
(`:561`); `RenderFrame::finish` calls `output.present()` (`:1031`). Every existing render test
(`renderer.rs:1179+`) is pure CPU vertex math (`build_sprite_quad`) — **none create a device**, so the
GPU pipeline itself is untested.

### Sketch (the refactor)

Introduce a target abstraction so the pipeline can render to either a surface or an owned texture:

```rust
enum RenderTarget {
    Surface { surface: wgpu::Surface<'static>, config: wgpu::SurfaceConfiguration },
    Offscreen { color: wgpu::Texture, readback: wgpu::Buffer, width: u32, height: u32 },
}

impl Renderer {
    /// Headless constructor: no surface. Picks a fixed format (Rgba8UnormSrgb),
    /// builds the SAME pipelines against it, and renders into an owned texture.
    pub async fn new_headless(instance: &wgpu::Instance, width: u32, height: u32) -> FpResult<Self>;
}
impl RenderFrame<'_> {
    /// Offscreen analog of finish(): submit, copy_texture_to_buffer, map+poll, return RGBA8 pixels.
    pub fn finish_readback(self) -> FpResult<Vec<u8>>;   // width*height*4, row-unpadded
}
```

The pipeline-building code (`renderer.rs:247-526`) is format-parameterized already (`surface_format`
threaded through), so the offscreen path mostly differs in (a) no surface config, (b) a fixed color
format, (c) `copy_texture_to_buffer` + map instead of `present()`. **Estimated effort: ~1-2 days** for
the refactor + a couple of golden tests. The adapter request currently passes
`compatible_surface: Some(&surface)` (`:208`) — headless passes `None` and may need
`force_fallback_adapter` fallback for CI runners without a GPU (Linux CI often has none; gate these tests
behind a `headless-gpu` feature / runtime adapter-probe and skip cleanly when no adapter, mirroring the
asset-gate pattern).

### Golden-image test pattern (when built)

Draw a known sprite (e.g. evilken SFF v1 frame, or a synthetic 2-color index sprite + palette) at a fixed
position, `finish_readback()`, and assert on *robust* properties rather than exact pixels: a hash of the
RGBA buffer vs a committed golden, **plus** cheap invariants (non-blank: count of non-transparent pixels
> 0; expected color present at the sprite's center; palette index 0 is transparent). Robust invariants
catch the "silhouette/black sprite" and "nothing drawn" bug classes without being brittle to driver-level
sub-pixel differences. Commit goldens only for the clean-room shippable assets (trainingdummy, the
`assets/data` fightfx) so the fixtures stay in-repo and CI-runnable; gate evilken/KFM goldens like the
other asset-gated tests.

**Recommendation:** schedule render readback as a *second* feature after the input-behavioral harness
lands — (A)/(B) deliver the most value per effort and need no GPU at all.

---

## 7. First implementation slice (smallest valuable step)

**Slice 1: the motion synthesizer + its self-validation, then the range-of-motion table.** This is the
smallest step that delivers durable value and unblocks everything else:

1. Add `fp_input::synth::synthesize_motion` (§2) + the **self-validating unit test** that replays each
   synthesized motion through `CommandMatcher` and asserts the command fires. Cover the canonical motions
   (QCF, DP/`F,D,DF`, charge `/$B…F`, double-QCF, simultaneous `x+y`) using *synthetic* `CommandDef`s
   (no assets) so it runs on CI immediately. This proves the synthesizer is correct against the engine's
   own recognizer.
2. Add the **table-driven range-of-motion test** (§3) on the shipped, non-asset-gated **trainingdummy**
   (walk fwd/back, crouch, stand, jump neu/fwd/back). Reuses the existing seed-test loop; pure CPU.

Both run on CI with zero assets beyond what already ships, and zero GPU. They convert the current
"eyeball the window" loop into red/green tests for the most common change classes (locomotion + "does
this motion parse").

**Slice 2 (next):** the `synthesize_command` `MatchInput` adapter + the per-character
"all-declared-moves-performable" test on **evilken** (asset-gated). **Slice 3 (later):** headless render
readback (§6).

---

## 8. Proposed fakoli-state feature + tasks

**Feature: "GUI-free behavioral test harness"** — assert in-game motion/move behavior in CI without a
window, replacing the screencapture loop for the common change classes.

- **T-A: Motion synthesizer + self-validation (`fp-input`).** Add `synth::synthesize_motion` +
  `SynthOpts`; unit tests that replay each synthesized motion through `CommandMatcher` and assert the
  command fires (QCF, DP, charge, double-QCF, `+` simultaneous). No assets. *Acceptance:* new tests pass;
  `clippy -D warnings` + `fmt` clean; the self-test is the correctness gate.
- **T-B: Range-of-motion table test (`fp-character`).** Table-driven walk/crouch/stand/jump
  (neu/fwd/back) on shipped trainingdummy via the seed-test loop; auto-turn case on a two-player `Match`.
  *Acceptance:* states + pos/vel monotonicity asserted; not asset-gated; CI-green.
- **T-C: `MatchInput` synth adapter + "all moves performable" test (`fp-engine`).**
  `synthesize_command(loaded, name, facing, opts) -> Vec<MatchInput>`; enumerate `loaded.command_defs()`,
  feed each via `Match::tick`, assert `move_type == Attack` (supers: set `power = power_max` first).
  Worked on **evilken**, asset-gated skip-if-absent. *Acceptance:* every real (non-`CPU*`) evilken command
  produces an attack; test skips cleanly without `test-assets/`.
- **T-D (separate/optional): Headless render readback (`fp-render`).** `Renderer::new_headless` +
  `RenderFrame::finish_readback`; one golden-image + invariants test on a shippable sprite, gated behind a
  `headless-gpu` feature / adapter-probe skip. *Acceptance:* offscreen RGBA readback works locally;
  non-blank + expected-color invariants assert; skips when no GPU adapter.

T-A → T-B can land in parallel (different crates); T-C depends on T-A. T-D is independent and can be
deferred.

---

## References (codebase, file:line)

- Two input paths: `CommandSource` trait `fp-character/src/lib.rs:613`; `ActiveCommands` `:638/:650`;
  `set_command_source` `:2490`; seed test `:5104`. Real path `Player::feed_input`
  `fp-engine/src/lib.rs:3270`; `match_input_to_state` `:3320`; matcher build `:778`; `MatchInput` `:137`;
  `Match::tick` `:2094` / `tick_with_partners` `:2111`.
- Matcher model: `fp-input/src/command.rs` — `CommandDef` `:84`, `CommandElement` `:31`,
  `compile_command` `:431`, `try_match` `:260`, `element_matches` `:305`; `state.rs` `logical_direction`
  `:126`, `dir_matches`/`dir_matches_detect` `:139/:169`.
- Executor / locomotion: `Character::tick` `executor.rs:807`; built-in State -1 bridge
  `loader.rs:1085-1135`; air-jump `is_active("holdup")` `executor.rs:1086`; the four built-in commands are
  `holdfwd/holdback/holdup/holddown`.
- Character fields: `pos` `lib.rs:1562`, `vel` `:1564`, `facing` `:1566`, `power` `:1574`,
  `power_max` `:1576`, `ctrl` `:1578`, `state_type` `:1592`, `move_type` `:1594`, `state_no` `:1654`;
  `StateType` enum `:155`, `MoveType` enum `:211`.
- Loader / commands: `LoadedCharacter` `loader.rs:441` (field `cmd: Option<CmdFile>`);
  `command_defs()` `:746`; `merge_cmd_statedefs` `:591`; CMD parser `fp-formats/src/cmd.rs` —
  `CmdCommand{name,command,time,buffer_time}` `:58-66`, `CmdFile{commands}` `:75-79`.
- Harness backbone: `Match::new` `fp-engine/src/lib.rs:1646`; `Player::new` `:776`; `p1()/p2()`
  `:1826/:1832`; `ReplayLog`/`MatchRecorder`/`replay_match` `fp-engine/src/replay.rs`;
  `MatchSnapshot` `fp-engine/src/snapshot.rs`; tests `tests/determinism.rs`, `tests/kfm_replay.rs`.
- Renderer (headless gap): `Renderer::new(.., surface, ..)` `renderer.rs:199`; surface format pick
  `:227`; `begin_frame`→`get_current_texture` `:561`; `finish`→`present` `:1031`; wgpu 24 `Cargo.toml:52`;
  `pollster` dev-dep `fp-render/Cargo.toml`.
- evilken worked example: `test-assets/evilken/evilken.cmd` (`QCF_a` `~D,DF,F,a`; `QCF2_x`
  `~D,DF,F,D,DF,F,x`; `dest1` `B,DB,D,DF,F,x+y`); `evilken.cns` super gate `power >= 3000`.

## References (external)

- wgpu offscreen capture / headless readback: gfx-rs wgpu "capture" example
  (https://deepwiki.com/gfx-rs/wgpu-native/5.4-capture-example); Learn Wgpu "Wgpu without a window"
  (https://sotrh.github.io/learn-wgpu/showcase/windowless/); WebGPU copying data / 256-byte row alignment
  (https://webgpufundamentals.org/webgpu/lessons/webgpu-copying-data.html).
- Fighting-game motion recognition (synthesize-frames-then-recognize is standard): CritPoints "How to
  Code Fighting Game Motion Inputs" (https://critpoints.net/2025/02/05/how-to-code-fighting-game-motion-inputs/);
  GameDevWithoutACause "QCF+Punch" (https://gamedevwithoutacause.com/?p=266).
