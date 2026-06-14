//! Fighters Paradise — a modern MUGEN engine reimplementation in Rust.
//!
//! This is the application entry point. It initializes the SDL2 window,
//! sets up the wgpu rendering pipeline, and runs the main 60Hz game loop.
//!
//! # Usage
//!
//! ```text
//! cargo run -p fp-app                          # two KFMs in a match (P1 keyboard, P2 idle)
//! cargo run -p fp-app -- <p1.def>              # P1 = that character, P2 = same character
//! cargo run -p fp-app -- <p1.def> <p2.def>     # P1 and P2 from two .def files
//! cargo run -p fp-app -- <file.sff> <file.air> # legacy SFF+AIR animation viewer (demo mode)
//! cargo run -p fp-app -- <file.sff>            # legacy single-sprite viewer
//! ```
//!
//! ## The two-player match (task 7.2 — the playable milestone)
//!
//! The default mode loads **two** full MUGEN characters (both KFM) from their
//! `.def`s, places them at opposing start positions facing each other, and drives
//! them through a [`fp_engine::Match`]. Basic ground locomotion is no longer
//! shimmed in the app: the loader supplies MUGEN's built-in
//! stand<->walk<->crouch<->jump command-states for every character (task 7.3
//! part B) and the `Match` runs each player's real [`fp_input::CommandMatcher`]
//! (task 7.3 part A). Every 60Hz frame the app gathers P1 input from the keyboard
//! and a neutral/idle input for P2, calls [`fp_engine::Match::tick`], then renders
//! BOTH fighters from their current AIR frame (per-character texture cache) plus a
//! minimal HUD: each fighter's life as a bar (P1 top-left, P2 top-right) and a
//! KO/round indicator.
//!
//! Controls (P1): arrow keys or WASD move; attack buttons map to U/I/O (a/b/c)
//! and J/K/L (x/y/z). P2 is a stationary dummy in this milestone.
//!
//! ## Legacy single-character / viewer modes
//!
//! Passing an `.sff`+`.air` pair falls back to the original animation viewer, and
//! a lone `.sff` shows the first sprite; these legacy demo paths are retained.
//! With no arguments and no KFM assets present, the app shows a checkerboard test
//! pattern. Missing or unloadable assets degrade gracefully to the test pattern
//! with a clear log message; the app never panics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fp_audio::{AudioSystem, Sound};
use fp_character::{Character, LoadedCharacter, SoundRequest};
use fp_core::SpriteId;
use fp_engine::{Match, MatchInput, Player, RoundState, StageBounds, Winner};
use fp_formats::air::{AirFile, AnimAction};
use fp_formats::sff::SffFile;
use fp_render::{PaletteTexture, Renderer, SpriteDrawParams, SpriteTexture};
use sdl2::event::Event;
use sdl2::keyboard::{Keycode, KeyboardState, Scancode};

// The single-character CNS driver (now `#[cfg(test)]`) and the legacy
// single-character tests are the only users of the command-recognition seam, so
// these imports are gated to keep the shipped binary free of unused imports.
#[cfg(test)]
use fp_character::ActiveCommands;
#[cfg(test)]
use fp_input::{compile_command, CommandDef, CommandMatcher, Direction, InputBuffer, InputState};

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
/// MUGEN common walk state number. Only referenced by the single-character CNS
/// regression tests now that the app no longer shims locomotion, so it is gated
/// to keep the shipped binary free of an unused constant.
#[cfg(test)]
const STATE_WALK: i32 = 20;

/// Player 1's starting world X (left of center), in pixels.
const P1_START_X: f32 = -60.0;
/// Player 2's starting world X (right of center), in pixels.
const P2_START_X: f32 = 60.0;
/// Horizontal half-extent of the playfield, in world pixels. The match clamps
/// both fighters inside `[-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH]`.
const STAGE_HALF_WIDTH: f32 = 220.0;
/// World pixels of horizontal travel mapped to one window pixel. The match
/// world is centered on the origin; this scales it into the window for display.
const WORLD_TO_SCREEN: f32 = 1.0;

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
// CNS-driven single playable character (Phase 5.5)
// ---------------------------------------------------------------------------
//
// The two-player [`Match`] path (Phase 7.2) is now the live runtime mode; this
// single-character CNS driver is retained only as the substrate for the
// extensive single-character regression tests below (command compilation, the
// engine movement bridge, facing-relative walk, live command triggers, etc.).
// It and its helpers are therefore `#[cfg(test)]`: kept for coverage, excluded
// from the shipped binary so it does not read as dead code.

/// A bridge that exposes the per-tick active commands from a [`CommandMatcher`]
/// snapshot to the character's `command = "..."` triggers.
///
/// The matcher is run once per tick; its active command names are snapshotted
/// into an [`ActiveCommands`] which is handed to the [`Character`] as its
/// command source. Modeling it this way avoids a borrow conflict (the character
/// borrows its command source immutably during state evaluation, while the
/// matcher needs `&mut` to advance) and keeps facing-relative direction handling
/// inside the matcher, which already resolves `F`/`B` against the facing.
#[cfg(test)]
fn snapshot_active_commands(matcher: &CommandMatcher, defs: &[CommandDef]) -> ActiveCommands {
    // The actual filter lives in `fp-input` (one place); this is a thin wrapper.
    ActiveCommands::from_names(matcher.active_command_names_in(defs))
}

/// A fully CNS-driven playable character: a [`LoadedCharacter`] (assets +
/// compiled state graph) plus the live [`Character`] entity the executor steps.
///
/// Each tick this polls input, runs the [`CommandMatcher`], feeds the active
/// commands into the entity, ticks the CNS state machine, and exposes the
/// current AIR frame for rendering. There is no hardcoded state machine: every
/// transition comes from the merged CNS/CMD state graph.
#[cfg(test)]
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
}

#[cfg(test)]
impl CnsCharacter {
    /// Builds a CNS-driven character from a loaded `.def`.
    ///
    /// Compiles the `.cmd` commands into a [`CommandMatcher`] via the shared
    /// [`LoadedCharacter::command_defs`] (which feeds the raw MUGEN command
    /// strings straight to `compile_command`, parsing the `$`/`>` modifiers
    /// natively), and starts the entity standing with control in state 0. The
    /// engine-built-in stand<->walk<->crouch<->jump locomotion is supplied by the
    /// loader for every character (task 7.3 part B), so there is no app-side shim
    /// here anymore.
    fn new(loaded: LoadedCharacter) -> Self {
        // Compile commands from the .cmd file (if any) into the matcher, using the
        // same shared compilation the two-player Match path uses.
        let command_defs: Vec<CommandDef> = loaded.command_defs();
        tracing::info!("Compiled {} commands from CMD file", command_defs.len());

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
        //    This single-character viewer/driver has no opponent, so it passes
        //    `None` + a default stage view: opponent-dependent triggers
        //    (`P2Dist`, `p2, ...`, …) read the safe default `0`.
        let _ = self
            .entity
            .tick(&self.loaded, None, fp_character::StageView::default());

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

// ---------------------------------------------------------------------------
// Two-player match (Phase 7.2) — the playable demo
// ---------------------------------------------------------------------------

/// Per-fighter GPU sprite cache for the two-player [`Match`] renderer.
///
/// A [`fp_engine::Match`] owns each fighter's [`LoadedCharacter`] (and thus its
/// [`SffFile`]); this holds only the lazily-decoded GPU textures, keyed by sprite
/// id. One [`FighterRender`] is kept per side (P1, P2) so the two characters'
/// textures never collide — the "per-character texture cache" the task requires.
#[derive(Default)]
struct FighterRender {
    /// GPU sprite cache keyed by sprite id, decoded on first use.
    sprite_cache: HashMap<SpriteId, CachedSprite>,
}

impl FighterRender {
    /// Ensures the GPU textures for `sprite_id` are cached, decoding from `sff`
    /// (the owning character's sprite file) on first use. Returns the cached
    /// entry, or `None` if the sprite is missing or fails to decode (logged,
    /// never a panic).
    fn get_or_create_sprite<'a>(
        &'a mut self,
        sff: &SffFile,
        sprite_id: SpriteId,
        renderer: &Renderer,
    ) -> Option<&'a CachedSprite> {
        if self.sprite_cache.contains_key(&sprite_id) {
            return self.sprite_cache.get(&sprite_id);
        }

        let (index, sff_sprite) = sff
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

        let pixels = match sff.decode_sprite(index) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to decode sprite {sprite_id}: {e}");
                return None;
            }
        };
        let palette_data = match sff.palette(pal_idx) {
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

/// A key into a fighter's decoded-sound cache: the SND file selector plus the
/// `group`/`sample` pair from a [`SoundRequest`].
///
/// `common` distinguishes a request that asked for the common/fight sound file
/// (`F`-prefixed `PlaySnd`) from one that asked for the character's own `.snd`.
/// The two could collide on the same `group`/`sample` once a separate common SND
/// is wired up, so the flag is part of the key today even though both currently
/// resolve against the character's own SND (see [`FighterAudio::play_requests`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SoundKey {
    /// `true` for a common/fight-file request, `false` for the character's own.
    common: bool,
    /// The sound group number.
    group: i32,
    /// The sound sample number within the group.
    sample: i32,
}

/// Per-fighter decoded-sound cache for the two-player [`Match`] audio layer.
///
/// A [`fp_engine::Match`] owns each fighter's [`LoadedCharacter`] (and thus its
/// optional [`fp_formats::snd::SndFile`]); this holds only the lazily-decoded
/// [`Sound`]s, keyed by [`SoundKey`]. A decode *failure* (or a missing SND/sound
/// entry) is cached as `None`, so a bad sound is looked up and logged once rather
/// than re-decoded every frame it is requested. One [`FighterAudio`] is kept per
/// side (P1, P2), mirroring the per-side [`FighterRender`] texture caches.
#[derive(Default)]
struct FighterAudio {
    /// Decoded sounds keyed by selector + group/sample; `None` caches a decode
    /// or lookup failure so it is not retried.
    sound_cache: HashMap<SoundKey, Option<Sound>>,
}

impl FighterAudio {
    /// Plays this fighter's [`SoundRequest`]s for the current frame through
    /// `audio`, decoding from `player`'s own `.snd` (lazily, via the cache).
    ///
    /// For each request: resolve the SND file (the character's own — see the
    /// common-fallback note below), look up/decode the `group`/`sample` WAV bytes
    /// into a [`Sound`] (caching the result, including failures, by
    /// [`SoundKey`]), then play it on the request's channel at a volume mapped
    /// from `volume_scale` (`/100`, clamped to `[0, 4]`). Missing SND, missing
    /// sound entry, or decode failure is skipped with a debug log — never a panic.
    ///
    /// `common` (an `F`-prefixed `PlaySnd`, the common/fight sound file) is not
    /// yet backed by a separate common SND: such requests fall back to the
    /// character's own SND with a debug log. `looping` is not yet supported by
    /// [`AudioSystem::play_sound`]; the field is read but ignored this milestone.
    fn play_requests(
        &mut self,
        audio: &mut AudioSystem,
        player: &Player,
        requests: &[SoundRequest],
    ) {
        for req in requests {
            let key = SoundKey {
                common: req.common,
                group: req.group,
                sample: req.sample,
            };
            // Emit the one-shot advisories only the first time a key is seen (the
            // decode below caches it), so a per-tick-repeating PlaySnd does not
            // flood the log at 60Hz.
            if !self.sound_cache.contains_key(&key) {
                if req.common {
                    tracing::debug!(
                        "PlaySnd common/fight sound (group {}, sample {}) has no dedicated \
                         common SND yet; falling back to the character's own .snd",
                        req.group,
                        req.sample,
                    );
                }
                if req.looping {
                    tracing::debug!(
                        "PlaySnd looping not yet supported (group {}, sample {}); playing once",
                        req.group,
                        req.sample,
                    );
                }
            }

            let sound = self.get_or_decode(player, key);
            if let Some(sound) = sound {
                let volume = (req.volume_scale as f32 / 100.0).clamp(0.0, 4.0);
                audio.play_sound(sound, req.channel, volume);
            }
        }
    }

    /// Returns the decoded [`Sound`] for `key`, decoding from `player`'s `.snd`
    /// on first use and caching the result (a failure is cached as `None` so it
    /// is not retried). Returns `None` when the SND, the sound entry, or the
    /// decode is missing/failed — each logged once at debug.
    fn get_or_decode(&mut self, player: &Player, key: SoundKey) -> Option<&Sound> {
        self.sound_cache
            .entry(key)
            .or_insert_with(|| Self::decode_sound(player, key))
            .as_ref()
    }

    /// Decodes one sound from `player`'s own `.snd`, returning `None` (with a
    /// debug log) when the SND is absent, the `group`/`sample` entry is missing,
    /// or the WAV bytes fail to decode. Both own- and common-file requests use
    /// the character's own SND today (no separate common SND is loaded yet).
    fn decode_sound(player: &Player, key: SoundKey) -> Option<Sound> {
        let Some(snd) = player.loaded.snd.as_ref() else {
            tracing::debug!(
                "no .snd for this fighter; skipping sound (group {}, sample {})",
                key.group,
                key.sample,
            );
            return None;
        };
        // `SndFile::sound` takes u32 selectors; a negative group/sample (which a
        // SoundRequest permits) has no entry, so reject it with an honest log
        // rather than wrapping to a huge u32 that misses with a misleading reason.
        let (Ok(group), Ok(sample)) = (u32::try_from(key.group), u32::try_from(key.sample)) else {
            tracing::debug!(
                "negative sound selector (group {}, sample {}) has no .snd entry; skipping",
                key.group,
                key.sample,
            );
            return None;
        };
        let Some(bytes) = snd.sound(group, sample) else {
            tracing::debug!(
                "sound entry (group {}, sample {}) not found in .snd; skipping",
                key.group,
                key.sample,
            );
            return None;
        };
        match Sound::decode(bytes) {
            Ok(sound) => Some(sound),
            Err(e) => {
                tracing::debug!(
                    "failed to decode sound (group {}, sample {}): {e}; skipping",
                    key.group,
                    key.sample,
                );
                None
            }
        }
    }
}

/// Resolves the AIR frame for a [`Player`]'s current anim + element, if any.
///
/// Reads the player's loaded animations and clamps the (possibly stale) element
/// cursor into range, driven off the engine-owned [`Player`] state. Returns
/// `None` for a missing/empty action.
fn player_current_frame(player: &Player) -> Option<&fp_formats::air::AnimFrame> {
    let action = player.loaded.air.action(player.anim())?;
    if action.frames.is_empty() {
        return None;
    }
    let idx = clamp_elem(player.anim_elem(), action.frames.len());
    action.frames.get(idx)
}

/// Builds a [`fp_engine::Match`] for two characters loaded from their `.def`
/// paths, applying the engine-built-in stand<->walk bridge to BOTH, seeding their
/// opposing start positions, and standing each in state 0 with control.
///
/// This is the single construction path shared by the live app and the headless
/// integration test, so the test exercises exactly the wiring the demo runs.
/// Returns an error only if a character truly cannot be loaded; the caller
/// degrades to the test pattern on `Err` rather than crashing.
fn build_two_player_match(p1_def: &Path, p2_def: &Path) -> fp_core::FpResult<Match> {
    let p1 = build_player(p1_def, P1_START_X)?;
    let p2 = build_player(p2_def, P2_START_X)?;
    // `Match::new` seeds facing toward each other from the start positions and
    // starts in the intro phase; the default 99-second round clock applies.
    Ok(Match::new(
        p1,
        p2,
        StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH),
    ))
}

/// Loads one character `.def` into a [`Player`] positioned at `start_x`,
/// standing it in state 0 with control.
///
/// No app-side movement shim is applied: the loader supplies MUGEN's built-in
/// stand<->walk<->crouch<->jump locomotion for every character (task 7.3 part B),
/// and the [`Match`] runs each player's real [`fp_input::CommandMatcher`]
/// (task 7.3 part A) so `holdfwd`/`holdback`/… and walk velocity all fire from
/// the character's own data.
fn build_player(def_path: &Path, start_x: f32) -> fp_core::FpResult<Player> {
    tracing::info!("Loading match character: {}", def_path.display());
    let loaded = LoadedCharacter::load(def_path)?;

    let mut entity = Character::with_constants(loaded.constants);
    entity.pos = fp_core::Vec2::new(start_x, 0.0);
    entity.state_no = STATE_STAND;
    entity.ctrl = true;
    entity.anim = STATE_STAND; // action 0 == stand

    tracing::info!(
        "Match player {:?}: {} states, {} sprites, {} animations, life {}",
        loaded.name,
        loaded.state_count(),
        loaded.sff.sprites.len(),
        loaded.air.actions.len(),
        entity.life,
    );

    Ok(Player::new(entity, loaded))
}

/// Builds a [`MatchInput`] (absolute screen directions + button presses) from the
/// current SDL2 keyboard state, using the same key map as the single-character
/// path (WASD/arrows + U/I/O/J/K/L). The engine converts these to facing-relative
/// commands internally, so this stays a pure absolute-direction snapshot.
fn match_input_from_keyboard(keyboard: &KeyboardState<'_>) -> MatchInput {
    MatchInput {
        up: keyboard.is_scancode_pressed(Scancode::W)
            || keyboard.is_scancode_pressed(Scancode::Up),
        down: keyboard.is_scancode_pressed(Scancode::S)
            || keyboard.is_scancode_pressed(Scancode::Down),
        left: keyboard.is_scancode_pressed(Scancode::A)
            || keyboard.is_scancode_pressed(Scancode::Left),
        right: keyboard.is_scancode_pressed(Scancode::D)
            || keyboard.is_scancode_pressed(Scancode::Right),
        a: keyboard.is_scancode_pressed(Scancode::U),
        b: keyboard.is_scancode_pressed(Scancode::I),
        c: keyboard.is_scancode_pressed(Scancode::O),
        x: keyboard.is_scancode_pressed(Scancode::J),
        y: keyboard.is_scancode_pressed(Scancode::K),
        z: keyboard.is_scancode_pressed(Scancode::L),
    }
}

/// A single solid HUD color: a 1x1 indexed-color texture plus a one-entry
/// palette, drawn scaled up to fill a rectangle. Recoloring per draw is not
/// possible (a 1x1 R8 texture carries no per-draw color and `fp-render` exposes
/// no texel update), so each HUD color is its own pre-built quad.
struct HudColor {
    /// 1x1 quad whose single texel is palette index 1.
    quad: SpriteTexture,
    /// One-color palette (index 1 = this color; index 0 = transparent).
    palette: PaletteTexture,
}

impl HudColor {
    /// Builds a 1x1 quad + palette for the given RGB color on the GPU.
    fn new(renderer: &Renderer, r: u8, g: u8, b: u8) -> Self {
        // Index 0 is transparent (discarded by the shader); the texel uses 1.
        let quad = SpriteTexture::new(renderer.device(), renderer.queue(), 1, 1, &[1]);
        let mut pal = [0u8; 1024];
        pal[4] = r;
        pal[5] = g;
        pal[6] = b;
        pal[7] = 255;
        let palette = PaletteTexture::new(renderer.device(), renderer.queue(), &pal);
        Self { quad, palette }
    }
}

/// A screen-space rectangle (top-left `x`/`y`, `w`idth, `h`eight) in pixels,
/// used to lay out HUD quads.
#[derive(Debug, Clone, Copy)]
struct HudRect {
    /// Left edge in window pixels.
    x: f32,
    /// Top edge in window pixels.
    y: f32,
    /// Width in window pixels.
    w: f32,
    /// Height in window pixels.
    h: f32,
}

/// A minimal HUD: per-fighter life bars, a smaller power (super-meter) bar under
/// each, and a round/KO indicator, drawn as scaled solid-color quads through the
/// existing `RenderFrame::draw_sprite` pipeline (no new renderer API). Full
/// lifebars are a later phase.
struct Hud {
    /// Neutral dark frame/background.
    dark: HudColor,
    /// Full-life green.
    green: HudColor,
    /// Low-life / P2-win red.
    red: HudColor,
    /// KO marker yellow.
    yellow: HudColor,
    /// Draw / generic white.
    white: HudColor,
    /// Power (super-meter) fill blue.
    blue: HudColor,
}

impl Hud {
    /// Builds all HUD color quads on the GPU.
    fn new(renderer: &Renderer) -> Self {
        Self {
            dark: HudColor::new(renderer, 40, 40, 48),
            green: HudColor::new(renderer, 60, 210, 90),
            red: HudColor::new(renderer, 220, 60, 60),
            yellow: HudColor::new(renderer, 240, 220, 60),
            white: HudColor::new(renderer, 240, 240, 240),
            blue: HudColor::new(renderer, 70, 150, 240),
        }
    }

    /// Draws a solid-color filled `rect` by drawing the given color's 1x1 quad
    /// scaled up.
    fn fill(&self, frame: &mut fp_render::RenderFrame<'_>, color: &HudColor, rect: HudRect) {
        let params = SpriteDrawParams {
            x: rect.x,
            y: rect.y,
            scale_x: rect.w.max(0.0),
            scale_y: rect.h.max(0.0),
            ..Default::default()
        };
        frame.draw_sprite(&color.quad, &color.palette, &params);
    }

    /// Draws the full match HUD: a life bar for each fighter (P1 top-left, P2
    /// top-right), a smaller power (super-meter) bar directly beneath each, and a
    /// round/KO indicator. Crude by design.
    fn draw(&self, frame: &mut fp_render::RenderFrame<'_>, win_w: f32, m: &Match) {
        const MARGIN: f32 = 12.0;
        const BAR_W: f32 = 200.0;
        const BAR_H: f32 = 16.0;
        /// The power bar is shorter than the life bar and a touch thinner, sitting
        /// just below it with a small vertical gap.
        const POWER_BAR_W: f32 = 160.0;
        const POWER_BAR_H: f32 = 8.0;
        const POWER_GAP: f32 = 4.0;
        let power_y = MARGIN + BAR_H + POWER_GAP;

        // P1 life bar, top-left, growing rightward.
        let p1_bar = HudRect { x: MARGIN, y: MARGIN, w: BAR_W, h: BAR_H };
        self.draw_life_bar(frame, p1_bar, m.p1(), false);
        // P1 power bar, directly beneath the life bar, also growing rightward.
        let p1_power = HudRect { x: MARGIN, y: power_y, w: POWER_BAR_W, h: POWER_BAR_H };
        self.draw_power_bar(frame, p1_power, m.p1(), false);
        // P2 life bar, top-right, draining toward the center (mirrored).
        let p2_bar = HudRect { x: win_w - MARGIN - BAR_W, y: MARGIN, w: BAR_W, h: BAR_H };
        self.draw_life_bar(frame, p2_bar, m.p2(), true);
        // P2 power bar, beneath the life bar, mirrored to anchor at the right edge.
        let p2_power = HudRect {
            x: win_w - MARGIN - POWER_BAR_W,
            y: power_y,
            w: POWER_BAR_W,
            h: POWER_BAR_H,
        };
        self.draw_power_bar(frame, p2_power, m.p2(), true);

        // Round / KO indicator: a centered colored marker once the round is
        // decided. Intro/Fight show nothing here (the bars carry the state).
        let marker = match (m.round_state(), m.winner()) {
            (RoundState::Ko, _) => Some(&self.yellow),
            (RoundState::Win, Some(Winner::P1)) => Some(&self.green),
            (RoundState::Win, Some(Winner::P2)) => Some(&self.red),
            (RoundState::Win, _) => Some(&self.white), // draw
            _ => None,
        };
        if let Some(color) = marker {
            let marker_w = 80.0;
            let marker_h = 24.0;
            let rect = HudRect {
                x: (win_w - marker_w) / 2.0,
                y: MARGIN,
                w: marker_w,
                h: marker_h,
            };
            self.fill(frame, color, rect);
        }
    }

    /// Draws one fighter's life bar: a dark backing the full `bar`, then a colored
    /// fill proportional to `life / life_max`. When `mirror` is set the fill is
    /// anchored to the right edge (so P2's bar drains toward the center).
    fn draw_life_bar(
        &self,
        frame: &mut fp_render::RenderFrame<'_>,
        bar: HudRect,
        player: &Player,
        mirror: bool,
    ) {
        // Backing.
        self.fill(frame, &self.dark, bar);

        let frac = life_fraction(player.life(), player.life_max());
        let fill_w = bar.w * frac;
        // Color shifts from green (healthy) to red (near death).
        let color = if frac > 0.33 { &self.green } else { &self.red };
        if fill_w > 0.0 {
            let fill_x = if mirror { bar.x + (bar.w - fill_w) } else { bar.x };
            self.fill(
                frame,
                color,
                HudRect {
                    x: fill_x,
                    w: fill_w,
                    ..bar
                },
            );
        }
    }

    /// Draws one fighter's power (super-meter) bar: a dark backing the full `bar`,
    /// then a blue fill proportional to `power / power_max`. When `mirror` is set
    /// the fill is anchored to the right edge (so P2's bar fills toward the
    /// center, matching its life bar above).
    fn draw_power_bar(
        &self,
        frame: &mut fp_render::RenderFrame<'_>,
        bar: HudRect,
        player: &Player,
        mirror: bool,
    ) {
        // Backing.
        self.fill(frame, &self.dark, bar);

        let frac = power_fraction(player.power(), player.power_max());
        let fill_w = bar.w * frac;
        if fill_w > 0.0 {
            let fill_x = if mirror { bar.x + (bar.w - fill_w) } else { bar.x };
            self.fill(
                frame,
                &self.blue,
                HudRect {
                    x: fill_x,
                    w: fill_w,
                    ..bar
                },
            );
        }
    }
}

/// The fraction of life remaining, clamped to `[0, 1]` and safe against a
/// non-positive `life_max` (returns `0.0` rather than dividing by zero).
fn life_fraction(life: i32, life_max: i32) -> f32 {
    if life_max <= 0 {
        return 0.0;
    }
    (life.max(0) as f32 / life_max as f32).clamp(0.0, 1.0)
}

/// The fraction of power (super meter) filled, clamped to `[0, 1]` and safe
/// against a non-positive `power_max` (returns `0.0` rather than dividing by
/// zero). Mirrors [`life_fraction`] so the power bar shares the same safety
/// guarantees as the life bar.
fn power_fraction(power: i32, power_max: i32) -> f32 {
    if power_max <= 0 {
        return 0.0;
    }
    (power.max(0) as f32 / power_max as f32).clamp(0.0, 1.0)
}

/// Maps a fighter's world X position into a screen X, centered on the window.
fn world_to_screen_x(world_x: f32, win_w: f32) -> f32 {
    win_w / 2.0 + world_x * WORLD_TO_SCREEN
}

/// Ensures the GPU texture for a fighter's current AIR frame is cached. Run once
/// per frame **before** `begin_frame`, because decoding needs `&Renderer` while a
/// live [`fp_render::RenderFrame`] holds the renderer borrowed. A missing frame
/// is a no-op.
fn cache_player_sprite(cache: &mut FighterRender, player: &Player, renderer: &Renderer) {
    if let Some(sprite_id) = player_current_frame(player).map(|f| f.sprite) {
        cache.get_or_create_sprite(&player.loaded.sff, sprite_id, renderer);
    }
}

/// Draws one fighter from its current AIR frame at its world position and facing,
/// reading the already-populated per-character texture cache (see
/// [`cache_player_sprite`]). A missing frame or uncached sprite is skipped.
fn draw_player(
    frame: &mut fp_render::RenderFrame<'_>,
    cache: &FighterRender,
    player: &Player,
    win_w: f32,
    win_h: f32,
) {
    let Some(anim_frame) = player_current_frame(player) else {
        return;
    };
    let Some(cached) = cache.sprite_cache.get(&anim_frame.sprite) else {
        return;
    };

    let ground_y = win_h * 0.8;
    let facing_right = player.facing() == fp_character::Facing::Right;
    let screen_x = world_to_screen_x(player.pos().x, win_w);

    let draw_x = screen_x - cached.axis_x as f32 + anim_frame.offset.x as f32;
    let draw_y = ground_y + player.pos().y - cached.axis_y as f32 + anim_frame.offset.y as f32;

    // Mirror the frame's authored flip when facing left (same rule as the single
    // character path).
    let flip_h = if facing_right {
        anim_frame.flip_h
    } else {
        !anim_frame.flip_h
    };

    let (render_blend, alpha) = map_blend_mode(&anim_frame.blend);
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

/// The two-player [`Match`] run state: the match plus the per-run rendering and
/// audio resources held alongside it (per-side texture caches, the shared audio
/// system, and per-side decoded-sound caches).
///
/// Bundled into one struct (rather than a wide enum tuple) so the audio layer
/// sits next to the renderer caches without making the `match` arms unwieldy.
struct MatchRun {
    /// The two-player match coordinator.
    m: Box<Match>,
    /// P1's per-character GPU sprite cache.
    p1_render: FighterRender,
    /// P2's per-character GPU sprite cache.
    p2_render: FighterRender,
    /// The shared audio mixer (silent [`fp_audio::NullBackend`] when no device
    /// exists).
    audio: AudioSystem,
    /// P1's per-character decoded-sound cache.
    p1_audio: FighterAudio,
    /// P2's per-character decoded-sound cache.
    p2_audio: FighterAudio,
}

/// The selected run mode after parsing CLI args and loading assets.
enum Mode {
    /// Two-player [`Match`] (the playable demo): match + per-run render/audio.
    Match(Box<MatchRun>),
    /// Legacy SFF+AIR animation viewer.
    Viewer(Box<AnimViewer>),
    /// Single static sprite.
    Static(SpriteTexture, PaletteTexture),
    /// Checkerboard fallback.
    TestPattern(SpriteTexture, PaletteTexture),
}

/// Picks the run mode from CLI args, loading assets and degrading gracefully.
///
/// - no args → two-player [`Match`] of two KFMs (default), if KFM assets exist.
/// - `<p1.def>` → two-player match, P1 and P2 both that character.
/// - `<p1.def> <p2.def>` → two-player match with the two given characters.
/// - `<file.sff> <file.air>` → legacy animation viewer.
/// - `<file.sff>` → legacy static sprite.
/// - anything missing/unloadable → falls back to the test pattern (no panic).
fn select_mode(args: &[String], renderer: &Renderer) -> Mode {
    match args.len() {
        // <p1.def> <p2.def> → two-player match from two characters.
        n if n >= 3 && is_def_path(&args[1]) && is_def_path(&args[2]) => {
            load_match_or_fallback(Path::new(&args[1]), Path::new(&args[2]), renderer)
        }
        // <sff> <air> [..] → legacy viewer (first arg is NOT a .def).
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
        // <p1.def> → two-player match, the same character on both sides.
        n if n >= 2 && is_def_path(&args[1]) => {
            let def = Path::new(&args[1]);
            load_match_or_fallback(def, def, renderer)
        }
        // <sff> → legacy static sprite.
        2 => match load_sff_sprite(renderer, Path::new(&args[1])) {
            Ok((s, p)) => Mode::Static(s, p),
            Err(e) => {
                tracing::warn!("sprite failed to load: {e}; showing test pattern");
                let (s, p) = generate_test_pattern(renderer);
                Mode::TestPattern(s, p)
            }
        },
        // No args → default to a two-KFM match, falling back to the test pattern.
        _ => {
            let def = PathBuf::from(DEFAULT_DEF);
            if def.exists() {
                tracing::info!("No files provided; loading two-KFM match from {DEFAULT_DEF}");
                load_match_or_fallback(&def, &def, renderer)
            } else {
                tracing::info!("No files and no default character; showing test pattern");
                tracing::info!("Usage: fp-app [p1.def [p2.def]] | <file.sff> [file.air]");
                let (s, p) = generate_test_pattern(renderer);
                Mode::TestPattern(s, p)
            }
        }
    }
}

/// Builds a two-player [`Match`] from two `.def` paths, falling back to the test
/// pattern on failure (so a bad/missing character never crashes the app).
fn load_match_or_fallback(p1_def: &Path, p2_def: &Path, renderer: &Renderer) -> Mode {
    match build_two_player_match(p1_def, p2_def) {
        Ok(m) => Mode::Match(Box::new(MatchRun {
            m: Box::new(m),
            p1_render: FighterRender::default(),
            p2_render: FighterRender::default(),
            // The default constructor opens a real device when present and falls
            // back to a silent NullBackend otherwise — it never panics, so the
            // app runs identically with or without audio hardware.
            audio: AudioSystem::default(),
            p1_audio: FighterAudio::default(),
            p2_audio: FighterAudio::default(),
        })),
        Err(e) => {
            tracing::warn!("match failed to load: {e}; showing test pattern");
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

    // The minimal match HUD (life bars + KO marker). Built once; only drawn in
    // the two-player match mode.
    let hud = Hud::new(&renderer);

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

        // Sample the keyboard ONCE per real frame, BEFORE the fixed-timestep
        // catch-up loop (audit #27). On a frame that has to run multiple ticks to
        // catch up, every sub-tick is driven by this single physical snapshot —
        // re-reading `keyboard_state()` inside the loop would replay the same live
        // state N times anyway, but doing it here makes the "one input per frame"
        // semantics explicit and keeps press-vs-hold edges and command timing
        // correct (one buffer push per frame). P2 stays an idle dummy this
        // milestone. The snapshot is cheap, so we take it unconditionally even in
        // non-Match modes (which ignore it).
        let p1_input = match_input_from_keyboard(&event_pump.keyboard_state());

        while accumulator >= TICK_DURATION {
            match mode {
                Mode::Match(ref mut run) => {
                    // P1 from this frame's single keyboard snapshot; P2 is an idle
                    // dummy this milestone.
                    run.m.tick(p1_input, MatchInput::none());

                    // AFTER the tick: play this frame's surfaced sound requests,
                    // P1 then P2, each from its own decoded-sound cache. Graceful
                    // throughout — a silent backend or a missing sound is a no-op.
                    run.p1_audio
                        .play_requests(&mut run.audio, run.m.p1(), run.m.p1_sound_requests());
                    run.p2_audio
                        .play_requests(&mut run.audio, run.m.p2(), run.m.p2_sound_requests());
                }
                Mode::Viewer(ref mut v) => v.tick(),
                Mode::Static(..) | Mode::TestPattern(..) => {}
            }
            accumulator -= TICK_DURATION;
        }

        // Ensure the current animation frame's sprite is cached before rendering.
        // Caching needs `&Renderer`, which a live `RenderFrame` would hold
        // borrowed, so it must happen before `begin_frame`.
        match mode {
            Mode::Match(ref mut run) => {
                cache_player_sprite(&mut run.p1_render, run.m.p1(), &renderer);
                cache_player_sprite(&mut run.p2_render, run.m.p2(), &renderer);
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
            Mode::Match(ref run) => {
                // Draw both fighters from their current AIR frame, each via its
                // own per-character texture cache (populated above).
                draw_player(&mut frame, &run.p1_render, run.m.p1(), win_w as f32, win_h as f32);
                draw_player(&mut frame, &run.p2_render, run.m.p2(), win_w as f32, win_h as f32);
                // Minimal HUD on top: life bars + KO/round indicator.
                hud.draw(&mut frame, win_w as f32, &run.m);
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


    // =====================================================================
    // Task 7.2 — two-player Match wiring (the playable demo)
    // =====================================================================

    /// AC4 (no division by zero): the life-bar fraction is always in `[0, 1]` and
    /// safe against a zero/negative `life_max` and overkill/over-full life.
    #[test]
    fn life_fraction_is_clamped_and_safe() {
        assert!((life_fraction(1000, 1000) - 1.0).abs() < 1e-6, "full life is 1.0");
        assert!((life_fraction(500, 1000) - 0.5).abs() < 1e-6, "half life is 0.5");
        assert!((life_fraction(0, 1000)).abs() < 1e-6, "no life is 0.0");
        assert_eq!(life_fraction(-50, 1000), 0.0, "overkill clamps to 0, not negative");
        assert_eq!(life_fraction(2000, 1000), 1.0, "over-full clamps to 1");
        assert_eq!(life_fraction(100, 0), 0.0, "zero life_max yields 0, no div-by-zero");
        assert_eq!(life_fraction(100, -10), 0.0, "negative life_max yields 0");
    }

    /// PR-C (audit #26): the power-bar fraction mirrors `life_fraction`'s safety —
    /// always in `[0, 1]`, clamped at both ends, and never divides by a
    /// zero/negative `power_max`.
    #[test]
    fn power_fraction_is_clamped_and_safe() {
        assert!((power_fraction(3000, 3000) - 1.0).abs() < 1e-6, "full meter is 1.0");
        assert!((power_fraction(1500, 3000) - 0.5).abs() < 1e-6, "half meter is 0.5");
        assert!((power_fraction(0, 3000)).abs() < 1e-6, "empty meter is 0.0");
        assert_eq!(power_fraction(-50, 3000), 0.0, "negative power clamps to 0");
        assert_eq!(power_fraction(9999, 3000), 1.0, "over-full meter clamps to 1");
        assert_eq!(power_fraction(100, 0), 0.0, "zero power_max yields 0, no div-by-zero");
        assert_eq!(power_fraction(100, -10), 0.0, "negative power_max yields 0");
    }

    /// AC2: world X maps into the window centered on the midpoint, with the origin
    /// landing at the window center and signs preserved.
    #[test]
    fn world_to_screen_x_centers_on_window() {
        let win_w = 640.0;
        assert!((world_to_screen_x(0.0, win_w) - 320.0).abs() < 1e-6, "origin at center");
        assert!(world_to_screen_x(-60.0, win_w) < 320.0, "negative world X is left of center");
        assert!(world_to_screen_x(60.0, win_w) > 320.0, "positive world X is right of center");
    }

    /// Builds the same two-KFM [`Match`] the app builds (default both KFM), or
    /// returns `None` (after a skip note) when the fixture is absent/unloadable.
    /// This is the single wiring the app's default mode uses, so the integration
    /// test below exercises exactly the runtime path.
    fn build_kfm_match() -> Option<Match> {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping two-KFM match test: {} not present", def.display());
            return None;
        }
        match build_two_player_match(&def, &def) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("skipping two-KFM match test: build failed: {e}");
                None
            }
        }
    }

    /// AC1: the app-level builder loads two characters, applies the stand<->walk
    /// bridge to BOTH, seeds opposing positions/facings, and starts in the intro
    /// phase with the default 99-second clock — the wiring `cargo run` uses.
    #[test]
    fn two_kfm_match_builds_with_opposing_positions_and_facings() {
        let Some(m) = build_kfm_match() else { return };

        // Opposing start positions (P1 left of center, P2 right).
        assert!(m.p1().pos().x < 0.0, "P1 starts left of center");
        assert!(m.p2().pos().x > 0.0, "P2 starts right of center");
        // Facing each other (left fighter faces right, right fighter faces left).
        assert_eq!(m.p1().facing(), fp_character::Facing::Right, "P1 faces the opponent");
        assert_eq!(m.p2().facing(), fp_character::Facing::Left, "P2 faces the opponent");
        // Both at full life, intro phase, 99-second clock, no winner yet.
        assert_eq!(m.p1().life(), m.p1().life_max());
        assert_eq!(m.p2().life(), m.p2().life_max());
        assert_eq!(m.round_state(), RoundState::Intro);
        assert_eq!(m.timer(), 99 * 60);
        assert_eq!(m.winner(), None);

        // BOTH characters must carry the engine stand<->walk bridge in [Statedef -1]
        // (the bridge is applied per-character by `build_player`).
        for player in [m.p1(), m.p2()] {
            let minus_one = player
                .loaded
                .state(-1)
                .expect("[Statedef -1] must exist after the bridge is applied");
            let walks = minus_one.controllers.iter().any(|c| {
                c.controller_type
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("ChangeState"))
                    && c.params
                        .get("value")
                        .is_some_and(|e| e.source.trim() == STATE_WALK.to_string())
            });
            assert!(walks, "each fighter must have the stand->walk bridge in [Statedef -1]");
        }
    }

    /// AC4 (never panics): the two-KFM match survives sustained, varied synthetic
    /// input on P1 (and an idle P2) without panicking, advancing the round.
    #[test]
    fn two_kfm_match_ticks_without_panic() {
        let Some(mut m) = build_kfm_match() else { return };
        for i in 0..240u32 {
            let p1 = MatchInput {
                left: i % 3 == 0,
                right: i % 2 == 0,
                up: i % 7 == 0,
                down: i % 5 == 0,
                a: i % 11 == 0,
                b: i % 13 == 0,
                ..MatchInput::none()
            };
            m.tick(p1, MatchInput::none());
        }
        // It reached at least the fight phase and never panicked.
        assert!(matches!(
            m.round_state(),
            RoundState::Fight | RoundState::Ko | RoundState::Win
        ));
    }

    /// Drives `m` until it leaves the intro and enters [`RoundState::Fight`],
    /// returning whether the fight became live within the budget.
    fn run_until_fight(m: &mut Match) -> bool {
        for _ in 0..120 {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() == RoundState::Fight {
                return true;
            }
        }
        m.round_state() == RoundState::Fight
    }

    /// AC3 (the headline headless integration test): build the same two-KFM Match
    /// the app builds, drive P1's keyboard inputs so a real KFM attack connects,
    /// and assert P2's life drops — proving the app-level two-player wiring works
    /// end to end with no window/GPU.
    ///
    /// This drives ENTIRELY through the public `MatchInput`/accessor seam (the
    /// `Match`'s internals are private to fp-engine), exactly as `cargo run` does:
    /// P1 walks into range with "toward opponent" held, then presses an attack
    /// button; KFM's own CNS produces the HitDef and the engine resolves the
    /// contact, dropping P2's life. The build path (`build_two_player_match`) is
    /// the same one the app's default mode uses.
    #[test]
    fn headless_two_player_attack_connects_and_drops_life() {
        let Some(mut m) = build_kfm_match() else { return };

        assert!(run_until_fight(&mut m), "fight must go live before driving input");
        let p2_life_before = m.p2().life();
        assert_eq!(p2_life_before, m.p2().life_max(), "P2 starts at full life");

        // Phase 1: P1 faces right (toward P2), so holding "right" is forward. Walk
        // into punching range; the player-push pins the two at touching distance.
        for _ in 0..240 {
            m.tick(
                MatchInput { right: true, ..MatchInput::none() },
                MatchInput::none(),
            );
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }

        // Phase 2: in range, throw light punches (the `x` button). Press on alternate
        // frames so the command recognizer sees fresh presses; over a generous budget
        // a punch must connect and reduce P2's life.
        let mut p2_was_hit = false;
        for i in 0..400 {
            let inp = if i % 3 == 0 {
                MatchInput { x: true, ..MatchInput::none() }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            if m.p2().life() < p2_life_before {
                p2_was_hit = true;
                break;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }

        assert!(
            p2_was_hit,
            "a P1 attack must connect and drop P2's life over the frame budget; \
             P2 life stayed at {} (round state {:?})",
            m.p2().life(),
            m.round_state()
        );
        assert!(
            m.p2().life() < p2_life_before,
            "P2 life must be strictly below its starting value after a connecting hit"
        );
    }

    /// 8.3b AC (headless, gated): build the same two-KFM `Match` the app builds,
    /// drive a PlaySnd-bearing action (KFM walks into range and throws light
    /// punches — its attack states author `PlaySnd`), and assert P1's surfaced
    /// `p1_sound_requests()` becomes non-empty at some tick — proving the request
    /// reaches the app's play path. Each frame the requests are also pumped
    /// through `FighterAudio::play_requests` over a silent `NullBackend`
    /// `AudioSystem` (the same call the live app makes), proving the decode/play
    /// path runs end to end without a device and never panics.
    ///
    /// We do not assert audio output (the `NullBackend` is silent and the
    /// `RecordingBackend` is `#[cfg(test)]`-private to fp-audio); the surfaced
    /// requests plus a panic-free play pump are the observable contract here.
    #[test]
    fn headless_two_player_attack_surfaces_and_plays_sound_requests() {
        let Some(mut m) = build_kfm_match() else { return };
        assert!(run_until_fight(&mut m), "fight must go live before driving input");

        // The exact audio layer the live app holds: a silent-fallback mixer plus
        // a per-fighter decoded-sound cache. NullBackend forces silence so the
        // test is deterministic and device-free.
        let mut audio = AudioSystem::with_backend(Box::new(fp_audio::NullBackend));
        let mut p1_audio = FighterAudio::default();
        let mut p2_audio = FighterAudio::default();

        // Pump P1+P2 requests through the play path for this frame.
        let pump = |m: &Match,
                    audio: &mut AudioSystem,
                    p1_audio: &mut FighterAudio,
                    p2_audio: &mut FighterAudio| {
            p1_audio.play_requests(audio, m.p1(), m.p1_sound_requests());
            p2_audio.play_requests(audio, m.p2(), m.p2_sound_requests());
        };

        // Phase 1: walk into punching range (holding "right" = forward for P1).
        for _ in 0..240 {
            m.tick(MatchInput { right: true, ..MatchInput::none() }, MatchInput::none());
            pump(&m, &mut audio, &mut p1_audio, &mut p2_audio);
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }

        // Phase 2: throw light punches; KFM's attack states fire PlaySnd, so the
        // surfaced P1 requests must become non-empty within the budget.
        let mut saw_request = false;
        for i in 0..400 {
            let inp = if i % 3 == 0 {
                MatchInput { x: true, ..MatchInput::none() }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            if !m.p1_sound_requests().is_empty() {
                saw_request = true;
            }
            // Pump every frame regardless, exercising the real play path.
            pump(&m, &mut audio, &mut p1_audio, &mut p2_audio);
            if saw_request {
                break;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }

        assert!(
            saw_request,
            "a PlaySnd-bearing KFM action must surface at least one P1 sound \
             request over the frame budget (round state {:?})",
            m.round_state()
        );
    }

    /// AC3 (round advances toward KO): land attacks until P2 is finished, then
    /// assert the round advances out of the fight (KO/Win) — proving the app-level
    /// wiring carries a connecting attack all the way to the round result. Damage
    /// must accrue regardless; the KO consequence is asserted once P2 is downed.
    #[test]
    fn headless_two_player_sustained_attacks_advance_round() {
        let Some(mut m) = build_kfm_match() else { return };
        if !run_until_fight(&mut m) {
            eprintln!("skipping: match never reached the fight phase");
            return;
        }

        let start = m.p2().life();
        // Close to range first.
        for _ in 0..240 {
            m.tick(
                MatchInput { right: true, ..MatchInput::none() },
                MatchInput::none(),
            );
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }
        // Hammer P2 with light punches for a long budget (KFM has ~1000 life).
        for i in 0..6000 {
            let inp = if i % 3 == 0 {
                MatchInput { x: true, ..MatchInput::none() }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }

        // P2 must have taken real damage from the app-level wiring.
        assert!(
            m.p2().life() < start,
            "sustained P1 attacks must reduce P2 life (was {start}, now {})",
            m.p2().life()
        );
        // If P2 was finished, the round must have advanced out of Fight with P1 up.
        if m.p2().life() <= 0 {
            assert!(
                matches!(m.round_state(), RoundState::Ko | RoundState::Win),
                "a KO must advance the round out of Fight, got {:?}",
                m.round_state()
            );
            if m.round_state() == RoundState::Win {
                assert_eq!(m.winner(), Some(Winner::P1), "P1 wins once P2 is KO'd");
            }
        }
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

        // CRITICAL: this whole path runs WITHOUT any app-side shim.
        // `holdfwd = /$F` compiles natively in fp-input, `alive` resolves to
        // Life>0 in fp-character (so the common stand state does not fall into
        // death 5050), and the MUGEN engine-built-in stand<->walk command-state is
        // supplied by the fp-character loader for every character (task 7.3 part B).

        // Hold Forward (facing right → absolute Right) for many ticks. The
        // `holdfwd` command activates while held; the loader's built-in
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

    // ---- AC3: the engine built-in stand->walk command-state is present in
    // [Statedef -1] after loading (now supplied by the fp-character loader for
    // every character, task 7.3 part B — not an app shim). ----

    #[test]
    fn headless_walk_bridge_present_after_construction() {
        let Some(pc) = load_kfm_pc() else { return };
        // The loader appended the engine built-in stand<->walk locomotion to
        // [Statedef -1]. It must carry a ChangeState into state 20 gated on a
        // holdfwd-style command.
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

    // ---- AC1/AC4: no app-side movement shim remains. The death-state-strip and
    // synthesized standalone movement state are gone; the engine built-in
    // stand->walk command-state now comes from the fp-character loader (task 7.3
    // part B), and is present exactly once in [Statedef -1]. Gated. ----

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

        // (b) The engine built-in stand->walk command-state appears EXACTLY once
        //     in [Statedef -1] (the loader appends it once; KFM's own `.cmd` `-1`
        //     authors no walk-entry of its own), confirming no duplicate / no extra
        //     synthesized movement state.
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
            "exactly one stand->walk ChangeState built-in (no duplicate / no extra synthesized movement state)"
        );
    }

    // ---- AC1/AC2 end-to-end: stand state falls through to walk via [Statedef -1]
    // (no synthesized standalone movement state). The transition flows through the
    // loader's built-in command->state controllers in [Statedef -1]. Gated. ----

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

    // =====================================================================
    // PROCTOR (test-engineer) coverage for task 7.2 — two-player Match wiring.
    //
    // These blocks COMPLEMENT Forge's 7.2 tests above. Forge's suite proves the
    // happy path (build the two-KFM match, attack connects, life drops, round
    // advances) and the helper purity (life_fraction, world_to_screen_x,
    // clamp_elem, map_blend_mode). The gaps Proctor closes here are:
    //
    //   * AC1 — an app-built two-KFM match WALKS P1 toward P2 when "toward the
    //           opponent" is held, end to end, with NO app shim: the `Match` runs
    //           KFM's real CommandMatcher (so `holdfwd` fires), the loader's
    //           built-in stand->walk command-state enters state 20, and KFM's own
    //           `[Statedef 20]` VelSet (gated on `command="holdfwd"`) moves it.
    //   * AC1 — `build_player` seeds the exact start X / stand state / control /
    //           full life the demo relies on (not just "left/right of center").
    //   * AC2 — `player_current_frame` (the engine-Player analog of the single-
    //           character `current_frame`) degrades to None for a missing anim and
    //           resolves a real frame for the live stand anim — the exact accessor
    //           the renderer calls per frame for BOTH fighters.
    //   * AC2 — the stage bounds the app constructs (`STAGE_HALF_WIDTH`) are wired
    //           into the Match and the two fighters start strictly inside them.
    //   * AC3 — the round advances toward Ko via the TIME-OVER branch too (Forge's
    //           tests only reach Ko via damage). A zero-second round built from
    //           app players reaches Ko/Win with the higher-life fighter winning,
    //           proving the app-built Player feeds the full round flow.
    //   * AC4 — degenerate inputs (both left+right, all buttons, P2 also active)
    //           never panic and the match still advances; CLI arg-routing
    //           precedence (.def vs .sff) is pinned via `is_def_path`.
    //
    // Every fixture-dependent test skips cleanly when test-assets/ is absent.
    // =====================================================================

    /// AC1 end-to-end (NO app shim): an app-built two-KFM match must actually WALK
    /// P1 toward P2 when "toward the opponent" is held — not merely flip to state
    /// 20 and stand still. This is the load-bearing proof of the whole 7.3 change:
    /// the `Match` runs KFM's real `CommandMatcher` (Part A) so `holdfwd` fires,
    /// the loader's built-in stand->walk command-state (Part B) enters state 20,
    /// and KFM's own `[Statedef 20]` VelSet (gated on `command="holdfwd"`) supplies
    /// the walk speed — with the two shims now deleted (Part C).
    #[test]
    fn app_built_match_walks_p1_toward_opponent() {
        let Some(mut m) = build_kfm_match() else { return };
        assert!(run_until_fight(&mut m), "fight must go live before driving input");

        // P1 faces right (toward P2); holding right is "fwd". The gap between the
        // two must shrink as P1 closes in (they start ~120px apart, well outside
        // the player-push touching distance).
        let gap_before = (m.p1().pos().x - m.p2().pos().x).abs();
        let p1_x_before = m.p1().pos().x;
        for _ in 0..60 {
            m.tick(MatchInput { right: true, ..MatchInput::none() }, MatchInput::none());
        }
        let gap_after = (m.p1().pos().x - m.p2().pos().x).abs();
        assert!(
            m.p1().pos().x > p1_x_before,
            "holding 'toward opponent' must advance P1's world X via the walk-velocity \
             bridge; pos.x went {p1_x_before} -> {} (no advance = bridge not wired)",
            m.p1().pos().x
        );
        assert!(
            gap_after < gap_before,
            "P1 walking toward P2 must close the gap ({gap_before} -> {gap_after})"
        );
    }

    // ---- AC1: build_player seeds the precise demo start state. ----

    #[test]
    fn build_player_seeds_position_state_control_and_full_life() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping build_player seed test: {} not present", def.display());
            return;
        }
        let player = match build_player(&def, P1_START_X) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping build_player seed test: {e}");
                return;
            }
        };
        // The exact start X the demo places P1 at (before Match::new's facing seed,
        // which does not move X).
        assert!((player.pos().x - P1_START_X).abs() < 1e-6, "P1 seeded at P1_START_X");
        assert!((player.pos().y).abs() < 1e-6, "seeded on the ground plane (y=0)");
        assert_eq!(player.character.state_no, STATE_STAND, "starts in the stand state");
        assert!(player.character.ctrl, "starts with control");
        assert_eq!(player.anim(), STATE_STAND, "starts on the stand animation (action 0)");
        assert!(player.life() > 0, "starts with positive life");
        assert_eq!(player.life(), player.life_max(), "starts at FULL life");
    }

    // ---- AC2: player_current_frame — the per-frame render accessor for BOTH
    // fighters — degrades gracefully and resolves the live frame. ----

    #[test]
    fn player_current_frame_resolves_stand_and_none_for_missing_anim() {
        let Some(mut m) = build_kfm_match() else { return };
        // After ticking through the intro into the fight, P1's stand anim has frames.
        assert!(run_until_fight(&mut m), "drive to the fight phase");
        assert!(
            player_current_frame(m.p1()).is_some(),
            "the stand animation must resolve a current frame for rendering"
        );
        assert!(
            player_current_frame(m.p2()).is_some(),
            "P2's animation must also resolve a frame (both fighters render)"
        );
        // Now point a player at a nonexistent anim and confirm None (graceful, no
        // panic). We reach into the public `character` field exactly as the engine
        // would. (`m` is borrowed immutably above, so build a fresh solo player to
        // mutate rather than reaching into the match.)
        let mut solo = match build_player(&test_asset("kfm/kfm.def"), P1_START_X) {
            Ok(p) => p,
            Err(_) => return,
        };
        solo.character.anim = 987_654;
        assert!(
            player_current_frame(&solo).is_none(),
            "a nonexistent anim id must yield None for the renderer, not panic"
        );
    }

    // ---- AC2: the app's stage bounds are wired into the Match and both fighters
    // start strictly inside them. ----

    #[test]
    fn app_stage_bounds_wired_and_fighters_start_inside() {
        let Some(m) = build_kfm_match() else { return };
        let bounds = m.bounds();
        assert!((bounds.left - -STAGE_HALF_WIDTH).abs() < 1e-6, "left bound = -STAGE_HALF_WIDTH");
        assert!((bounds.right - STAGE_HALF_WIDTH).abs() < 1e-6, "right bound = STAGE_HALF_WIDTH");
        // Both start strictly inside the playfield (their centers, at least).
        assert!(
            m.p1().pos().x > bounds.left && m.p1().pos().x < bounds.right,
            "P1 starts inside the stage"
        );
        assert!(
            m.p2().pos().x > bounds.left && m.p2().pos().x < bounds.right,
            "P2 starts inside the stage"
        );
    }

    // ---- AC3 (round advances toward Ko via the TIME-OVER branch). Forge's tests
    // reach Ko only via damage; this proves the app-built Player also carries the
    // round flow to a result when the clock expires. ----

    #[test]
    fn app_players_time_over_reaches_ko_and_win() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping time-over test: {} not present", def.display());
            return;
        }
        // Build two app-style players, but with a ZERO-second round so the fight
        // phase times out immediately. This drives the engine's time-over branch
        // (not the KO-by-damage branch Forge covers), using the exact `build_player`
        // construction the demo uses.
        let (p1, p2) = match (build_player(&def, P1_START_X), build_player(&def, P2_START_X)) {
            (Ok(a), Ok(b)) => (a, b),
            _ => {
                eprintln!("skipping time-over test: a player failed to build");
                return;
            }
        };
        let mut m = Match::with_round_seconds(
            p1,
            p2,
            StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH),
            0,
        );
        assert_eq!(m.round_state(), RoundState::Intro, "starts in intro");
        assert_eq!(m.timer(), 0, "a zero-second round starts with an empty clock");

        // Tick neutrally through intro + the KO hold into the decided round.
        let mut reached_win = false;
        for _ in 0..400 {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() == RoundState::Win {
                reached_win = true;
                break;
            }
        }
        assert!(
            reached_win,
            "a zero-second round must advance Intro -> Fight -> Ko -> Win; ended in {:?}",
            m.round_state()
        );
        // Both fighters idled at full, equal life, so time-over is a draw.
        assert_eq!(
            m.winner(),
            Some(Winner::Draw),
            "equal life at time over is a draw"
        );
    }

    /// AC3: the round visits the intermediate [`RoundState::Ko`] hold on its way to
    /// [`RoundState::Win`] (it does not jump straight to Win). Pins the lifecycle
    /// the HUD's KO/round indicator renders against.
    #[test]
    fn app_players_round_passes_through_ko_before_win() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping ko-before-win test: {} not present", def.display());
            return;
        }
        let (p1, p2) = match (build_player(&def, P1_START_X), build_player(&def, P2_START_X)) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return,
        };
        let mut m = Match::with_round_seconds(
            p1,
            p2,
            StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH),
            0,
        );
        let mut saw_ko = false;
        let mut saw_win = false;
        for _ in 0..400 {
            m.tick(MatchInput::none(), MatchInput::none());
            match m.round_state() {
                RoundState::Ko => saw_ko = true,
                RoundState::Win => {
                    saw_win = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_ko, "the round must hold in Ko before resolving to Win");
        assert!(saw_win, "the round must ultimately resolve to Win");
    }

    // ---- AC4 (never panics): degenerate / adversarial inputs on BOTH players. ----

    #[test]
    fn match_survives_conflicting_and_all_button_input_on_both_players() {
        let Some(mut m) = build_kfm_match() else { return };
        // Both directions held at once (no net horizontal), every button pressed,
        // on BOTH players simultaneously, for a long budget. Must never panic and
        // the round must still advance off the intro.
        let chaos = MatchInput {
            left: true,
            right: true,
            up: true,
            down: true,
            a: true,
            b: true,
            c: true,
            x: true,
            y: true,
            z: true,
        };
        for _ in 0..300 {
            m.tick(chaos, chaos);
        }
        assert!(
            matches!(
                m.round_state(),
                RoundState::Fight | RoundState::Ko | RoundState::Win
            ),
            "the match must leave the intro under sustained chaotic input on both sides"
        );
    }

    /// AC4 (life HUD invariant under combat): neither fighter's life ever exceeds
    /// its max nor goes more negative than the HUD can clamp, across a real fight
    /// — `life_fraction` already clamps, but this proves the engine never feeds it
    /// a NaN-inducing `life_max <= 0`.
    #[test]
    fn life_values_stay_hud_safe_through_a_fight() {
        let Some(mut m) = build_kfm_match() else { return };
        assert!(run_until_fight(&mut m), "drive to fight");
        for i in 0..600 {
            let inp = if i % 3 == 0 {
                MatchInput { right: true, x: true, ..MatchInput::none() }
            } else {
                MatchInput { right: true, ..MatchInput::none() }
            };
            m.tick(inp, MatchInput::none());
            for p in [m.p1(), m.p2()] {
                assert!(p.life_max() > 0, "life_max must stay positive for the HUD");
                assert!(p.life() <= p.life_max(), "life never exceeds its max");
                // The HUD fraction must always be a finite [0,1] value for any life.
                let f = life_fraction(p.life(), p.life_max());
                assert!(f.is_finite() && (0.0..=1.0).contains(&f), "HUD fraction in [0,1]");
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
    }

    // ---- AC1 (CLI arg routing): the .def-vs-.sff precedence `select_mode` relies
    // on. `select_mode` itself needs a GPU Renderer, but its routing decisions are
    // pure functions of `is_def_path` over the args — pin those decisions here. ----

    #[test]
    fn is_def_path_drives_match_vs_viewer_routing() {
        // Two .def args -> two-player match branch.
        assert!(is_def_path("p1.def") && is_def_path("p2.def"));
        // A .def first with a non-.def second still routes to the match (P1.def),
        // because select_mode checks `is_def_path(args[1])` for the single-def arm.
        assert!(is_def_path("kfm.DEF"));
        // SFF first -> legacy viewer/static branch (NOT a match).
        assert!(!is_def_path("char.sff"));
        assert!(!is_def_path("char.air"));
        // Extensionless / unusual names are not .def.
        assert!(!is_def_path("kfm"));
        assert!(!is_def_path("kfm.def.bak"));
        assert!(!is_def_path(".def"), "a bare dotfile named .def has no extension");
    }

    // ---- AC2 (HUD geometry): the P2 bar is mirrored to the right edge and the
    // world->screen mapping respects the WORLD_TO_SCREEN scale. ----

    #[test]
    fn world_to_screen_x_applies_scale_and_is_monotonic() {
        let win_w = 800.0;
        let center = win_w / 2.0;
        // Origin maps to the window center.
        assert!((world_to_screen_x(0.0, win_w) - center).abs() < 1e-6);
        // Monotonic and scale-respecting: +world is +scale*world from center.
        let a = world_to_screen_x(10.0, win_w);
        let b = world_to_screen_x(20.0, win_w);
        assert!(b > a, "screen X increases with world X");
        assert!(
            ((b - a) - (10.0 * WORLD_TO_SCREEN)).abs() < 1e-4,
            "the delta equals the world delta times WORLD_TO_SCREEN"
        );
        // Mirror symmetry about the center.
        let left = world_to_screen_x(-30.0, win_w);
        let right = world_to_screen_x(30.0, win_w);
        assert!((center - left - (right - center)).abs() < 1e-4, "symmetric about center");
    }
}
