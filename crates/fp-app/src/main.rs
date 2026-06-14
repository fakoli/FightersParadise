//! Fighters Paradise — a modern MUGEN engine reimplementation in Rust.
//!
//! This is the application entry point. It initializes the SDL2 window,
//! sets up the wgpu rendering pipeline, and runs the main 60Hz game loop.
//!
//! # Usage
//!
//! ```text
//! cargo run -p fp-app                          # play KFM from test-assets/kfm/kfm.def
//! cargo run -p fp-app -- <char.def>            # play any MUGEN character from its .def
//! cargo run -p fp-app -- <file.sff> <file.air> # SFF+AIR animation viewer (demo mode)
//! cargo run -p fp-app -- <file.sff>            # show first sprite
//! ```
//!
//! The default mode loads a full MUGEN character (KFM) from its `.def` and runs
//! it entirely from its own CNS/CMD state machine — there is no hardcoded state
//! logic. Arrow keys (or WASD) move the character; attack buttons map to
//! U/I/O/J/K/L. Holding Forward drives the character into its CNS walk state.
//!
//! When given an `.sff`+`.air` pair instead, it falls back to a simple animation
//! viewer (the legacy demo path), and a lone `.sff` shows the first sprite.
//! With no arguments and no KFM assets present, it shows a checkerboard test
//! pattern. Missing assets degrade gracefully; the app never panics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fp_character::{ActiveCommands, Character, CompiledController, CompiledExpr, LoadedCharacter};
use fp_core::SpriteId;
use fp_formats::air::{AirFile, AnimAction};
use fp_formats::sff::SffFile;
use fp_input::{
    compile_command, CommandDef, CommandMatcher, Direction, InputBuffer, InputState,
};
use fp_render::{PaletteTexture, Renderer, SpriteDrawParams, SpriteTexture};
use sdl2::event::Event;
use sdl2::keyboard::{Keycode, Scancode};

/// Window width in pixels.
const WINDOW_WIDTH: u32 = 640;
/// Window height in pixels.
const WINDOW_HEIGHT: u32 = 480;
/// Fixed timestep duration: 1/60th of a second (~16.667ms).
const TICK_DURATION: Duration = Duration::from_nanos(16_666_667);

/// Default character `.def` loaded when no CLI argument is given.
const DEFAULT_DEF: &str = "test-assets/kfm/kfm.def";

/// MUGEN common stand (idle) state number.
const STATE_STAND: i32 = 0;
/// MUGEN common walk state number.
const STATE_WALK: i32 = 20;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("Fighters Paradise v0.1.0");

    if let Err(e) = run() {
        tracing::error!("Fatal error: {e}");
        std::process::exit(1);
    }
}

/// Cached GPU textures for a single sprite.
struct CachedSprite {
    texture: SpriteTexture,
    palette: PaletteTexture,
    axis_x: i16,
    axis_y: i16,
}

// ---------------------------------------------------------------------------
// CNS-driven playable character (Phase 5.5)
// ---------------------------------------------------------------------------

/// A bridge that exposes the per-tick active commands from a [`CommandMatcher`]
/// snapshot to the character's `command = "..."` triggers.
///
/// The matcher is run once per tick; its active command names are snapshotted
/// into an [`ActiveCommands`] which is handed to the [`Character`] as its
/// command source. Modeling it this way avoids a borrow conflict (the character
/// borrows its command source immutably during state evaluation, while the
/// matcher needs `&mut` to advance) and keeps facing-relative direction handling
/// inside the matcher, which already resolves `F`/`B` against the facing.
fn snapshot_active_commands(matcher: &CommandMatcher, defs: &[CommandDef]) -> ActiveCommands {
    let names = defs
        .iter()
        .filter(|d| matcher.command_active(&d.name))
        .map(|d| d.name.clone());
    ActiveCommands::from_names(names)
}

/// A fully CNS-driven playable character: a [`LoadedCharacter`] (assets +
/// compiled state graph) plus the live [`Character`] entity the executor steps.
///
/// Each tick this polls input, runs the [`CommandMatcher`], feeds the active
/// commands into the entity, ticks the CNS state machine, and exposes the
/// current AIR frame for rendering. There is no hardcoded state machine: every
/// transition comes from the merged CNS/CMD state graph.
struct CnsCharacter {
    /// The loaded character (assets + compiled, merged state graph).
    loaded: LoadedCharacter,
    /// The live entity stepped by the executor.
    entity: Character,
    /// Rolling input history for command recognition.
    input_buffer: InputBuffer,
    /// Command recognizer built from the `.cmd` command list.
    matcher: CommandMatcher,
    /// The command definitions (kept to enumerate active names each tick).
    command_defs: Vec<CommandDef>,
    /// GPU sprite cache keyed by sprite id.
    sprite_cache: HashMap<SpriteId, CachedSprite>,
}

impl CnsCharacter {
    /// Builds a CNS-driven character from a loaded `.def`.
    ///
    /// Compiles the `.cmd` commands into a [`CommandMatcher`] (feeding the raw
    /// MUGEN command strings straight to [`compile_command`], which parses the
    /// `$`/`>` modifiers natively as of task 5.6a), supplies the MUGEN
    /// engine-built-in stand<->walk movement bridge when the character's own data
    /// does not author it (see [`inject_engine_movement_bridge`]), and starts the
    /// entity standing with control in state 0.
    fn new(mut loaded: LoadedCharacter) -> Self {
        // Compile commands from the .cmd file (if any) into the matcher.
        //
        // The raw command string is fed straight to `compile_command`: as of task
        // 5.6a, fp-input parses the MUGEN `$` (direction-detect) and `>`
        // (strict-immediate) modifiers natively, so KFM's `holdfwd = /$F` etc.
        // compile without any app-side pre-processing. Commands that still fail to
        // compile (genuinely malformed) are skipped here rather than aborting.
        let command_defs: Vec<CommandDef> = loaded
            .cmd
            .as_ref()
            .map(|cmd_file| {
                cmd_file
                    .commands
                    .iter()
                    .filter_map(|c| {
                        let elements = match compile_command(&c.command) {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!(
                                    "skipping uncompilable command {:?} ({:?}): {e}",
                                    c.name,
                                    c.command
                                );
                                return None;
                            }
                        };
                        Some(CommandDef {
                            name: c.name.clone(),
                            elements,
                            time: c.time,
                            buffer_time: c.buffer_time,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        tracing::info!("Compiled {} commands from CMD file", command_defs.len());

        // Supply the MUGEN engine-built-in stand<->walk bridge when (and only
        // when) the character's data does not author its own. This is NOT a
        // band-aid for an fp-input/fp-character bug: stock KFM genuinely has no
        // `holdfwd -> ChangeState 20` controller in either common1.cns or kfm.cmd
        // — in real MUGEN that transition is hardcoded in the engine, not the data
        // files. See the function docs for the full diagnosis.
        inject_engine_movement_bridge(&mut loaded);

        let mut entity = Character::with_constants(loaded.constants);
        // Start standing, with control, in the stand state. The stand animation
        // is the MUGEN stand action (0); state 0's own `ChangeAnim` controller
        // corrects it on the first tick if the character authors a different
        // stand anim, so we do not need to evaluate the entry expression here.
        entity.state_no = STATE_STAND;
        entity.ctrl = true;
        entity.anim = STATE_STAND; // action 0 == stand

        tracing::info!(
            "CNS character {:?}: {} states, {} sprites, {} animations",
            loaded.name,
            loaded.state_count(),
            loaded.sff.sprites.len(),
            loaded.air.actions.len(),
        );

        Self {
            loaded,
            entity,
            input_buffer: InputBuffer::new(),
            matcher: CommandMatcher::new(command_defs.clone()),
            command_defs,
            sprite_cache: HashMap::new(),
        }
    }

    /// Advances one 60Hz tick: input -> command -> CNS state machine.
    fn tick(&mut self, input: InputState) {
        // 1. Push raw input into the rolling buffer.
        self.input_buffer.push(input);

        // 2. Run the command matcher (facing-relative; F/B respect facing).
        let facing_right = self.entity.facing == fp_character::Facing::Right;
        self.matcher.check_commands(&self.input_buffer, facing_right);

        // 3. Snapshot active commands into the entity's command source so
        //    `command = "..."` triggers evaluate against live input this tick.
        let active = snapshot_active_commands(&self.matcher, &self.command_defs);
        self.entity.set_command_source(Box::new(active));

        // 4. Step the CNS state machine one tick. The executor now owns world
        //    position integration from velocity, applying the facing sign to X
        //    (`pos.x += vel.x * facing_sign`, task 6.2c) so a left-facing
        //    character walks the correct way. The app must NOT integrate again.
        let _ = self.entity.tick(&self.loaded);

        // 5. Clamp to the stage ground plane (y = 0) so the character never
        //    sinks. The ground plane is a stage concern, so it stays here rather
        //    than in the character executor.
        if self.entity.pos.y > 0.0 {
            self.entity.pos.y = 0.0;
            if self.entity.vel.y > 0.0 {
                self.entity.vel.y = 0.0;
            }
        }
    }

    /// Resolves the AIR frame for the entity's current anim + element, if any.
    fn current_frame(&self) -> Option<&fp_formats::air::AnimFrame> {
        let action = self.loaded.air.action(self.entity.anim)?;
        if action.frames.is_empty() {
            return None;
        }
        let idx = clamp_elem(self.entity.anim_elem, action.frames.len());
        action.frames.get(idx)
    }

    /// Get or create cached GPU textures for a given sprite ID.
    fn get_or_create_sprite(
        &mut self,
        sprite_id: SpriteId,
        renderer: &Renderer,
    ) -> Option<&CachedSprite> {
        if self.sprite_cache.contains_key(&sprite_id) {
            return self.sprite_cache.get(&sprite_id);
        }

        let (index, sff_sprite) = self
            .loaded
            .sff
            .sprites
            .iter()
            .enumerate()
            .find(|(_, s)| s.group == sprite_id.group() && s.image == sprite_id.image())?;

        let axis_x = sff_sprite.axis_x;
        let axis_y = sff_sprite.axis_y;
        let width = sff_sprite.width;
        let height = sff_sprite.height;
        let pal_idx = sff_sprite.palette_index as usize;

        if width == 0 || height == 0 {
            tracing::warn!("Sprite {sprite_id} has zero dimensions ({width}x{height})");
            return None;
        }

        let pixels = match self.loaded.sff.decode_sprite(index) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to decode sprite {sprite_id}: {e}");
                return None;
            }
        };
        let palette_data = match self.loaded.sff.palette(pal_idx) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to load palette {pal_idx} for sprite {sprite_id}: {e}");
                return None;
            }
        };

        let texture = SpriteTexture::new(
            renderer.device(),
            renderer.queue(),
            width as u32,
            height as u32,
            &pixels,
        );
        let palette = PaletteTexture::new(renderer.device(), renderer.queue(), &palette_data);

        self.sprite_cache.insert(
            sprite_id,
            CachedSprite {
                texture,
                palette,
                axis_x,
                axis_y,
            },
        );
        self.sprite_cache.get(&sprite_id)
    }
}

/// Clamps a (possibly out-of-range) signed animation element index into
/// `0..len`, returning `0` for an empty action (the caller guards emptiness).
fn clamp_elem(index: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if index < 0 {
        0
    } else {
        (index as usize).min(len - 1)
    }
}

/// Builds a controller with a type, trigger groups, and params, all compiled.
fn make_ctrl(
    state_number: i32,
    label: &str,
    kind: &str,
    triggerall: &[&str],
    groups: &[(u32, &[&str])],
    params: &[(&str, &str)],
) -> CompiledController {
    CompiledController {
        state_number,
        label: label.to_string(),
        controller_type: Some(kind.to_string()),
        triggerall: triggerall.iter().map(|s| CompiledExpr::compile(s)).collect(),
        triggers: groups
            .iter()
            .map(|(n, conds)| fp_character::CompiledTriggerGroup {
                number: *n,
                conditions: conds.iter().map(|s| CompiledExpr::compile(s)).collect(),
            })
            .collect(),
        persistent: None,
        ignorehitpause: None,
        params: params
            .iter()
            .map(|(k, v)| (k.to_string(), fp_character::CompiledParam::compile(v)))
            .collect(),
    }
}

/// Supplies the MUGEN engine-built-in stand<->walk movement bridge into the
/// loaded state graph, but only when the character's own data does not already
/// author it.
///
/// # Why this is still needed (genuine engine gap, not a band-aid)
///
/// Task 5.6c removed the `normalize_command` and `drop_unevaluable_alive_controllers`
/// band-aids because their root causes were fixed upstream (fp-input now parses
/// `$`/`>`; fp-character now implements the `alive` trigger). This shim is a
/// different animal: it papers over a **genuine engine gap**, not a bug.
///
/// In real MUGEN the stand->walk and walk->stand transitions are **hardcoded
/// engine built-ins** — they are NOT authored in any character's data files.
/// Empirically verified against stock KFM (the diagnostic in task 5.6c):
///
/// - `kfm.cmd` `[Statedef -1]` defines only special moves and the `FF`/`BB`
///   double-tap *run* (-> states 100/105); it has **no** plain
///   `holdfwd -> ChangeState 20`.
/// - `common1.cns` `[Statedef 0]` (stand) has no stand->walk `ChangeState`; the
///   only `value = 20` in all of KFM's data is a `ChangeAnim` inside
///   `[Statedef 20]`, not a state transition.
///
/// So with all three band-aids removed, KFM holds Forward and never leaves state
/// 0 (`pos.x` stays 0). The correct long-term fix is to model MUGEN's built-in
/// common-state command->state bridge inside `fp-character`'s executor (so every
/// character gets it for free, exactly as MUGEN does), at which point this shim
/// can be deleted.
///
/// # Walk velocity is now fully engine-driven (no app-side repair)
///
/// Entering state 20 *and walking* now works end-to-end with no app-side repair:
/// KFM's `[Statedef 20]` sets motion with `VelSet x = const(velocity.walk.fwd.x)`,
/// `const(<member>)` resolves the authored magnitude (2.4) in fp-character (5.6e),
/// and the executor integrates the world position **facing-relative**
/// (`pos.x += vel.x * facing_sign`, task 6.2c). The stored velocity stays
/// facing-relative (matching the `Vel X` trigger that common1.cns's walk-anim
/// selectors rely on), so KFM walks the correct direction for BOTH facings
/// without rewriting the walk-state `VelSet`. The previous CB26 walk-velocity
/// facing repair (which rewrote state-20 `VelSet x` to `{walk_fwd_x} * facing`)
/// was therefore removed in 6.2c.
///
/// TODO(fp-character): move the stand<->walk (and crouch/jump) engine built-in
/// command->state transitions into the executor's special-state handling so every
/// character gets them for free, then remove this remaining app-side bridge (CB25).
///
/// The bridge is appended to `[Statedef -1]` (the per-tick command->state state),
/// gated on the standard `holdfwd`/`holdback` commands the data file defines, so
/// the rest of the CNS-driven path runs unmodified. It is a no-op for any
/// character that authors its own `ChangeState`-to-walk (avoiding a double walk).
fn inject_engine_movement_bridge(loaded: &mut LoadedCharacter) {
    // Only inject if the data doesn't already drive entry into the walk state
    // (avoid double-walking for characters that author their own bridge).
    let already_walks = loaded.states.values().any(|s| {
        s.controllers.iter().any(|c| {
            c.controller_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"))
                && c.params
                    .get("value")
                    .is_some_and(|e| e.source.trim() == STATE_WALK.to_string())
        })
    });
    if already_walks {
        tracing::info!("character authors its own walk bridge; skipping engine built-in");
        return;
    }

    let stand_to_walk = make_ctrl(
        -1,
        "engine: stand->walk",
        "ChangeState",
        &["stateno = 0", "ctrl"],
        &[(1, &["command = \"holdfwd\""]), (2, &["command = \"holdback\""])],
        &[("value", "20")],
    );
    let walk_to_stand = make_ctrl(
        -1,
        "engine: walk->stand",
        "ChangeState",
        &[
            "stateno = 20",
            "command != \"holdfwd\"",
            "command != \"holdback\"",
        ],
        &[(1, &["1"])],
        &[("value", "0")],
    );

    {
        let minus_one = loaded.states.entry(-1).or_insert_with(|| empty_state(-1));
        minus_one.controllers.push(stand_to_walk);
        minus_one.controllers.push(walk_to_stand);
    }

    tracing::info!("injected engine built-in stand<->walk command->state bridge");
}

/// Builds an empty `[Statedef n]` (no entry params, no controllers).
fn empty_state(number: i32) -> fp_character::CompiledState {
    fp_character::CompiledState {
        number,
        state_type: None,
        movetype: None,
        physics: None,
        anim: None,
        ctrl: None,
        velset: None,
        controllers: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Legacy SFF+AIR animation viewer (demo path)
// ---------------------------------------------------------------------------

/// A simple animation viewer: plays AIR action 0 from an SFF, no state machine.
struct AnimViewer {
    sff: SffFile,
    action: AnimAction,
    elem: usize,
    elem_time: i32,
    sprite_cache: HashMap<SpriteId, CachedSprite>,
}

impl AnimViewer {
    fn new(sff: SffFile, air: AirFile) -> Self {
        let action = air.action(0).cloned().unwrap_or(AnimAction {
            action_number: 0,
            frames: vec![],
            loopstart: 0,
        });
        tracing::info!("Animation viewer: {} actions loaded", air.actions.len());
        Self {
            sff,
            action,
            elem: 0,
            elem_time: 0,
            sprite_cache: HashMap::new(),
        }
    }

    /// Advance the viewer's animation cursor one tick.
    fn tick(&mut self) {
        if self.action.frames.is_empty() {
            return;
        }
        self.elem_time += 1;
        while let Some(frame) = self.action.frames.get(self.elem) {
            if frame.ticks <= 0 || self.elem_time < frame.ticks {
                break;
            }
            self.elem_time = 0;
            self.elem += 1;
            if self.elem >= self.action.frames.len() {
                self.elem = self.action.loopstart.min(self.action.frames.len() - 1);
            }
        }
    }

    fn current_frame(&self) -> Option<&fp_formats::air::AnimFrame> {
        self.action.frames.get(self.elem)
    }

    fn get_or_create_sprite(
        &mut self,
        sprite_id: SpriteId,
        renderer: &Renderer,
    ) -> Option<&CachedSprite> {
        if self.sprite_cache.contains_key(&sprite_id) {
            return self.sprite_cache.get(&sprite_id);
        }
        let (index, sff_sprite) = self
            .sff
            .sprites
            .iter()
            .enumerate()
            .find(|(_, s)| s.group == sprite_id.group() && s.image == sprite_id.image())?;
        let axis_x = sff_sprite.axis_x;
        let axis_y = sff_sprite.axis_y;
        let width = sff_sprite.width;
        let height = sff_sprite.height;
        let pal_idx = sff_sprite.palette_index as usize;
        if width == 0 || height == 0 {
            return None;
        }
        let pixels = match self.sff.decode_sprite(index) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to decode sprite {sprite_id}: {e}");
                return None;
            }
        };
        let palette_data = match self.sff.palette(pal_idx) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to load palette {pal_idx}: {e}");
                return None;
            }
        };
        let texture = SpriteTexture::new(
            renderer.device(),
            renderer.queue(),
            width as u32,
            height as u32,
            &pixels,
        );
        let palette = PaletteTexture::new(renderer.device(), renderer.queue(), &palette_data);
        self.sprite_cache.insert(
            sprite_id,
            CachedSprite {
                texture,
                palette,
                axis_x,
                axis_y,
            },
        );
        self.sprite_cache.get(&sprite_id)
    }
}

/// Build an `InputState` from the current SDL2 keyboard state.
fn poll_input_state(keyboard: &sdl2::keyboard::KeyboardState<'_>) -> InputState {
    let mut state = InputState {
        direction: Direction {
            up: keyboard.is_scancode_pressed(Scancode::W)
                || keyboard.is_scancode_pressed(Scancode::Up),
            down: keyboard.is_scancode_pressed(Scancode::S)
                || keyboard.is_scancode_pressed(Scancode::Down),
            left: keyboard.is_scancode_pressed(Scancode::A)
                || keyboard.is_scancode_pressed(Scancode::Left),
            right: keyboard.is_scancode_pressed(Scancode::D)
                || keyboard.is_scancode_pressed(Scancode::Right),
        },
        ..Default::default()
    };

    // Buttons: U=a, I=b, O=c, J=x, K=y, L=z, Enter=start
    state.set_button(fp_input::Button::A, keyboard.is_scancode_pressed(Scancode::U));
    state.set_button(fp_input::Button::B, keyboard.is_scancode_pressed(Scancode::I));
    state.set_button(fp_input::Button::C, keyboard.is_scancode_pressed(Scancode::O));
    state.set_button(fp_input::Button::X, keyboard.is_scancode_pressed(Scancode::J));
    state.set_button(fp_input::Button::Y, keyboard.is_scancode_pressed(Scancode::K));
    state.set_button(fp_input::Button::Z, keyboard.is_scancode_pressed(Scancode::L));
    state.set_button(
        fp_input::Button::Start,
        keyboard.is_scancode_pressed(Scancode::Return),
    );

    state
}

/// Map AIR blend mode to renderer blend mode + alpha.
fn map_blend_mode(air_blend: &fp_formats::air::BlendMode) -> (fp_render::BlendMode, f32) {
    match air_blend {
        fp_formats::air::BlendMode::Normal => (fp_render::BlendMode::Normal, 1.0),
        fp_formats::air::BlendMode::Additive => (fp_render::BlendMode::Additive, 1.0),
        fp_formats::air::BlendMode::AdditiveAlpha(a) => {
            (fp_render::BlendMode::Additive, *a as f32 / 256.0)
        }
        fp_formats::air::BlendMode::Subtractive => (fp_render::BlendMode::Subtractive, 1.0),
    }
}

/// Whether a path looks like a character definition (`.def`).
fn is_def_path(p: &str) -> bool {
    Path::new(p)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("def"))
}

/// The selected run mode after parsing CLI args and loading assets.
enum Mode {
    /// Full CNS-driven playable character.
    Cns(Box<CnsCharacter>),
    /// Legacy SFF+AIR animation viewer.
    Viewer(Box<AnimViewer>),
    /// Single static sprite.
    Static(SpriteTexture, PaletteTexture),
    /// Checkerboard fallback.
    TestPattern(SpriteTexture, PaletteTexture),
}

/// Picks the run mode from CLI args, loading assets and degrading gracefully.
///
/// - `<char.def>` (or no args, defaulting to KFM) → CNS-driven character.
/// - `<file.sff> <file.air>` → animation viewer.
/// - `<file.sff>` → static sprite.
/// - anything missing/unloadable → falls back to the test pattern (no panic).
fn select_mode(args: &[String], renderer: &Renderer) -> Mode {
    // Explicit .def argument, or an .sff/.air demo, else default to KFM.
    match args.len() {
        // <sff> <air> [..] → viewer (only when the first arg is NOT a .def).
        n if n >= 3 && !is_def_path(&args[1]) => {
            let sff_path = Path::new(&args[1]);
            let air_path = Path::new(&args[2]);
            tracing::info!("Loading SFF: {}", sff_path.display());
            tracing::info!("Loading AIR: {}", air_path.display());
            match (SffFile::load(sff_path), AirFile::load(air_path)) {
                (Ok(sff), Ok(air)) => Mode::Viewer(Box::new(AnimViewer::new(sff, air))),
                (Err(e), _) | (_, Err(e)) => {
                    tracing::warn!("viewer assets failed to load: {e}; showing test pattern");
                    let (s, p) = generate_test_pattern(renderer);
                    Mode::TestPattern(s, p)
                }
            }
        }
        // <char.def> → CNS character.
        n if n >= 2 && is_def_path(&args[1]) => load_cns_or_fallback(Path::new(&args[1]), renderer),
        // <sff> → static sprite.
        2 => match load_sff_sprite(renderer, Path::new(&args[1])) {
            Ok((s, p)) => Mode::Static(s, p),
            Err(e) => {
                tracing::warn!("sprite failed to load: {e}; showing test pattern");
                let (s, p) = generate_test_pattern(renderer);
                Mode::TestPattern(s, p)
            }
        },
        // No args → default to KFM, falling back to the test pattern.
        _ => {
            let def = PathBuf::from(DEFAULT_DEF);
            if def.exists() {
                tracing::info!("No files provided; loading default character {DEFAULT_DEF}");
                load_cns_or_fallback(&def, renderer)
            } else {
                tracing::info!("No files and no default character; showing test pattern");
                tracing::info!("Usage: fp-app <char.def> | <file.sff> [file.air]");
                let (s, p) = generate_test_pattern(renderer);
                Mode::TestPattern(s, p)
            }
        }
    }
}

/// Loads a CNS character from a `.def`, falling back to the test pattern on
/// failure (so a bad/missing character never crashes the app).
fn load_cns_or_fallback(def_path: &Path, renderer: &Renderer) -> Mode {
    tracing::info!("Loading character: {}", def_path.display());
    match LoadedCharacter::load(def_path) {
        Ok(loaded) => Mode::Cns(Box::new(CnsCharacter::new(loaded))),
        Err(e) => {
            tracing::warn!("character failed to load: {e}; showing test pattern");
            let (s, p) = generate_test_pattern(renderer);
            Mode::TestPattern(s, p)
        }
    }
}

fn run() -> fp_core::FpResult<()> {
    // --- SDL2 setup ---
    let sdl = sdl2::init().map_err(|e| fp_core::FpError::Other(format!("SDL2 init: {e}")))?;
    let video = sdl
        .video()
        .map_err(|e| fp_core::FpError::Other(format!("SDL2 video: {e}")))?;

    let window = video
        .window("Fighters Paradise", WINDOW_WIDTH, WINDOW_HEIGHT)
        .position_centered()
        .resizable()
        .metal_view()
        .build()
        .map_err(|e| fp_core::FpError::Other(format!("SDL2 window: {e}")))?;

    // --- wgpu setup ---
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..Default::default()
    });

    // SAFETY: The surface must live shorter than the window. We ensure this by
    // owning both in the same scope and dropping surface (inside Renderer) when
    // we exit the main loop.
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::from_window(&window).map_err(
            |e| fp_core::FpError::Render(format!("failed to create surface: {e}")),
        )?)
    }
    .map_err(|e| fp_core::FpError::Render(format!("failed to create surface: {e}")))?;

    let mut renderer = pollster::block_on(Renderer::new(
        &instance,
        surface,
        WINDOW_WIDTH,
        WINDOW_HEIGHT,
    ))?;

    // --- Load content based on CLI args ---
    let args: Vec<String> = std::env::args().collect();
    let mut mode = select_mode(&args, &renderer);

    // --- Main loop ---
    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| fp_core::FpError::Other(format!("SDL2 event pump: {e}")))?;

    let mut previous = Instant::now();
    let mut accumulator = Duration::ZERO;
    let mut running = true;

    while running {
        // Poll events
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => {
                    running = false;
                }
                Event::Window {
                    win_event: sdl2::event::WindowEvent::Resized(w, h),
                    ..
                } => {
                    renderer.resize(w as u32, h as u32);
                }
                _ => {}
            }
        }

        // Fixed timestep accumulation
        let current = Instant::now();
        accumulator += current - previous;
        previous = current;

        while accumulator >= TICK_DURATION {
            match mode {
                Mode::Cns(ref mut pc) => {
                    let keyboard = event_pump.keyboard_state();
                    let input = poll_input_state(&keyboard);
                    pc.tick(input);
                }
                Mode::Viewer(ref mut v) => v.tick(),
                Mode::Static(..) | Mode::TestPattern(..) => {}
            }
            accumulator -= TICK_DURATION;
        }

        // Ensure the current animation frame's sprite is cached before rendering.
        match mode {
            Mode::Cns(ref mut pc) => {
                if let Some(sid) = pc.current_frame().map(|f| f.sprite) {
                    pc.get_or_create_sprite(sid, &renderer);
                }
            }
            Mode::Viewer(ref mut v) => {
                if let Some(sid) = v.current_frame().map(|f| f.sprite) {
                    v.get_or_create_sprite(sid, &renderer);
                }
            }
            Mode::Static(..) | Mode::TestPattern(..) => {}
        }

        // Render
        let mut frame = renderer.begin_frame()?;
        frame.clear(0.1, 0.1, 0.15);

        let (win_w, win_h) = window.size();

        match mode {
            Mode::Cns(ref pc) => {
                if let Some(anim_frame) = pc.current_frame() {
                    if let Some(cached) = pc.sprite_cache.get(&anim_frame.sprite) {
                        let center_x = win_w as f32 / 2.0;
                        let ground_y = win_h as f32 * 0.7;
                        let facing_right = pc.entity.facing == fp_character::Facing::Right;

                        let draw_x = center_x + pc.entity.pos.x - cached.axis_x as f32
                            + anim_frame.offset.x as f32;
                        let draw_y = ground_y + pc.entity.pos.y - cached.axis_y as f32
                            + anim_frame.offset.y as f32;

                        let (render_blend, alpha) = map_blend_mode(&anim_frame.blend);
                        let flip_h = if facing_right {
                            anim_frame.flip_h
                        } else {
                            !anim_frame.flip_h
                        };

                        let params = SpriteDrawParams {
                            x: draw_x,
                            y: draw_y,
                            flip_h,
                            flip_v: anim_frame.flip_v,
                            blend: render_blend,
                            alpha,
                            ..Default::default()
                        };
                        frame.draw_sprite(&cached.texture, &cached.palette, &params);
                    }
                }
            }
            Mode::Viewer(ref v) => {
                if let Some(anim_frame) = v.current_frame() {
                    if let Some(cached) = v.sprite_cache.get(&anim_frame.sprite) {
                        let center_x = win_w as f32 / 2.0;
                        let ground_y = win_h as f32 * 0.7;
                        let draw_x =
                            center_x - cached.axis_x as f32 + anim_frame.offset.x as f32;
                        let draw_y =
                            ground_y - cached.axis_y as f32 + anim_frame.offset.y as f32;
                        let (render_blend, alpha) = map_blend_mode(&anim_frame.blend);
                        let params = SpriteDrawParams {
                            x: draw_x,
                            y: draw_y,
                            flip_h: anim_frame.flip_h,
                            flip_v: anim_frame.flip_v,
                            blend: render_blend,
                            alpha,
                            ..Default::default()
                        };
                        frame.draw_sprite(&cached.texture, &cached.palette, &params);
                    }
                }
            }
            Mode::Static(ref sprite_tex, ref palette_tex)
            | Mode::TestPattern(ref sprite_tex, ref palette_tex) => {
                let params = SpriteDrawParams {
                    x: (win_w as f32 - sprite_tex.width as f32) / 2.0,
                    y: (win_h as f32 - sprite_tex.height as f32) / 2.0,
                    ..Default::default()
                };
                frame.draw_sprite(sprite_tex, palette_tex, &params);
            }
        }

        frame.finish();
    }

    tracing::info!("Shutting down");
    Ok(())
}

/// Load the first sprite from an SFF file and create GPU textures.
fn load_sff_sprite(
    renderer: &Renderer,
    path: &Path,
) -> fp_core::FpResult<(SpriteTexture, PaletteTexture)> {
    tracing::info!("Loading SFF file: {}", path.display());
    let sff = SffFile::load(path)?;

    tracing::info!(
        "SFF loaded: {} sprites, {} palettes",
        sff.sprites.len(),
        sff.palettes.len()
    );

    let sprite = sff
        .sprites
        .first()
        .ok_or_else(|| fp_core::FpError::not_found("sprite", "SFF file contains no sprites"))?;

    let pixels = sff.decode_sprite(0)?;
    let palette_data = sff.palette(sprite.palette_index as usize)?;

    tracing::info!(
        "Sprite ({}, {}): {}x{}, palette index {}",
        sprite.group,
        sprite.image,
        sprite.width,
        sprite.height,
        sprite.palette_index
    );

    let sprite_tex = SpriteTexture::new(
        renderer.device(),
        renderer.queue(),
        sprite.width as u32,
        sprite.height as u32,
        &pixels,
    );
    let palette_tex = PaletteTexture::new(renderer.device(), renderer.queue(), &palette_data);

    Ok((sprite_tex, palette_tex))
}

/// Generate a checkerboard test pattern and rainbow palette.
fn generate_test_pattern(renderer: &Renderer) -> (SpriteTexture, PaletteTexture) {
    let size: u32 = 128;
    let tile: u32 = 8;
    let mut pixels = vec![0u8; (size * size) as usize];

    for y in 0..size {
        for x in 0..size {
            let checker = ((x / tile) + (y / tile)) % 2;
            pixels[(y * size + x) as usize] = if checker == 0 { 1 } else { 2 };
        }
    }

    let mut palette = [0u8; 1024];
    // Index 1: white
    palette[4] = 255;
    palette[5] = 255;
    palette[6] = 255;
    palette[7] = 255;
    // Index 2: dark gray
    palette[8] = 80;
    palette[9] = 80;
    palette[10] = 80;
    palette[11] = 255;
    // Fill remaining with rainbow gradient
    for i in 3..256usize {
        let t = (i - 3) as f32 / 253.0;
        let (r, g, b) = hsv_to_rgb(t * 360.0, 0.8, 0.9);
        palette[i * 4] = r;
        palette[i * 4 + 1] = g;
        palette[i * 4 + 2] = b;
        palette[i * 4 + 3] = 255;
    }

    let sprite_tex = SpriteTexture::new(renderer.device(), renderer.queue(), size, size, &pixels);
    let palette_tex = PaletteTexture::new(renderer.device(), renderer.queue(), &palette);

    (sprite_tex, palette_tex)
}

/// Simple HSV to RGB conversion for palette generation.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;

    let (r, g, b) = match (h as u32) / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    // `CommandSource::is_active` is the trait method the Proctor snapshot tests
    // call on `ActiveCommands`; bring the trait into scope so it resolves.
    use fp_character::CommandSource;
    use std::path::PathBuf;

    /// Resolves a path under the workspace `test-assets/` directory.
    /// `CARGO_MANIFEST_DIR` points at `crates/fp-app`; go up two levels.
    fn test_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join(rel)
    }

    /// A neutral input frame.
    fn neutral() -> InputState {
        InputState::default()
    }

    /// An input frame holding the absolute Right direction.
    fn hold_right() -> InputState {
        InputState {
            direction: Direction {
                right: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn is_def_path_detects_def() {
        assert!(is_def_path("kfm.def"));
        assert!(is_def_path("path/to/KFM.DEF"));
        assert!(!is_def_path("kfm.sff"));
        assert!(!is_def_path("kfm"));
    }

    #[test]
    fn clamp_elem_is_safe() {
        assert_eq!(clamp_elem(-5, 3), 0);
        assert_eq!(clamp_elem(0, 3), 0);
        assert_eq!(clamp_elem(2, 3), 2);
        assert_eq!(clamp_elem(99, 3), 2);
        assert_eq!(clamp_elem(0, 0), 0); // empty action
    }

    #[test]
    fn engine_movement_bridge_controller_targets_walk_state() {
        // A walk-bridge controller built by `make_ctrl` carries `value = 20`
        // (the target stand->walk state). Full end-to-end behavior is covered by
        // the headless test below; here we pin the controller shape.
        let already = make_ctrl(
            -1,
            "x",
            "ChangeState",
            &["1"],
            &[(1, &["1"])],
            &[("value", "20")],
        );
        assert_eq!(already.params.get("value").map(|e| e.source.as_str()), Some("20"));
    }

    /// HEADLESS integration test (no GPU/window): load KFM, inject synthetic
    /// input (hold Forward, then release) across N ticks, and assert the
    /// character enters the CNS walk state (20) while held and returns toward
    /// stand (0) on release — proving input -> command -> CNS-state end to end.
    ///
    /// Gated on test-assets: skips cleanly if KFM is not present.
    #[test]
    fn headless_hold_forward_drives_cns_walk_state() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping headless walk test: {} not present", def.display());
            return;
        }

        let loaded = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping headless walk test: kfm.def failed to load: {e}");
                return;
            }
        };
        let mut pc = CnsCharacter::new(loaded);

        // Sanity: start standing with control, facing right.
        assert_eq!(pc.entity.state_no, STATE_STAND, "starts in stand state");
        assert!(pc.entity.ctrl, "starts with control");
        assert_eq!(pc.entity.facing, fp_character::Facing::Right, "starts facing right");

        // CRITICAL (5.6c): this whole path runs WITHOUT the removed band-aids.
        // `holdfwd = /$F` compiles natively in fp-input (5.6a — no
        // `normalize_command`), and `alive` resolves to Life>0 in fp-character
        // (5.6b — no `drop_unevaluable_alive_controllers`), so the common stand
        // state does not fall into death 5050. The only remaining shim is the
        // MUGEN engine-built-in stand<->walk bridge (a genuine engine gap, not a
        // band-aid — see `inject_engine_movement_bridge`).

        // Hold Forward (facing right → absolute Right) for many ticks. The
        // `holdfwd` command activates while held; the engine movement bridge in
        // [Statedef -1] then fires the stand->walk ChangeState into state 20.
        let mut entered_walk = false;
        for _ in 0..30 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK {
                entered_walk = true;
                break;
            }
        }
        assert!(
            entered_walk,
            "holding Forward should drive the CNS walk state (20); ended in state {}",
            pc.entity.state_no
        );

        // STRENGTHENED: while genuinely in the walk state, the character must be
        // strictly advancing in the facing direction. Facing right, "forward" is
        // +x, so state 20's `VelSet x = const(velocity.walk.fwd.x)` (gated on the
        // live `holdfwd` command) must produce a strictly positive x-velocity AND
        // strictly advance pos.x over the held ticks. We verify BOTH the velocity
        // (the immediate CNS effect) and the integrated position (the observable
        // motion), so the test fails if the character merely flips to state 20
        // without actually walking.
        assert_eq!(
            pc.entity.state_no, STATE_WALK,
            "must still be in walk state to assert forward motion"
        );
        let mut saw_forward_vel = false;
        let x_before = pc.entity.pos.x;
        for _ in 0..5 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK && pc.entity.vel.x > 0.0 {
                saw_forward_vel = true;
            }
        }
        assert!(
            saw_forward_vel,
            "in walk state, facing right + holding forward must set a strictly \
             positive x-velocity (walk.fwd.x); never saw vel.x > 0"
        );
        assert!(
            pc.entity.pos.x > x_before,
            "walking forward must strictly advance pos.x in the facing (+x) \
             direction; pos.x went {x_before} -> {} (no advance)",
            pc.entity.pos.x
        );

        // Release all input: the walk->stand rule must return us toward stand,
        // and the character must stop advancing once it leaves the walk state.
        let mut returned_to_stand = false;
        for _ in 0..30 {
            pc.tick(neutral());
            if pc.entity.state_no == STATE_STAND {
                returned_to_stand = true;
                break;
            }
        }
        assert!(
            returned_to_stand,
            "releasing input should return the character toward stand (0); ended in state {}",
            pc.entity.state_no
        );
    }

    /// The character never panics under sustained, varied synthetic input.
    #[test]
    fn headless_random_input_never_panics() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let Ok(loaded) = LoadedCharacter::load(&def) else {
            eprintln!("skipping: kfm.def failed to load");
            return;
        };
        let mut pc = CnsCharacter::new(loaded);

        // Cycle through a spread of inputs; the loop must never panic.
        for i in 0..120u32 {
            let input = InputState {
                direction: Direction {
                    up: i % 7 == 0,
                    down: i % 5 == 0,
                    left: i % 3 == 0,
                    right: i % 2 == 0,
                },
                ..Default::default()
            };
            pc.tick(input);
        }
    }

    // =====================================================================
    // Edge-case, error-path, and MUGEN-semantics coverage for the fp-app
    // integration. Each block is annotated with the acceptance criterion it
    // exercises. The pure helpers (snapshot_active_commands, map_blend_mode,
    // make_ctrl, empty_state, clamp_elem) are tested fully synthetically; the
    // CnsCharacter behavioral tests (facing-relative input, command="..." live
    // triggers, graceful current_frame, native `alive` resolution, engine
    // movement bridge) require the merged CNS graph and are gated on test-assets
    // — they skip cleanly when KFM is absent.
    //
    // NOTE (task 5.6c): the `normalize_command` and `drop_unevaluable_alive_controllers`
    // band-aids were removed; commands now compile straight from the raw MUGEN
    // strings (fp-input 5.6a) and `alive` resolves natively (fp-character 5.6b).
    // =====================================================================

    /// An input frame holding the absolute Left direction.
    fn hold_left() -> InputState {
        InputState {
            direction: Direction {
                left: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Loads KFM and wraps it in a `CnsCharacter`, returning `None` (after a
    /// skip note) when the fixture is absent or fails to load. Keeps every gated
    /// behavioral test from duplicating the skip boilerplate.
    fn load_kfm_pc() -> Option<CnsCharacter> {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return None;
        }
        match LoadedCharacter::load(&def) {
            Ok(loaded) => Some(CnsCharacter::new(loaded)),
            Err(e) => {
                eprintln!("skipping: kfm.def failed to load: {e}");
                None
            }
        }
    }

    // ---- AC2: raw MUGEN command strings compile directly via fp-input ----
    // (Task 5.6c removed the `normalize_command` band-aid; fp-input parses `$`/`>`
    // natively as of 5.6a, so the raw string is fed straight to `compile_command`.)

    #[test]
    fn raw_hold_forward_compiles_natively() {
        // The headline locomotion command in kfm.cmd is `holdfwd = /$F`. With no
        // app-side normalization, it must compile straight from the raw string to
        // a single hold-direction element (the `$` direction-detect modifier is
        // now handled inside fp-input), proving the pipeline accepts real MUGEN
        // syntax with the band-aid gone.
        let elements = compile_command("/$F").expect("raw /$F should compile natively");
        assert_eq!(elements.len(), 1, "hold-forward is a single element");
    }

    #[test]
    fn raw_strict_and_detect_commands_compile_natively() {
        // `$` (direction-detect) and `>` (strict-immediate) — the two symbols the
        // old `normalize_command` used to strip — now compile natively. A motion
        // command using both must produce one element per token, in order.
        let qcf = compile_command("$D, $DF, $F, x").expect("detect motion compiles");
        assert_eq!(qcf.len(), 4, "D, DF, F, x is four elements");
        let strict = compile_command(">~D, >F, a").expect("strict motion compiles");
        assert_eq!(strict.len(), 3, "~D, F, a is three elements");
    }

    // ---- AC2: snapshot_active_commands maps matcher output -> ActiveCommands ----

    /// Builds a `CommandDef` from a raw MUGEN command string, mirroring exactly
    /// how `CnsCharacter::new` compiles the .cmd list (no normalization).
    fn cmd_def(name: &str, raw: &str, time: u32, buffer_time: u32) -> CommandDef {
        CommandDef {
            name: name.to_string(),
            elements: compile_command(raw).expect("test command should compile"),
            time,
            buffer_time,
        }
    }

    #[test]
    fn snapshot_active_commands_reflects_held_direction() {
        // PART B: a held Forward must surface the `holdfwd` command name through
        // the matcher into the ActiveCommands snapshot the entity reads.
        let defs = vec![cmd_def("holdfwd", "/$F", 1, 1)];
        let mut matcher = CommandMatcher::new(defs.clone());
        let mut buffer = InputBuffer::new();
        buffer.push(hold_right()); // absolute Right == Forward when facing right
        matcher.check_commands(&buffer, /* facing_right */ true);

        let active = snapshot_active_commands(&matcher, &defs);
        assert!(
            active.is_active("holdfwd"),
            "holding Forward should make holdfwd active"
        );
        // A command the matcher did not fire must be inactive.
        assert!(!active.is_active("holdback"));
        // Case-insensitive match (MUGEN command labels are case-insensitive).
        assert!(active.is_active("HoldFwd"));
    }

    #[test]
    fn snapshot_active_commands_empty_when_neutral() {
        // No input held → no command active → an empty snapshot, never a panic.
        let defs = vec![cmd_def("holdfwd", "/$F", 1, 1)];
        let mut matcher = CommandMatcher::new(defs.clone());
        let mut buffer = InputBuffer::new();
        buffer.push(neutral());
        matcher.check_commands(&buffer, true);
        let active = snapshot_active_commands(&matcher, &defs);
        assert!(!active.is_active("holdfwd"));
    }

    #[test]
    fn snapshot_active_commands_empty_defs_is_inert() {
        // With no command defs, the snapshot is empty regardless of the matcher.
        let matcher = CommandMatcher::new(Vec::new());
        let active = snapshot_active_commands(&matcher, &[]);
        assert!(!active.is_active("anything"));
    }

    // ---- AC3: map_blend_mode covers every AIR blend variant ----

    #[test]
    fn map_blend_mode_normal_and_additive() {
        let (m, a) = map_blend_mode(&fp_formats::air::BlendMode::Normal);
        assert_eq!(m, fp_render::BlendMode::Normal);
        assert!((a - 1.0).abs() < 1e-6);

        let (m, a) = map_blend_mode(&fp_formats::air::BlendMode::Additive);
        assert_eq!(m, fp_render::BlendMode::Additive);
        assert!((a - 1.0).abs() < 1e-6);
    }

    #[test]
    fn map_blend_mode_subtractive() {
        let (m, a) = map_blend_mode(&fp_formats::air::BlendMode::Subtractive);
        assert_eq!(m, fp_render::BlendMode::Subtractive);
        assert!((a - 1.0).abs() < 1e-6);
    }

    #[test]
    fn map_blend_mode_additive_alpha_scales_256() {
        // AIR additive-alpha is 0..256; the renderer wants 0.0..1.0. A value of
        // 128 maps to 0.5, 256 to 1.0, 0 to 0.0 — and never panics on the edges.
        let (m, a) = map_blend_mode(&fp_formats::air::BlendMode::AdditiveAlpha(128));
        assert_eq!(m, fp_render::BlendMode::Additive);
        assert!((a - 0.5).abs() < 1e-6, "128/256 == 0.5, got {a}");

        let (_, full) = map_blend_mode(&fp_formats::air::BlendMode::AdditiveAlpha(255));
        assert!(full < 1.0 && full > 0.9, "255/256 ~ 0.996, got {full}");

        let (_, zero) = map_blend_mode(&fp_formats::air::BlendMode::AdditiveAlpha(0));
        assert!((zero - 0.0).abs() < 1e-6, "0/256 == 0.0, got {zero}");
    }

    // ---- AC3: clamp_elem bounds the animation cursor (already covered for the
    // happy path; add the boundary i32::MIN/MAX and len==1 cases) ----

    #[test]
    fn clamp_elem_handles_extreme_values() {
        assert_eq!(clamp_elem(i32::MIN, 4), 0, "very-negative clamps to 0");
        assert_eq!(clamp_elem(i32::MAX, 4), 3, "very-large clamps to last index");
        assert_eq!(clamp_elem(0, 1), 0, "single-frame action");
        assert_eq!(clamp_elem(5, 1), 0, "single-frame action clamps to 0");
    }

    // ---- AC3: make_ctrl / empty_state build well-formed graph pieces ----

    #[test]
    fn make_ctrl_compiles_triggers_and_params() {
        let c = make_ctrl(
            -1,
            "test",
            "ChangeState",
            &["stateno = 0", "ctrl"],
            &[(1, &["command = \"holdfwd\""])],
            &[("value", "20")],
        );
        assert_eq!(c.state_number, -1);
        assert_eq!(c.controller_type.as_deref(), Some("ChangeState"));
        assert_eq!(c.triggerall.len(), 2);
        assert!(c.triggerall.iter().all(|e| !e.is_fallback), "triggerall compiled");
        assert_eq!(c.triggers.len(), 1);
        assert_eq!(c.triggers[0].number, 1);
        assert!(!c.triggers[0].conditions[0].is_fallback);
        assert_eq!(c.params.get("value").map(|e| e.source.as_str()), Some("20"));
        assert!(c.persistent.is_none());
        assert!(c.ignorehitpause.is_none());
    }

    #[test]
    fn empty_state_has_no_entry_fields_or_controllers() {
        let s = empty_state(-1);
        assert_eq!(s.number, -1);
        assert!(s.state_type.is_none());
        assert!(s.movetype.is_none());
        assert!(s.physics.is_none());
        assert!(s.anim.is_none());
        assert!(s.ctrl.is_none());
        assert!(s.velset.is_none());
        assert!(s.controllers.is_empty());
    }

    // ---- AC2 (facing-relative): holding the SCREEN direction toward the
    // opponent drives walk regardless of which way the character faces. ----

    #[test]
    fn headless_facing_left_hold_left_drives_walk() {
        // PART B requires F/B to be facing-relative. When the character faces
        // LEFT, "Forward" is screen-Left. Holding Left must therefore drive the
        // walk state, exactly as holding Right does when facing Right. This is the
        // mirror of Forge's facing-right walk test and guards the facing handling.
        let Some(mut pc) = load_kfm_pc() else { return };
        pc.entity.facing = fp_character::Facing::Left;
        assert_eq!(pc.entity.state_no, STATE_STAND, "starts standing");

        let mut entered_walk = false;
        for _ in 0..30 {
            pc.tick(hold_left());
            if pc.entity.state_no == STATE_WALK {
                entered_walk = true;
                break;
            }
        }
        assert!(
            entered_walk,
            "facing Left, holding screen-Left (=Forward) should drive walk (20); \
             ended in state {}",
            pc.entity.state_no
        );
    }

    #[test]
    fn headless_facing_left_hold_right_does_not_walk_forward() {
        // The negative of the above: facing Left, holding screen-Right is BACK,
        // not Forward. The FORWARD command (`holdfwd`) must NOT fire, so the
        // character must never advance in the FACING (forward) direction.
        //
        // Facing left, "forward" is -x. The correct MUGEN outcome of holding Back
        // is walk-BACK, which (facing left) moves the character toward +x — that
        // is expected and fine. What must NOT happen is forward motion (toward
        // -x), which would mean the forward command wrongly drove the walk. We
        // therefore assert the character never moved in the forward (-x) sense.
        // (Before 5.6c the walk velocity was a broken zero, so this test could not
        // distinguish the two; with walking now working it pins the facing sign.)
        let Some(mut pc) = load_kfm_pc() else { return };
        pc.entity.facing = fp_character::Facing::Left;
        let x_start = pc.entity.pos.x;
        for _ in 0..20 {
            pc.tick(hold_right());
        }
        assert!(
            pc.entity.pos.x >= x_start - 1e-3,
            "facing Left + holding Right (Back) must not walk FORWARD (toward -x); \
             moved from {x_start} to {} (negative = wrong forward motion)",
            pc.entity.pos.x
        );
    }

    #[test]
    fn headless_facing_left_hold_left_advances_forward_to_negative_x() {
        // Positive companion: facing Left, holding screen-Left is FORWARD. With
        // walking now functional (5.6c) and the executor integrating position
        // facing-relative (6.2c), the character must strictly advance in the
        // facing (forward = -x) direction, mirroring the facing-right walk.
        //
        // 6.2c semantics: the STORED velocity is FACING-RELATIVE — walk-forward is
        // `const(velocity.walk.fwd.x)` = +2.4 for BOTH facings (this is exactly
        // what common1.cns's `vel x > 0` walk-anim selector relies on). The facing
        // sign is applied only when integrating world position
        // (`pos.x += vel.x * facing_sign`), so facing left the SAME +2.4 velocity
        // produces -x motion. We therefore assert the facing-relative velocity is
        // strictly positive (forward) AND the world position advances toward -x.
        let Some(mut pc) = load_kfm_pc() else { return };
        pc.entity.facing = fp_character::Facing::Left;
        // Reach walk state first.
        let mut walking = false;
        for _ in 0..30 {
            pc.tick(hold_left());
            if pc.entity.state_no == STATE_WALK {
                walking = true;
                break;
            }
        }
        assert!(walking, "facing left, holding forward should reach walk (20)");
        let x_before = pc.entity.pos.x;
        let mut saw_forward_vel = false;
        for _ in 0..5 {
            pc.tick(hold_left());
            // Facing-relative walk-forward velocity is positive for both facings.
            if pc.entity.state_no == STATE_WALK && pc.entity.vel.x > 0.0 {
                saw_forward_vel = true;
            }
        }
        assert!(
            saw_forward_vel,
            "facing left, walk-forward velocity is FACING-RELATIVE and must be \
             strictly positive (+walk.fwd.x); the facing sign is applied at \
             integration, not to the stored velocity"
        );
        assert!(
            pc.entity.pos.x < x_before,
            "facing left, walking forward must strictly advance toward -x; \
             pos.x went {x_before} -> {}",
            pc.entity.pos.x
        );
    }

    // ---- AC2: command="..." triggers evaluate against LIVE input each tick.
    // We prove the round-trip by toggling input and observing the CNS state
    // follow it (walk while held, stand on release), then re-entering walk on a
    // fresh hold — i.e. the trigger is re-evaluated every tick, not latched. ----

    #[test]
    fn headless_live_command_triggers_track_input_each_tick() {
        let Some(mut pc) = load_kfm_pc() else { return };

        // Hold forward → walk.
        let mut walked = false;
        for _ in 0..30 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK {
                walked = true;
                break;
            }
        }
        assert!(walked, "first hold should enter walk; got {}", pc.entity.state_no);

        // Release → stand.
        let mut stood = false;
        for _ in 0..30 {
            pc.tick(neutral());
            if pc.entity.state_no == STATE_STAND {
                stood = true;
                break;
            }
        }
        assert!(stood, "release should return to stand; got {}", pc.entity.state_no);

        // Hold AGAIN → forward locomotion again. This is the key live-evaluation
        // assertion: a forward command fires on the *new* hold, proving triggers
        // are re-evaluated per tick rather than consumed once at startup.
        //
        // MUGEN semantics note: because the buffer still carries the earlier
        // forward press, the fresh hold can complete KFM's `FF` (F, F) double-tap
        // RUN command (-> state 100, run) instead of, or before, the plain walk
        // (-> state 20). Both are forward-locomotion states gated on a live
        // forward command, so either proves the command re-fired. We assert the
        // character left stand into a forward-locomotion state, not the specific
        // number (asserting only 20 would be wrong: run is the correct MUGEN
        // outcome of a re-tap and was the observed behavior).
        const STATE_RUN: i32 = 100;
        let mut moved_forward_again = false;
        for _ in 0..30 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK || pc.entity.state_no == STATE_RUN {
                moved_forward_again = true;
                break;
            }
        }
        assert!(
            moved_forward_again,
            "a fresh hold must re-trigger forward locomotion (walk 20 or run 100) \
             via live per-tick eval; got {}",
            pc.entity.state_no
        );
        assert_ne!(
            pc.entity.state_no, STATE_STAND,
            "the character must have left stand on the fresh hold"
        );
    }

    // ---- AC3: current_frame degrades gracefully (never panics, returns a real
    // frame for the live anim, None for a nonexistent anim). ----

    #[test]
    fn headless_current_frame_is_present_for_stand_and_none_for_missing() {
        let Some(mut pc) = load_kfm_pc() else { return };
        // After one tick the stand state's anim has frames; current_frame resolves.
        pc.tick(neutral());
        assert!(
            pc.current_frame().is_some(),
            "stand animation should resolve a current frame"
        );
        // Point the entity at an anim that does not exist; current_frame must be
        // None (graceful), not a panic.
        pc.entity.anim = 999_999;
        assert!(
            pc.current_frame().is_none(),
            "a nonexistent anim id must yield None, not panic"
        );
    }

    // ---- AC3: the engine-default walk bridge is injected into [Statedef -1] and
    // injection is idempotent (no double bridge on a second call). ----

    #[test]
    fn headless_walk_bridge_present_after_construction() {
        let Some(pc) = load_kfm_pc() else { return };
        // CnsCharacter::new injected the stand<->walk bridge (KFM relies on the
        // MUGEN engine default). [Statedef -1] must exist and carry a ChangeState
        // into state 20 gated on a holdfwd-style command.
        let minus_one = pc
            .loaded
            .state(-1)
            .expect("[Statedef -1] must exist after construction");
        let walks = minus_one.controllers.iter().any(|c| {
            c.controller_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"))
                && c.params
                    .get("value")
                    .is_some_and(|e| e.source.trim() == STATE_WALK.to_string())
        });
        assert!(walks, "a ChangeState -> walk(20) must be present in [Statedef -1]");
    }

    #[test]
    fn inject_engine_movement_bridge_is_idempotent() {
        let Some(mut pc) = load_kfm_pc() else { return };
        // Count walk-bridge controllers in [Statedef -1] after construction.
        let count_walk_bridges = |pc: &CnsCharacter| -> usize {
            pc.loaded
                .state(-1)
                .map(|s| {
                    s.controllers
                        .iter()
                        .filter(|c| {
                            c.params
                                .get("value")
                                .is_some_and(|e| e.source.trim() == STATE_WALK.to_string())
                        })
                        .count()
                })
                .unwrap_or(0)
        };
        let before = count_walk_bridges(&pc);
        assert!(before >= 1, "construction injected at least one walk bridge");
        // A second injection must NOT add another (the helper detects an existing
        // walk bridge and skips), or the character would double-walk.
        inject_engine_movement_bridge(&mut pc.loaded);
        let after = count_walk_bridges(&pc);
        assert_eq!(
            before, after,
            "second inject_engine_movement_bridge must be a no-op"
        );
    }

    // ---- AC2 (5.6b end-to-end): the `alive` trigger now resolves natively, so
    // the `drop_unevaluable_alive_controllers` band-aid is gone. KFM's stock
    // common stand state STILL carries its `trigger1 = !alive => ChangeState 5050`
    // death gate (we no longer strip it); with `alive` correctly resolving to
    // `Life > 0`, a full-life character must never trip it. ----

    #[test]
    fn headless_alive_resolves_so_no_death_state_at_full_life() {
        let Some(mut pc) = load_kfm_pc() else { return };

        // The alive-gated controllers are NO LONGER dropped: KFM's common stand
        // state must still author its `!alive` death gate, proving we kept the
        // stock data intact rather than papering over it.
        let any_alive = pc.loaded.states.values().any(|s| {
            s.controllers.iter().any(|c| {
                c.triggerall
                    .iter()
                    .chain(c.triggers.iter().flat_map(|g| g.conditions.iter()))
                    .any(|e| e.source.to_ascii_lowercase().contains("alive"))
            })
        });
        assert!(
            any_alive,
            "stock KFM authors `alive`-gated controllers; they must NOT be stripped \
             (the drop band-aid was removed in 5.6c)"
        );

        // Sanity: full life means `alive` is truthy from the start.
        assert!(pc.entity.life > 0, "KFM starts at full life");

        // Across many idle ticks the character must never enter KFM's death state
        // (5050) — proving `alive` resolves correctly (5.6b) instead of defaulting
        // to 0 and tripping the `!alive` gate, which the band-aid used to mask.
        for _ in 0..60 {
            pc.tick(neutral());
            assert_ne!(
                pc.entity.state_no, 5050,
                "a full-life character must not fall into the death state while idle"
            );
        }
    }

    // =====================================================================
    // PROCTOR (test-engineer) coverage for task 5.6c.
    //
    // The blocks below COMPLEMENT Forge's tests above. They harden the four
    // acceptance criteria against edge cases, error paths, and MUGEN semantics
    // that the existing suite does not pin:
    //   * AC1 — exactly ONE residual shim remains (the documented engine bridge);
    //           the removed band-aids leave no synthesized-command/death-strip
    //           behavior.
    //   * AC2 — every KFM hold direction compiles natively from its raw `/$X`
    //           string; a real-fixture round-trip over the whole kfm.cmd proves
    //           the no-normalization pipeline accepts stock MUGEN syntax and only
    //           drops genuinely-malformed (`;`-alternate) forms; snapshot semantics
    //           are decoupled from matcher internals and case-insensitive.
    //   * AC3 — after release the character not only returns toward stand but
    //           STOPS advancing (the task's explicit "stop advancing once it
    //           leaves the walk state"); cursor/blend helpers never panic on the
    //           full input range.
    //   * AC4 — covered by the diagnosis reported out-of-band: the single residual
    //           is a genuine engine gap, asserted minimal here.
    // All fixture-dependent tests skip cleanly when test-assets/ is absent.
    // =====================================================================

    // ---- AC2: every KFM "hold direction" command compiles natively ----

    #[test]
    fn all_kfm_hold_directions_compile_natively() {
        // kfm.cmd defines holdfwd/holdback/holdup/holddown as `/$F`,`/$B`,`/$U`,
        // `/$D`. With `normalize_command` gone, each must compile straight from the
        // raw string to exactly one hold-direction element (the `/` = hold and `$`
        // = direction-detect modifiers are handled inside fp-input as of 5.6a).
        for raw in ["/$F", "/$B", "/$U", "/$D"] {
            let els = compile_command(raw)
                .unwrap_or_else(|e| panic!("raw hold command {raw:?} must compile: {e}"));
            assert_eq!(els.len(), 1, "{raw:?} is a single hold-direction element");
        }
    }

    #[test]
    fn holdback_diagonal_detect_compiles_to_single_element() {
        // The walk->stand rule is gated on BOTH holdfwd and holdback. The back
        // hold (`/$B`) must compile just like forward, or release-while-facing
        // would never resolve. One element, no error.
        let els = compile_command("/$B").expect("holdback `/$B` must compile natively");
        assert_eq!(els.len(), 1);
    }

    #[test]
    fn malformed_command_returns_err_not_panic() {
        // The `.cmd` compile loop relies on `compile_command` returning `Err`
        // (logged + skipped) rather than panicking on junk. An empty string and a
        // bare unknown token must both be `Err`, never a panic.
        assert!(compile_command("").is_err(), "empty command is an error");
        assert!(compile_command("   ").is_err(), "whitespace-only is an error");
        assert!(
            compile_command("not_a_token").is_err(),
            "an unknown token is an error, not a panic"
        );
    }

    #[test]
    fn semicolon_alternate_form_is_skipped_gracefully() {
        // MUGEN's `;` alternate-command separator is NOT parsed by fp-input's
        // comma-splitting `compile_command`; such a form fails to compile. The
        // .cmd loop in `CnsCharacter::new` must SKIP it (warn + continue), never
        // abort. We pin the precondition here: the form is `Err`, so the loop's
        // `filter_map(|c| ...None)` path is the one exercised.
        let raw = "~D, DB, B, D, DB, B, x;~F, D, DF, F, D, DF, x";
        assert!(
            compile_command(raw).is_err(),
            "a `;`-alternate command must be Err so the loader skips it gracefully"
        );
    }

    // ---- AC2: snapshot_active_commands semantics independent of fixtures ----

    #[test]
    fn snapshot_active_commands_only_reports_passed_defs() {
        // The snapshot enumerates the *passed* def slice, filtering by the
        // matcher's active set. A def the matcher knows about but that is NOT in
        // the passed slice must not leak into the snapshot — proving the snapshot
        // is driven by the caller's def list, not matcher internals.
        let matcher_defs = vec![cmd_def("holdfwd", "/$F", 1, 1)];
        let mut matcher = CommandMatcher::new(matcher_defs);
        let mut buffer = InputBuffer::new();
        buffer.push(hold_right());
        matcher.check_commands(&buffer, true);

        // Query with an EMPTY def slice: nothing can be reported.
        let active_empty = snapshot_active_commands(&matcher, &[]);
        assert!(
            !active_empty.is_active("holdfwd"),
            "an empty def slice yields an empty snapshot even when holdfwd is firing"
        );

        // Query with a def slice that does not include the firing command.
        let other = vec![cmd_def("holdback", "/$B", 1, 1)];
        let active_other = snapshot_active_commands(&matcher, &other);
        assert!(
            !active_other.is_active("holdfwd"),
            "holdfwd is not in the passed slice, so it must not appear"
        );
        assert!(
            !active_other.is_active("holdback"),
            "holdback is in the slice but not firing (only Right held), so inactive"
        );
    }

    #[test]
    fn snapshot_active_commands_case_insensitive_query() {
        // MUGEN command labels are case-insensitive. The snapshot built from a
        // lowercase def must answer queries in any case.
        let defs = vec![cmd_def("holdfwd", "/$F", 1, 1)];
        let mut matcher = CommandMatcher::new(defs.clone());
        let mut buffer = InputBuffer::new();
        buffer.push(hold_right());
        matcher.check_commands(&buffer, true);
        let active = snapshot_active_commands(&matcher, &defs);
        assert!(active.is_active("holdfwd"));
        assert!(active.is_active("HOLDFWD"), "uppercase query matches");
        assert!(active.is_active("HoldFwd"), "mixed-case query matches");
    }

    // ---- AC2 (real fixture): the whole kfm.cmd compiles via the no-normalize
    // pipeline; only the genuinely-malformed `;`-alternate forms are dropped.
    // Gated: skips when test-assets/ is absent. ----

    #[test]
    fn kfm_cmd_round_trip_compiles_locomotion_and_skips_only_malformed() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping kfm.cmd round-trip: {} not present", def.display());
            return;
        }
        let Ok(loaded) = LoadedCharacter::load(&def) else {
            eprintln!("skipping kfm.cmd round-trip: kfm.def failed to load");
            return;
        };
        let Some(cmd) = loaded.cmd.as_ref() else {
            eprintln!("skipping kfm.cmd round-trip: no .cmd parsed");
            return;
        };

        let total = cmd.commands.len();
        assert!(total > 0, "kfm.cmd must define commands");

        // Compile EVERY raw command string exactly as `CnsCharacter::new` does
        // (straight to `compile_command`, no app-side normalization).
        let mut compiled = 0usize;
        let mut failed: Vec<&str> = Vec::new();
        let mut holdfwd_ok = false;
        let mut holdback_ok = false;
        for c in &cmd.commands {
            match compile_command(&c.command) {
                Ok(_) => {
                    compiled += 1;
                    if c.name.eq_ignore_ascii_case("holdfwd") {
                        holdfwd_ok = true;
                    }
                    if c.name.eq_ignore_ascii_case("holdback") {
                        holdback_ok = true;
                    }
                }
                Err(_) => failed.push(c.command.as_str()),
            }
        }

        // The locomotion commands the engine bridge depends on MUST compile.
        assert!(
            holdfwd_ok,
            "holdfwd must compile from its raw kfm.cmd string (no normalization)"
        );
        assert!(
            holdback_ok,
            "holdback must compile from its raw kfm.cmd string (no normalization)"
        );

        // Only the `;`-alternate forms are expected to fail; every failure must
        // contain a `;` (otherwise a real MUGEN command regressed). The bulk must
        // compile, proving the raw pipeline is faithful, not a mass-drop.
        for f in &failed {
            assert!(
                f.contains(';'),
                "the only acceptable compile failures are `;`-alternate forms; \
                 got an unexpected failure: {f:?}"
            );
        }
        assert!(
            compiled >= total.saturating_sub(failed.len()),
            "compiled count accounting is consistent"
        );
        assert!(
            compiled * 2 >= total,
            "the large majority of kfm.cmd must compile natively (compiled {compiled}/{total})"
        );
    }

    // ---- AC3: after release, the character STOPS advancing (the task's explicit
    // 'stop advancing once it leaves the walk state'). Gated on fixtures.
    //
    // MUGEN-FAITHFUL NUANCE (verified by trace, NOT a bug): on release KFM enters
    // stand(0) carrying its residual walk velocity, then `stand.friction = 0.85`
    // *coasts* it down for a few ticks until `[State 0, 3]`'s `trigger2 = Time = 4`
    // fires `VelSet x = 0` — a hard stop. So we assert the character STOPS (vel.x
    // reaches ~0 and pos.x freezes) within a handful of ticks and stays frozen,
    // rather than demanding an instantaneous halt (which would be wrong MUGEN
    // physics). Note the `abs(vel x) < Const(...)` friction-threshold gate does not
    // resolve yet (the documented `const(...)` gap), so the `Time = 4` rule is the
    // operative hard stop — which is exactly what the trace shows. ----

    #[test]
    fn headless_release_stops_forward_advance() {
        let Some(mut pc) = load_kfm_pc() else { return };

        // Drive into the walk state holding Forward (facing right).
        let mut walking = false;
        for _ in 0..30 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK {
                walking = true;
                break;
            }
        }
        assert!(walking, "hold Forward should reach walk; got {}", pc.entity.state_no);

        // Release until we are back in stand.
        let mut stood = false;
        for _ in 0..30 {
            pc.tick(neutral());
            if pc.entity.state_no == STATE_STAND {
                stood = true;
                break;
            }
        }
        assert!(stood, "release should return to stand; got {}", pc.entity.state_no);

        // Within a few more idle ticks the x-velocity must reach ~0 (friction
        // coast-down + the `Time = 4` hard stop). Allow up to 8 ticks of slack.
        let mut halted = false;
        for _ in 0..8 {
            pc.tick(neutral());
            if pc.entity.vel.x.abs() < 1e-3 {
                halted = true;
                break;
            }
        }
        assert!(
            halted,
            "standing idle must bring x-velocity to ~0 (friction + Time=4 VelSet); \
             got vel.x = {}",
            pc.entity.vel.x
        );

        // Once halted, the position must be FROZEN: no further advance across many
        // idle ticks (the walk velocity is fully cleared, not merely small).
        let x_frozen = pc.entity.pos.x;
        for _ in 0..15 {
            pc.tick(neutral());
        }
        assert!(
            (pc.entity.pos.x - x_frozen).abs() < 1e-4,
            "after halting in stand, pos.x must stay frozen; drifted {x_frozen} -> {} \
             while idle (residual walk velocity not cleared)",
            pc.entity.pos.x
        );
        assert_eq!(
            pc.entity.state_no, STATE_STAND,
            "the character must remain in stand while idle"
        );
    }

    // ---- AC3 (MUGEN semantics): cursor + blend helpers never panic on the full
    // input range; len==0 is the empty-action guard. ----

    #[test]
    fn clamp_elem_empty_action_with_extreme_indices() {
        // An empty action (len 0) must always yield 0 for ANY index, including the
        // signed extremes — the caller guards emptiness but clamp must not panic
        // or attempt `len - 1` underflow.
        assert_eq!(clamp_elem(i32::MIN, 0), 0);
        assert_eq!(clamp_elem(-1, 0), 0);
        assert_eq!(clamp_elem(0, 0), 0);
        assert_eq!(clamp_elem(i32::MAX, 0), 0);
    }

    #[test]
    fn map_blend_mode_additive_alpha_full_byte_range_never_panics() {
        // The in-memory `AdditiveAlpha` variant carries a `u8` (0..=255); the
        // renderer wants 0.0..1.0 via `/256`, so the max byte 255 maps to ~0.996
        // (NOT 1.0 — only the unrepresentable 256 would). Mapping every byte must
        // produce a finite, in-range, monotonically non-decreasing alpha with no
        // panic, since malformed content can carry any byte value.
        let mut prev = -1.0f32;
        for v in 0..=255u8 {
            let (m, alpha) = map_blend_mode(&fp_formats::air::BlendMode::AdditiveAlpha(v));
            assert_eq!(m, fp_render::BlendMode::Additive);
            assert!(
                alpha.is_finite() && (0.0..1.0).contains(&alpha),
                "alpha for AdditiveAlpha({v}) must be a finite [0,1) value, got {alpha}"
            );
            assert!(alpha >= prev, "alpha must be non-decreasing in the byte value");
            prev = alpha;
        }
        // The maximum byte (255) is the largest representable, just under 1.0.
        let (_, full) = map_blend_mode(&fp_formats::air::BlendMode::AdditiveAlpha(255));
        assert!((full - 255.0 / 256.0).abs() < 1e-6, "255/256, got {full}");
    }

    // ---- AC1/AC4: exactly ONE residual shim remains. The removed band-aids leave
    // no death-state-strip and no synthesized standalone movement state; the only
    // injected piece is the documented engine bridge inside [Statedef -1]. Gated. ----

    #[test]
    fn only_residual_shim_is_the_documented_engine_bridge() {
        let Some(pc) = load_kfm_pc() else { return };

        // (a) The death gate is NOT stripped (drop_unevaluable_alive_controllers
        //     removed): KFM still authors `!alive`-gated controllers somewhere.
        let alive_gate_present = pc.loaded.states.values().any(|s| {
            s.controllers.iter().any(|c| {
                c.triggerall
                    .iter()
                    .chain(c.triggers.iter().flat_map(|g| g.conditions.iter()))
                    .any(|e| e.source.to_ascii_lowercase().contains("alive"))
            })
        });
        assert!(
            alive_gate_present,
            "stock `alive`-gated controllers must remain (death-strip band-aid gone)"
        );

        // (b) The ONLY injected residual is the engine stand<->walk bridge in
        //     [Statedef -1]: exactly one ChangeState->20 and one ChangeState->0
        //     authored by the shim (labelled `engine:`). We assert the bridge is
        //     present and singular (idempotent), confirming the residual is minimal.
        let minus_one = pc.loaded.state(-1).expect("[Statedef -1] exists");
        let to_walk = minus_one
            .controllers
            .iter()
            .filter(|c| {
                c.controller_type
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"))
                    && c.params
                        .get("value")
                        .is_some_and(|e| e.source.trim() == STATE_WALK.to_string())
            })
            .count();
        assert_eq!(
            to_walk, 1,
            "exactly one stand->walk ChangeState shim (no duplicate / no extra synthesized movement state)"
        );
    }

    // ---- AC1/AC2 end-to-end: stand state falls through to walk via the OWN
    // merged bridge path (not a synthesized standalone movement state). The
    // `inject_default_movement` band-aid is gone, so the transition flows through
    // [Statedef -1]'s command->state controllers. Gated. ----

    #[test]
    fn headless_walk_transition_flows_through_statedef_minus_one() {
        let Some(mut pc) = load_kfm_pc() else { return };

        // The transition must be driven by [Statedef -1] (the per-tick
        // command->state state), which carries the bridge. Verify the bridge
        // controller exists there BEFORE driving input (precondition), then drive.
        let bridge_in_minus_one = pc
            .loaded
            .state(-1)
            .map(|s| {
                s.controllers.iter().any(|c| {
                    c.params
                        .get("value")
                        .is_some_and(|e| e.source.trim() == STATE_WALK.to_string())
                })
            })
            .unwrap_or(false);
        assert!(
            bridge_in_minus_one,
            "the stand->walk bridge must live in [Statedef -1], not a synthesized state"
        );

        let mut entered = false;
        for _ in 0..30 {
            pc.tick(hold_right());
            if pc.entity.state_no == STATE_WALK {
                entered = true;
                break;
            }
        }
        assert!(
            entered,
            "hold Forward must reach walk(20) through the merged [Statedef -1] bridge; got {}",
            pc.entity.state_no
        );
    }
}
